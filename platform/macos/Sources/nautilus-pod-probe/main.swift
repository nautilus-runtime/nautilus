import Containerization
import ContainerizationOCI
import Darwin
import Foundation
import Synchronization
import Virtualization

struct ProbeError: Error, CustomStringConvertible {
    let description: String

    init(_ description: String) {
        self.description = description
    }
}

final class Capture: Writer {
    private let bytes = Mutex(Data())

    func write(_ data: Data) throws {
        bytes.withLock { $0.append(data) }
    }

    func close() throws {}

    var text: String {
        bytes.withLock { String(decoding: $0, as: UTF8.self) }
    }
}

let identityProbe = #"""
    printf 'boot_id=%s\n' "$(cat /proc/sys/kernel/random/boot_id)"
    printf 'guest_kernel=%s\n' "$(uname -r)"
    awk '$1 == "Seccomp:" {print "seccomp=" $2}
         $1 == "Seccomp_filters:" {print "filters=" $2}' /proc/self/status
    """#

func checkedBootID(_ output: String, container: String) throws -> UUID {
    let lines = output.split(separator: "\n")
    guard let bootLine = lines.first(where: { $0.hasPrefix("boot_id=") }),
          let bootID = UUID(uuidString: String(bootLine.dropFirst("boot_id=".count))),
          lines.contains("seccomp=2"),
          let filterLine = lines.first(where: { $0.hasPrefix("filters=") }),
          let filters = Int(filterLine.dropFirst("filters=".count)), filters > 0
    else {
        throw ProbeError("\(container) did not report a valid boot ID and an active seccomp filter: \(output)")
    }
    return bootID
}

func runProbe() async throws {
    guard CommandLine.arguments.count == 3 else {
        throw ProbeError("Usage: nautilus-pod-probe ASSET_DIRECTORY WORK_PARENT")
    }
    guard VZVirtualMachine.isSupported else {
        throw ProbeError("Virtualization.framework is unavailable on this host")
    }

    let assets = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    let workParent = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
    let id = UUID().uuidString.lowercased()
    let work = workParent.appendingPathComponent("probe-\(id)", isDirectory: true)
    let files = FileManager.default
    let kernelURL = assets.appendingPathComponent("kernel")
    let initArchive = assets.appendingPathComponent("init-rootfs.tar.gz")
    for asset in [kernelURL, initArchive] where !files.isReadableFile(atPath: asset.path) {
        throw ProbeError("Missing probe asset: \(asset.path)")
    }
    try files.createDirectory(at: work, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    print("Probe data: \(work.path)")

    let platform = Platform(arch: "arm64", os: "linux", variant: "v8")
    let content = try LocalContentStore(path: work.appendingPathComponent("content"))
    let images = try ImageStore(path: work, contentStore: content)
    let initImage = try await InitImage.create(
        reference: "localhost/nautilus-probe-init:m0",
        rootfs: initArchive,
        platform: platform,
        imageStore: images,
        contentStore: content
    )
    let initFS = try await initImage.initBlock(at: work.appendingPathComponent("init.ext4"), for: .linuxArm)

    let serverImage = try await images.pull(
        reference: "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec",
        platform: platform
    )
    let clientImage = try await images.pull(
        reference: "ghcr.io/linuxcontainers/alpine@sha256:28a33e0390da2b341d05e53803c6766977039baedfc8a82a4910d5acc4755b66",
        platform: platform
    )
    guard serverImage.digest != clientImage.digest else {
        throw ProbeError("The two OCI images resolved to the same digest")
    }
    print("OCI server: \(serverImage.digest)")
    print("OCI client: \(clientImage.digest)")
    let unpacker = EXT4Unpacker(capacityInBytes: 128 * 1024 * 1024)
    let serverFS = try await unpacker.unpack(serverImage, for: platform, at: work.appendingPathComponent("server.ext4"))
    let clientFS = try await unpacker.unpack(clientImage, for: platform, at: work.appendingPathComponent("client.ext4"))

    let kernel = Kernel(path: kernelURL, platform: .linuxArm)
    let vmm = VZVirtualMachineManager(kernel: kernel, initialFilesystem: initFS)
    let serialLog = work.appendingPathComponent("guest-serial.log")
    let pod = try LinuxPod(id, vmm: vmm, vm: VMResources(cpus: 2, memoryInBytes: 512 * 1024 * 1024)) {
        $0.interfaces = []
        $0.ociRuntimePath = "/sbin/runc"
        $0.seccompProfile = .default
        $0.bootLog = .file(path: serialLog)
    }

    let serverOutput = Capture()
    let clientOutput = Capture()
    // Use guest loopback without adding a network device to the VM.
    let serverProgram = #"""
        set -eu
        test ! -e /etc/alpine-release
        touch /.nautilus-probe-server
        mkdir /www
        printf '%s\n' "\#(id)" > /www/index.html
        \#(identityProbe)
        exec /bin/busybox httpd -f -h /www -p 127.0.0.1:18080
        """#
    let clientProgram = #"""
        set -eu
        test -f /etc/alpine-release
        test ! -e /.nautilus-probe-server
        touch /.nautilus-probe-client
        \#(identityProbe)
        for try in 1 2 3 4 5; do
            if answer="$(wget -q -T 2 -O - http://127.0.0.1:18080/)" && [ "$answer" = "\#(id)" ]; then
                printf 'loopback=%s\n' "$answer"
                exit 23
            fi
            sleep 1
        done
        exit 1
        """#
    try await pod.addContainer("server", rootfs: serverFS) { config in
        config.process.arguments = ["/bin/sh", "-c", serverProgram]
        config.process.environmentVariables = ["PATH=/bin:/usr/bin"]
        config.process.noNewPrivileges = true
        config.process.stdout = serverOutput
        config.process.stderr = serverOutput
    }
    try await pod.addContainer("client", rootfs: clientFS) { config in
        config.process.arguments = ["/bin/sh", "-c", clientProgram]
        config.process.environmentVariables = ["PATH=/bin:/usr/bin"]
        config.process.noNewPrivileges = true
        config.process.stdout = clientOutput
        config.process.stderr = clientOutput
    }

    print("Starting PodSandbox \(id) with Apple Virtualization.framework")
    try await pod.create()
    do {
        let instance = try await pod.withVirtualMachineInstance { $0 }
        try await pod.startContainer("server")
        try await pod.startContainer("client")
        let clientStatus = try await pod.waitContainer("client", timeoutInSeconds: 30)

        print("Server output:\n\(serverOutput.text)")
        print("Client output:\n\(clientOutput.text)")
        guard clientStatus.exitCode == 23, clientOutput.text.split(separator: "\n").contains("loopback=\(id)") else {
            throw ProbeError("Client returned \(clientStatus.exitCode), expected 23 with the server's token")
        }

        let serverBootID = try checkedBootID(serverOutput.text, container: "server")
        let clientBootID = try checkedBootID(clientOutput.text, container: "client")
        guard serverBootID == clientBootID else {
            throw ProbeError("The two OCI containers reported different guest boot IDs")
        }

        try await pod.stop()
        guard instance.state == .stopped else {
            throw ProbeError("The guest VM did not reach the stopped state")
        }

        print("PASS: VM stopped, guest boot ID \(serverBootID), client exit 23, private rootfs, shared loopback, seccomp in both containers")
    } catch {
        let failure = error
        do {
            try await pod.stop()
        } catch {
            FileHandle.standardError.write(Data("PodSandbox stop after failure: \(error)\n".utf8))
        }
        throw failure
    }
}

do {
    try await runProbe()
} catch {
    FileHandle.standardError.write(Data("Probe failed: \(error)\n".utf8))
    exit(EXIT_FAILURE)
}
