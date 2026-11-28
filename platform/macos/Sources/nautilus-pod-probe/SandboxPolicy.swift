import Containerization
import ContainerizationError
import Foundation
import Synchronization

struct WorkloadOptions {
    var input: URL?
    var output: URL?
    var timeout: Int64?
    var command: [String] = []

    init(_ arguments: ArraySlice<String>, allowCommand: Bool) throws {
        var arguments = arguments
        while let option = arguments.popFirst() {
            if option == "--", allowCommand, !arguments.isEmpty {
                command = Array(arguments)
                return
            }
            guard let value = arguments.popFirst(), !value.isEmpty else {
                throw ProbeError("Missing value after \(option)")
            }
            switch option {
            case "--input" where input == nil:
                input = URL(fileURLWithPath: value)
            case "--output" where output == nil:
                output = URL(fileURLWithPath: value)
            case "--timeout" where timeout == nil:
                guard let seconds = Int64(value), (1...86400).contains(seconds) else {
                    throw ProbeError("Timeout must be between 1 and 86400 seconds")
                }
                timeout = seconds
            default:
                throw ProbeError("Unknown or repeated option \(option)")
            }
        }
    }
}

func applyWorkloadPolicy(to container: inout LinuxPod.ContainerConfiguration, hasInput: Bool) {
    container.mounts.append(contentsOf: SandboxFiles.containerMounts(hasInput: hasInput))
    container.cpus = 1
    container.memoryInBytes = 128 << 20
    container.unified = ["pids.max": "128"]
}

func waitOrCancel(_ sandbox: PodSandbox, container: String, timeout: Int64?) async throws -> Int32 {
    do {
        return try await sandbox.wait(container: container, timeoutInSeconds: timeout).exitCode
    } catch let error as ContainerizationError where error.code == .timeout {
        try await sandbox.stop(container: container)
        return 124
    }
}

final class ArtifactCapture: Writer {
    private struct State {
        var data = Data()
        var overflow = false
    }
    private let state = Mutex(State())

    func write(_ data: Data) throws {
        state.withLock { state in
            guard !state.overflow else { return }
            if data.count <= SandboxFiles.maximumFileBytes - state.data.count {
                state.data.append(data)
            } else {
                state.overflow = true
            }
        }
    }

    func close() throws {}

    func export(to path: URL) throws {
        let bytes = try state.withLock { state in
            guard !state.overflow else { throw ProbeError("Output exceeds the 1 MiB export limit") }
            return state.data
        }
        try SandboxFiles.export(bytes, to: path)
    }
}
