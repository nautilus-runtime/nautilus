import Containerization
import Darwin
import Foundation

enum SandboxFiles {
    static let maximumFileBytes = 1 << 20

    static func prepareVolumes(in sandboxDirectory: URL, inputFile: URL?) throws -> [LinuxPod.PodVolume] {
        guard let inputFile else { return [workspaceVolume] }

        let input = try readInput(inputFile)
        let inputDirectory = sandboxDirectory.appendingPathComponent("approved-input", isDirectory: true)
        try FileManager.default.createDirectory(
            at: inputDirectory,
            withIntermediateDirectories: false,
            attributes: [.posixPermissions: 0o755]
        )
        // Share only the copied file. The enclosing sandbox directory stays private to the host.
        try writeNewFile(input, to: inputDirectory.appendingPathComponent("file"), mode: 0o444)
        let share = LinuxPod.PodVolume(
            name: "input",
            source: .virtiofs(path: inputDirectory, readOnly: true),
            format: "virtiofs"
        )
        return [workspaceVolume, share]
    }

    static func containerMounts(hasInput: Bool) -> [Mount] {
        var mounts: [Mount] = [workspaceMount]
        if hasInput {
            mounts.append(.sharedMount(name: "input", destination: "/input", options: ["ro"]))
        }
        return mounts
    }

    static func export(_ bytes: Data, to destination: URL) throws {
        guard bytes.count <= maximumFileBytes else { throw Failure.fileTooLarge(destination.path) }
        try writeNewFile(bytes, to: destination, mode: 0o600)
    }

    private static func readInput(_ path: URL) throws -> Data {
        guard path.isFileURL else { throw Failure.regularFileRequired(path.absoluteString) }
        let descriptor = Darwin.open(path.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
        guard descriptor >= 0 else { throw posixError(path) }
        let file = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? file.close() }

        var metadata = stat()
        guard fstat(descriptor, &metadata) == 0 else { throw posixError(path) }
        guard metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else {
            throw Failure.regularFileRequired(path.path)
        }
        guard metadata.st_size <= maximumFileBytes else { throw Failure.fileTooLarge(path.path) }
        let bytes = try file.read(upToCount: maximumFileBytes + 1) ?? Data()
        guard bytes.count <= maximumFileBytes else { throw Failure.fileTooLarge(path.path) }
        return bytes
    }

    private static func writeNewFile(_ bytes: Data, to path: URL, mode: mode_t) throws {
        guard path.isFileURL else { throw Failure.regularFileRequired(path.absoluteString) }
        let descriptor = Darwin.open(path.path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, mode)
        guard descriptor >= 0 else { throw posixError(path) }
        let file = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        do {
            try file.write(contentsOf: bytes)
            try file.close()
        } catch {
            try? file.close()
            try? FileManager.default.removeItem(at: path)
            throw error
        }
    }

    private static func posixError(_ path: URL) -> NSError {
        let code = errno
        return NSError(domain: NSPOSIXErrorDomain, code: Int(code), userInfo: [NSFilePathErrorKey: path.path])
    }

    private enum Failure: LocalizedError {
        case regularFileRequired(String)
        case fileTooLarge(String)

        var errorDescription: String? {
            switch self {
            case .regularFileRequired(let path): "Expected a regular file at \(path)"
            case .fileTooLarge(let path): "File exceeds the \(SandboxFiles.maximumFileBytes)-byte limit at \(path)"
            }
        }
    }
}
