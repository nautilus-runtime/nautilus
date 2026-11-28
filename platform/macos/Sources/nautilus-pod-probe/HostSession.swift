import Darwin
import Foundation

/// The VZ VM is owned by this process. An exclusive directory lock protects its
/// guest disks from another invocation until the owner exits or removes them.
final class HostSession: Sendable {
    let directory: URL
    private let lock: Int32

    init(in workParent: URL) throws {
        let root = workParent.appendingPathComponent(".nautilus-sessions", isDirectory: true)
        if mkdir(root.path, 0o700) != 0 && errno != EEXIST {
            throw HostSession.failure("create the session root")
        }
        let rootLock = try Self.openPrivateDirectory(root)
        defer { close(rootLock) }
        while flock(rootLock, LOCK_EX) != 0 {
            guard errno == EINTR else { throw Self.failure("lock the session root") }
        }
        let files = FileManager.default
        for entry in try files.contentsOfDirectory(at: root, includingPropertiesForKeys: nil) {
            let name = entry.lastPathComponent
            guard name.hasPrefix("session-"), let id = UUID(uuidString: String(name.dropFirst(8))),
                  name == "session-\(id.uuidString.lowercased())", let owner = try? Self.openPrivateDirectory(entry)
            else { continue }
            defer { close(owner) }
            if flock(owner, LOCK_EX | LOCK_NB) == 0 {
                try files.removeItem(at: entry)
            } else if errno != EWOULDBLOCK {
                throw Self.failure("check the session owner")
            }
        }

        let session = root.appendingPathComponent("session-\(UUID().uuidString.lowercased())", isDirectory: true)
        guard mkdir(session.path, 0o700) == 0 else { throw Self.failure("create a session") }
        let owner = try Self.openPrivateDirectory(session)
        guard flock(owner, LOCK_EX | LOCK_NB) == 0 else {
            let failure = Self.failure("lock the session")
            close(owner)
            throw failure
        }
        directory = session
        lock = owner
    }

    deinit { close(lock) }

    func remove() throws {
        try FileManager.default.removeItem(at: directory)
    }

    private static func openPrivateDirectory(_ directory: URL) throws -> Int32 {
        let fd = open(directory.path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw failure("open the session directory") }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_uid == geteuid(), info.st_mode & 0o777 == 0o700 else {
            close(fd)
            throw ProbeError("Session directory must be owned by the current user with mode 0700")
        }
        return fd
    }

    private static func failure(_ operation: String) -> ProbeError {
        ProbeError("Could not \(operation): \(String(cString: strerror(errno)))")
    }
}

func withHostSession<T>(in workParent: URL, _ operation: (URL) async throws -> T) async throws -> T {
    let session = try HostSession(in: workParent)
    let result: T
    do {
        result = try await operation(session.directory)
    } catch let failure as SandboxCleanupError {
        // The VM may still have its disks open. The next owner can collect them after this process exits.
        throw failure
    } catch {
        let failure = error
        do {
            try session.remove()
        } catch {
            throw ProbeError("\(failure); session cleanup failed: \(error)")
        }
        throw failure
    }
    try session.remove()
    return result
}
