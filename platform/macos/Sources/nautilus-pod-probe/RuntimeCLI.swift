import Containerization
import Darwin
import Foundation

struct PodManifest: Decodable {
    let containers: [SandboxContainer]
    let foreground: String
}

struct ConsoleWriter: Writer {
    let output: FileHandle

    func write(_ data: Data) throws {
        try output.write(contentsOf: data)
    }

    func close() throws {}
}

let workspaceVolume = LinuxPod.PodVolume(name: "workspace", source: .tmpfs(sizeBytes: 16 << 20), format: "tmpfs")
let workspaceMount = Mount.sharedMount(name: "workspace", destination: "/workspace", options: ["bind", "nosuid", "nodev"])

func withSandbox<T>(_ sandbox: PodSandbox, _ operation: () async throws -> T) async throws -> T {
    let result: T
    do {
        result = try await operation()
    } catch {
        let failure = error
        do {
            try await sandbox.remove()
        } catch {
            throw ProbeError("\(failure); sandbox \(sandbox.id) cleanup failed: \(error)")
        }
        throw failure
    }
    try await sandbox.remove()
    return result
}

func runWorkload(_ args: [String]) async throws -> Int32 {
    guard args.count >= 4 else {
        throw ProbeError("Usage: nautilus-pod-probe run ASSETS WORK_PARENT IMAGE [-- PROGRAM ARG...]\n       nautilus-pod-probe pod ASSETS WORK_PARENT MANIFEST.json")
    }
    let manifest: PodManifest
    switch args[0] {
    case "run":
        guard args.count == 4 || (args.count >= 6 && args[4] == "--") else {
            throw ProbeError("Use -- PROGRAM ARG... after the image to override the command")
        }
        let container = SandboxContainer(id: "main", image: args[3], arguments: Array(args.dropFirst(5)))
        manifest = PodManifest(containers: [container], foreground: container.id)
    case "pod":
        guard args.count == 4 else { throw ProbeError("Provide exactly one Pod manifest") }
        manifest = try JSONDecoder().decode(PodManifest.self, from: Data(contentsOf: URL(fileURLWithPath: args[3])))
    default:
        throw ProbeError("Unknown workload command: \(args[0])")
    }
    guard manifest.containers.contains(where: { $0.id == manifest.foreground }) else {
        throw ProbeError("Foreground member \(manifest.foreground) is missing from the Pod manifest")
    }

    let assets = URL(fileURLWithPath: args[1], isDirectory: true)
    let work = URL(fileURLWithPath: args[2], isDirectory: true)
    let sandbox = try await PodSandbox.create(
        assetsDirectory: assets,
        workParent: work,
        containers: manifest.containers,
        configurePod: { _, config in config.volumes = [workspaceVolume] },
        configureContainer: { id, config in
            config.mounts.append(workspaceMount)
            let output = id == manifest.foreground ? FileHandle.standardOutput : .standardError
            config.process.stdout = ConsoleWriter(output: output)
            config.process.stderr = ConsoleWriter(output: .standardError)
        }
    )
    return try await withSandbox(sandbox) {
        let status = try JSONEncoder().encode(await sandbox.status())
        try FileHandle.standardError.write(contentsOf: status + Data([0x0a]))
        for container in manifest.containers {
            try await sandbox.start(container: container.id)
        }
        let result = try await sandbox.wait(container: manifest.foreground)
        return Int32(result.exitCode)
    }
}
