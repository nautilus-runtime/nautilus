import Containerization
import ContainerizationOCI
import Foundation

struct PreparedContainer: Sendable {
    let spec: SandboxContainer
    let digest: String
    let rootFS: Containerization.Mount
    let process: LinuxProcessConfiguration
}

actor ImagePreparation {
    private struct ImageBase: Sendable {
        let image: Containerization.Image
        let digest: String
        let rootFS: Containerization.Mount
    }

    let assetsDirectory: URL
    let cacheDirectory: URL
    private let content: LocalContentStore
    private let images: ImageStore
    private let platform = Platform(arch: "arm64", os: "linux", variant: "v8")
    private var initTask: Task<Containerization.Mount, Error>?
    private var imageTasks: [String: Task<ImageBase, Error>] = [:]
    private var users = 0
    private var removed = false

    init(assetsDirectory: URL, workParent: URL) throws {
        self.assetsDirectory = assetsDirectory.standardizedFileURL
        for name in ["kernel", "init-rootfs.tar.gz"] {
            let asset = assetsDirectory.appendingPathComponent(name)
            guard FileManager.default.isReadableFile(atPath: asset.path) else {
                throw SandboxError("Missing sandbox asset: \(asset.path)")
            }
        }

        let cache = workParent.appendingPathComponent(
            "image-cache-\(UUID().uuidString.lowercased())",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: cache, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        do {
            let content = try LocalContentStore(path: cache.appendingPathComponent("content"))
            self.content = content
            self.images = try ImageStore(path: cache, contentStore: content)
            self.cacheDirectory = cache
        } catch {
            try? FileManager.default.removeItem(at: cache)
            throw error
        }
    }

    func materialize(
        _ containers: [SandboxContainer], in work: URL
    ) async throws -> (initialFilesystem: Containerization.Mount, containers: [PreparedContainer]) {
        guard !removed else {
            throw SandboxError("Image preparation cache has been removed")
        }
        users += 1
        defer { users -= 1 }

        let initBase = try await initialFilesystem()
        let initialFilesystem = try initBase.clone(to: work.appendingPathComponent("init.ext4").path)
        var children: [PreparedContainer] = []
        for (index, spec) in containers.enumerated() {
            let base = try await imageBase(reference: spec.image)
            let memberImage = work.appendingPathComponent("container-\(index).ext4")
            let rootFS = try base.rootFS.clone(to: memberImage.path)
            let imageConfig = try await base.image.config(for: platform)
            let process = try Self.processConfiguration(spec, image: imageConfig.config)
            children.append(PreparedContainer(spec: spec, digest: base.digest, rootFS: rootFS, process: process))
        }
        return (initialFilesystem, children)
    }

    func remove() throws {
        guard !removed else { return }
        guard users == 0 else {
            throw SandboxError("Image preparation cache is in use")
        }
        try FileManager.default.removeItem(at: cacheDirectory)
        initTask = nil
        imageTasks.removeAll()
        removed = true
    }

    private func initialFilesystem() async throws -> Containerization.Mount {
        if let initTask {
            return try await initTask.value
        }

        let archive = assetsDirectory.appendingPathComponent("init-rootfs.tar.gz")
        let output = cacheDirectory.appendingPathComponent("init-base.ext4")
        let platform = self.platform
        let images = self.images
        let content = self.content
        let task = Task {
            do {
                let image = try await InitImage.create(
                    reference: "localhost/nautilus-sandbox-init:local",
                    rootfs: archive,
                    platform: platform,
                    imageStore: images,
                    contentStore: content
                )
                return try await image.initBlock(at: output, for: .linuxArm)
            } catch {
                try? FileManager.default.removeItem(at: output)
                throw error
            }
        }
        initTask = task
        do {
            return try await task.value
        } catch {
            initTask = nil
            throw error
        }
    }

    private func imageBase(reference: String) async throws -> ImageBase {
        if let current = imageTasks[reference] {
            return try await current.value
        }

        let output = cacheDirectory.appendingPathComponent("image-\(UUID().uuidString.lowercased()).ext4")
        let platform = self.platform
        let images = self.images
        let task = Task {
            do {
                let image = try await images.pull(reference: reference, platform: platform)
                let digest = try await image.descriptor(for: platform).digest
                let unpacker = EXT4Unpacker(capacityInBytes: 512 * 1024 * 1024)
                let rootFS = try await unpacker.unpack(image, for: platform, at: output)
                return ImageBase(image: image, digest: digest, rootFS: rootFS)
            } catch {
                try? FileManager.default.removeItem(at: output)
                throw error
            }
        }
        imageTasks[reference] = task
        do {
            return try await task.value
        } catch {
            imageTasks.removeValue(forKey: reference)
            throw error
        }
    }

    private static func processConfiguration(
        _ spec: SandboxContainer, image: ImageConfig?
    ) throws -> LinuxProcessConfiguration {
        var process = image.map { LinuxProcessConfiguration(from: $0) } ?? LinuxProcessConfiguration()
        if !spec.arguments.isEmpty {
            process.arguments = spec.arguments
        }
        if let directory = spec.workingDirectory {
            process.workingDirectory = directory
        }
        for variable in spec.environment {
            guard let separator = variable.firstIndex(of: "="), separator != variable.startIndex,
                  !variable.utf8.contains(0) else {
                throw SandboxError("Container \(spec.id) has an invalid environment entry")
            }
            let name = variable[...separator]
            process.environmentVariables.removeAll { $0.hasPrefix(name) }
            process.environmentVariables.append(variable)
        }
        if !process.environmentVariables.contains(where: { $0.hasPrefix("PATH=") }) {
            process.environmentVariables.append("PATH=\(LinuxProcessConfiguration.defaultPath)")
        }
        guard let command = process.arguments.first, !command.isEmpty,
              process.arguments.allSatisfy({ !$0.utf8.contains(0) }),
              process.workingDirectory.hasPrefix("/"), !process.workingDirectory.utf8.contains(0) else {
            throw SandboxError("Container \(spec.id) needs a command and an absolute working directory")
        }
        process.noNewPrivileges = true
        return process
    }
}
