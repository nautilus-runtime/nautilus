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
            throw SandboxCleanupError("\(failure); sandbox \(sandbox.id) cleanup failed: \(error)")
        }
        throw failure
    }
    try await sandbox.remove()
    return result
}

func runWorkload(_ args: [String]) async throws -> Int32 {
    guard args.count >= 4 else {
        throw ProbeError("Usage: nautilus-pod-probe run ASSETS WORK_PARENT IMAGE [OPTIONS] [-- PROGRAM ARG...]\n       nautilus-pod-probe pod ASSETS WORK_PARENT MANIFEST.json [OPTIONS]\nOptions: --input FILE --output NEW_FILE --timeout SECONDS")
    }
    let options = try WorkloadOptions(args.dropFirst(4), allowCommand: args[0] == "run")
    let manifest: PodManifest
    switch args[0] {
    case "run":
        let container = SandboxContainer(id: "main", image: args[3], arguments: options.command)
        manifest = PodManifest(containers: [container], foreground: container.id)
    case "pod":
        manifest = try JSONDecoder().decode(PodManifest.self, from: Data(contentsOf: URL(fileURLWithPath: args[3])))
    default:
        throw ProbeError("Unknown workload command: \(args[0])")
    }
    guard manifest.containers.contains(where: { $0.id == manifest.foreground }) else {
        throw ProbeError("Foreground member \(manifest.foreground) is missing from the Pod manifest")
    }

    let assets = URL(fileURLWithPath: args[1], isDirectory: true)
    let workParent = URL(fileURLWithPath: args[2], isDirectory: true)
    let artifact = options.output.map { _ in ArtifactCapture() }
    return try await withHostSession(in: workParent) { work in
        let sandbox = try await PodSandbox.create(
            assetsDirectory: assets,
            workParent: work,
            containers: manifest.containers,
            configurePod: { directory, config in
                config.volumes = try SandboxFiles.prepareVolumes(in: directory, inputFile: options.input)
            },
            configureContainer: { id, config in
                applyWorkloadPolicy(to: &config, hasInput: options.input != nil)
                if id == manifest.foreground, let artifact {
                    config.process.stdout = artifact
                } else {
                    let output = id == manifest.foreground ? FileHandle.standardOutput : .standardError
                    config.process.stdout = ConsoleWriter(output: output)
                }
                config.process.stderr = ConsoleWriter(output: .standardError)
            }
        )
        let code = try await withSandbox(sandbox) {
            let status = try JSONEncoder().encode(await sandbox.status())
            try FileHandle.standardError.write(contentsOf: status + Data([0x0a]))
            for container in manifest.containers {
                try await sandbox.start(container: container.id)
            }
            try Task.checkCancellation()
            return try await waitOrCancel(sandbox, container: manifest.foreground, timeout: options.timeout)
        }
        if let output = options.output, let artifact {
            try artifact.export(to: output)
        }
        return code
    }
}
