import Containerization
import Foundation
import NIOPosix
import Synchronization
import Virtualization

struct SandboxError: Error, CustomStringConvertible {
    let description: String

    init(_ description: String) {
        self.description = description
    }
}

struct SandboxCleanupError: Error, CustomStringConvertible {
    let description: String

    init(_ description: String) {
        self.description = description
    }
}

struct SandboxContainer: Decodable, Sendable {
    var id: String
    var image: String
    var arguments: [String]
    var environment: [String]
    var workingDirectory: String?

    init(
        id: String, image: String, arguments: [String] = [],
        environment: [String] = [], workingDirectory: String? = nil
    ) {
        self.id = id
        self.image = image
        self.arguments = arguments
        self.environment = environment
        self.workingDirectory = workingDirectory
    }

    private enum CodingKeys: String, CodingKey {
        case id, image, arguments, environment, workingDirectory
    }

    init(from decoder: any Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        self.init(
            id: try values.decode(String.self, forKey: .id),
            image: try values.decode(String.self, forKey: .image),
            arguments: try values.decodeIfPresent([String].self, forKey: .arguments) ?? [],
            environment: try values.decodeIfPresent([String].self, forKey: .environment) ?? [],
            workingDirectory: try values.decodeIfPresent(String.self, forKey: .workingDirectory)
        )
    }
}

struct SandboxStatus: Codable, Sendable {
    enum Phase: String, Codable, Sendable {
        case ready, stopping, stopped, removed
    }

    struct Container: Codable, Sendable {
        enum Phase: String, Codable, Sendable {
            case created, starting, running, exited, stopping, stopped, failed
        }

        let id: String
        let image: String
        let digest: String
        var phase: Phase
        var exitCode: Int32?
    }

    let id: String
    let phase: Phase
    let containers: [Container]
}

actor PodSandbox {
    let id: String
    let workDirectory: URL
    private let pod: LinuxPod
    private let vm: VZVirtualMachineInstance
    private var phase = SandboxStatus.Phase.ready
    private var containers: [SandboxStatus.Container]
    private var exits: [String: ExitStatus] = [:]
    private var exitObservers: [String: Task<Void, Never>] = [:]
    private var outputDrains: [String: Task<ExitStatus, Error>] = [:]
    private var stopping: Task<Void, Error>?
    private var stoppingContainers: [String: Task<Void, Error>] = [:]

    private init(
        id: String, workDirectory: URL, pod: LinuxPod,
        vm: VZVirtualMachineInstance, containers: [PreparedContainer]
    ) {
        self.id = id
        self.workDirectory = workDirectory
        self.pod = pod
        self.vm = vm
        self.containers = containers.map {
            SandboxStatus.Container(id: $0.spec.id, image: $0.spec.image, digest: $0.digest, phase: .created)
        }
    }

    static func create(
        assetsDirectory: URL,
        workParent: URL,
        containers: [SandboxContainer],
        preparation: ImagePreparation? = nil,
        configurePod: @Sendable (URL, inout LinuxPod.Configuration) throws -> Void = { _, _ in },
        configureContainer: @Sendable @escaping (String, inout LinuxPod.ContainerConfiguration) throws -> Void = { _, _ in }
    ) async throws -> PodSandbox {
        guard VZVirtualMachine.isSupported else {
            throw SandboxError("Virtualization.framework is unavailable on this host")
        }
        let names = Set(containers.map(\.id))
        guard !names.isEmpty, names.count == containers.count, names.allSatisfy(Self.validContainerID) else {
            throw SandboxError("Supply one or more containers with unique IDs made of letters, digits, '-' or '_'")
        }
        if let preparation, preparation.assetsDirectory.path != assetsDirectory.standardizedFileURL.path {
            throw SandboxError("The image cache belongs to a different guest asset directory")
        }

        let id = UUID().uuidString.lowercased()
        let work = workParent.appendingPathComponent("sandbox-\(id)", isDirectory: true)
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let capture = VMCapture()
        do {
            let images = try preparation ?? ImagePreparation(assetsDirectory: assetsDirectory, workParent: work)
            let prepared = try await images.materialize(containers, in: work)
            if preparation == nil {
                try await images.remove()
            }
            let kernel = Kernel(path: assetsDirectory.appendingPathComponent("kernel"), platform: .linuxArm)
            let vmm = VZVirtualMachineManager(
                kernel: kernel, initialFilesystem: prepared.initialFilesystem, group: MultiThreadedEventLoopGroup.singleton
            )
            let pod = try LinuxPod(id, vmm: vmm, vm: VMResources(cpus: 2, memoryInBytes: 512 * 1024 * 1024)) { config in
                try configurePod(work, &config)
                config.ociRuntimePath = "/sbin/runc"
                config.seccompProfile = .default
                config.shareProcessNamespace = false
                config.interfaces = []
                config.virtualization = false
                config.bootLog = .file(path: work.appendingPathComponent("guest-serial.log"))
                config.extensions.append(capture)
            }

            for container in prepared.containers {
                try await pod.addContainer(container.spec.id, rootfs: container.rootFS) { config in
                    config.process = container.process
                    config.cpus = 2
                    config.memoryInBytes = 320 * 1024 * 1024
                    try configureContainer(container.spec.id, &config)
                    config.process.noNewPrivileges = true
                    config.seccompProfile = .default
                }
            }
            try await pod.create()
            guard let vm = capture.take() else {
                do {
                    try await pod.stop()
                } catch {
                    throw SandboxCleanupError("Missing VZ instance and could not stop sandbox at \(work.path): \(error)")
                }
                throw SandboxError("Virtual machine creation did not return a VZ instance")
            }
            return PodSandbox(id: id, workDirectory: work, pod: pod, vm: vm, containers: prepared.containers)
        } catch {
            let startupError = error
            if let cleanupError = startupError as? SandboxCleanupError {
                throw cleanupError
            }
            if let vm = capture.take(), vm.state != .stopped {
                // VZ can start the VM before the guest agent handshake fails.
                let cleanup = Task {
                    try await vm.stop()
                }
                do {
                    try await cleanup.value
                } catch {
                    throw SandboxCleanupError("Sandbox setup failed: \(startupError). VM cleanup failed: \(error). Data retained at \(work.path)")
                }
            }
            do {
                try FileManager.default.removeItem(at: work)
            } catch {
                throw SandboxCleanupError("Sandbox setup failed: \(startupError). Data cleanup failed at \(work.path): \(error)")
            }
            throw startupError
        }
    }

    func status() -> SandboxStatus {
        if phase == .ready, vm.state == .stopped {
            markStopped()
        }
        return SandboxStatus(id: id, phase: phase, containers: containers)
    }

    func start(container: String) async throws {
        let index = try containerIndex(container)
        try requireReady()
        if containers[index].phase == .running { return }
        guard containers[index].phase == .created else {
            throw SandboxError("Container \(container) cannot start from \(containers[index].phase.rawValue)")
        }
        containers[index].phase = .starting
        do {
            try await pod.startContainer(container)
            if containers[index].phase == .starting {
                containers[index].phase = .running
            }
        } catch {
            if containers[index].phase == .starting {
                containers[index].phase = .failed
            }
            throw error
        }

        let vm = self.vm
        exitObservers[container] = Task.detached { [weak self] in
            if let status = try? await Self.waitForGuestProcess(vm: vm, container: container) {
                await self?.observeExit(container, status)
            }
        }
    }

    func wait(container: String, timeoutInSeconds: Int64? = nil) async throws -> ExitStatus {
        let index = try containerIndex(container)
        if let exit = exits[container] { return exit }
        let state = containers[index].phase
        guard state == .running || state == .exited || state == .stopping else {
            throw SandboxError("Container \(container) has not started or has no available exit status")
        }
        let exit = try await Self.waitForGuestProcess(vm: vm, container: container, timeoutInSeconds: timeoutInSeconds)
        observeExit(container, exit)
        if let exit = exits[container] { return exit }
        if let draining = outputDrains[container] { return try await draining.value }

        // Only LinuxProcess.wait drains stdio. Give it a single reader even if callers use different wait deadlines.
        let pod = self.pod
        let draining = Task {
            try await pod.waitContainer(container)
        }
        outputDrains[container] = draining
        defer { outputDrains.removeValue(forKey: container) }
        let completed = try await draining.value
        exits[container] = completed
        return completed
    }

    func startExec(
        container: String, arguments: [String],
        configure: @Sendable @escaping (inout LinuxProcessConfiguration) throws -> Void = { _ in }
    ) async throws -> LinuxProcess {
        let index = try containerIndex(container)
        try requireReady()
        guard containers[index].phase == .running else {
            throw SandboxError("Container \(container) is not running")
        }
        guard let command = arguments.first, !command.isEmpty,
              arguments.allSatisfy({ !$0.utf8.contains(0) }) else {
            throw SandboxError("Exec needs a command")
        }
        let process = try await pod.execInContainer(container, processID: UUID().uuidString.lowercased()) { config in
            config.arguments = arguments
            try configure(&config)
            config.noNewPrivileges = true
        }
        do {
            try Task.checkCancellation()
            try requireReady()
            guard containers[index].phase == .running else {
                throw SandboxError("Container \(container) has stopped")
            }
            try await process.start()
        } catch {
            try? await process.delete()
            throw error
        }
        return process
    }

    func exec(
        container: String,
        arguments: [String],
        timeoutInSeconds: Int64? = nil,
        configure: @Sendable @escaping (inout LinuxProcessConfiguration) throws -> Void = { _ in }
    ) async throws -> ExitStatus {
        let process = try await startExec(container: container, arguments: arguments, configure: configure)
        let exit: ExitStatus
        do {
            exit = try await process.wait(timeoutInSeconds: timeoutInSeconds)
        } catch {
            let waitError = error
            let cleanup = Task {
                try? await process.kill(.kill)
                _ = try? await process.wait(timeoutInSeconds: 3)
                try await process.delete()
            }
            do {
                try await cleanup.value
            } catch {
                throw SandboxError("Exec wait failed: \(waitError). Process cleanup failed: \(error)")
            }
            throw waitError
        }
        try await process.delete()
        return exit
    }

    func stop(container: String) async throws {
        let index = try containerIndex(container)
        if let operation = stopping {
            return try await operation.value
        }
        if let operation = stoppingContainers[container] {
            return try await operation.value
        }
        if phase == .stopped || phase == .removed || containers[index].phase == .stopped { return }
        if containers[index].phase == .exited { return }

        let wasRunning = containers[index].phase == .running || containers[index].phase == .starting
        let pod = self.pod
        let operation = Task {
            try await pod.stopContainer(container)
        }
        containers[index].phase = .stopping
        stoppingContainers[container] = operation
        defer { stoppingContainers.removeValue(forKey: container) }
        do {
            try await operation.value
            containers[index].phase = .stopped
        } catch {
            if phase == .stopped || phase == .removed { return }
            containers[index].phase = .failed
            if wasRunning, exits[container] != nil {
                containers[index].phase = .exited
            }
            throw error
        }
    }

    func stop() async throws {
        if let operation = stopping {
            return try await operation.value
        }
        if phase == .stopped || phase == .removed { return }

        phase = .stopping
        let pod = self.pod
        let vm = self.vm
        let operation = Task {
            if vm.state != .stopped {
                do {
                    try await pod.stop()
                } catch {
                    if vm.state == .running {
                        try await vm.stop()
                    } else if vm.state != .stopped {
                        throw error
                    }
                }
            }
            guard vm.state == .stopped else {
                throw SandboxError("Virtual machine did not stop")
            }
        }
        stopping = operation
        defer { stopping = nil }
        do {
            try await operation.value
            markStopped()
        } catch {
            if phase != .removed { phase = .ready }
            throw error
        }
    }

    func remove() async throws {
        do {
            try await stop()
            guard phase != .removed else { return }
            try FileManager.default.removeItem(at: workDirectory)
            phase = .removed
        } catch {
            throw SandboxCleanupError("Could not remove sandbox at \(workDirectory.path): \(error)")
        }
    }

    private func markStopped() {
        guard phase != .removed else { return }
        phase = .stopped
        for observer in exitObservers.values { observer.cancel() }
        exitObservers.removeAll()
        for index in containers.indices where containers[index].phase != .exited {
            containers[index].phase = .stopped
        }
    }

    private func observeExit(_ id: String, _ status: ExitStatus) {
        guard let index = containers.firstIndex(where: { $0.id == id }) else { return }
        containers[index].exitCode = status.exitCode
        if containers[index].phase == .running {
            containers[index].phase = .exited
        }
    }

    private static func waitForGuestProcess(
        vm: VZVirtualMachineInstance, container: String, timeoutInSeconds: Int64? = nil
    ) async throws -> ExitStatus {
        let agent = try await vm.dialAgent()
        do {
            let result = try await agent.waitProcess(id: container, containerID: container, timeoutInSeconds: timeoutInSeconds)
            try? await agent.close()
            return result
        } catch {
            try? await agent.close()
            throw error
        }
    }

    private func containerIndex(_ id: String) throws -> Int {
        guard let index = containers.firstIndex(where: { $0.id == id }) else {
            throw SandboxError("Unknown container: \(id)")
        }
        return index
    }

    private func requireReady() throws {
        guard phase == .ready, vm.state == .running else {
            throw SandboxError("Sandbox \(id) is not ready")
        }
    }

    private static func validContainerID(_ id: String) -> Bool {
        (1...64).contains(id.utf8.count) && id.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95
        }
    }
}

private final class VMCapture: VZInstanceExtension {
    private let instance = Mutex<VZVirtualMachineInstance?>(nil)

    func didCreate(_ vm: VZVirtualMachineInstance) throws {
        instance.withLock { $0 = vm }
    }

    func take() -> VZVirtualMachineInstance? {
        instance.withLock {
            let vm = $0
            $0 = nil
            return vm
        }
    }
}
