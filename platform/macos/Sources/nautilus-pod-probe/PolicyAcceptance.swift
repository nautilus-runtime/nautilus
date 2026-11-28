import Containerization
import Darwin
import Foundation

func verifyPolicy(_ args: [String]) async throws {
    guard args.count == 3 else { throw ProbeError("Usage: nautilus-pod-probe verify-policy ASSETS WORK_PARENT") }
    let assets = URL(fileURLWithPath: args[1], isDirectory: true)
    let workParent = URL(fileURLWithPath: args[2], isDirectory: true)
    try await withHostSession(in: workParent) { work in
        try await checkPolicy(assets: assets, work: work)
    }
}

private func checkPolicy(assets: URL, work: URL) async throws {
    let files = FileManager.default
    let preparation = try ImagePreparation(assetsDirectory: assets, workParent: work)
    let input = work.appendingPathComponent("host-input")
    let export = work.appendingPathComponent("host-export")
    let marker = UUID().uuidString.lowercased()
    let markerBytes = Data(marker.utf8)
    try markerBytes.write(to: input)
    let artifact = ArtifactCapture()
    let worker = SandboxContainer(id: "worker", image: acceptanceBusyBox, arguments: ["/bin/sleep", "180"])
    let cancelled = CommandCancellation.container(image: acceptanceBusyBox)
    let producer = SandboxContainer(id: "artifact", image: acceptanceBusyBox, arguments: ["/bin/cat", "/input/file"])

    @Sendable func create(_ members: [SandboxContainer], input: URL? = nil) async throws -> PodSandbox {
        try await PodSandbox.create(
            assetsDirectory: assets,
            workParent: work,
            containers: members,
            preparation: preparation,
            configurePod: { directory, config in
                config.volumes = try SandboxFiles.prepareVolumes(in: directory, inputFile: input)
            },
            configureContainer: { id, config in
                applyWorkloadPolicy(to: &config, hasInput: input != nil)
                if id == producer.id { config.process.stdout = artifact }
            }
        )
    }

    let baseline = try Set(files.contentsOfDirectory(atPath: work.path))
    let first = try await create([worker, cancelled, producer], input: input)
    try await withSandbox(first) {
        try await first.start(container: worker.id)
        let probe = #"""
            test "$(ls /sys/class/net)" = lo
            test -z "$(ip route show)"
            if nc -w 1 192.0.2.1 443 </dev/null >/dev/null 2>&1; then exit 1; fi
            test ! -e "$1"
            test -z "${SSH_AUTH_SOCK:-}"
            test ! -S /var/run/docker.sock
            test ! -S /run/nautilus/nautilus.sock
            test "$(cat /input/file)" = '\#(marker)'
            if (printf no > /input/file) 2>/dev/null; then exit 1; fi
            if (touch /input/extra) 2>/dev/null; then exit 1; fi
            test "$(ls -A /input)" = file
            test "$(grep -c '^processor' /proc/cpuinfo)" = 2
            awk '$1 == "MemTotal:" { exit !($2 > 350000 && $2 <= 524288) }' /proc/meminfo
            test "$(cat /sys/fs/cgroup/cpu.max)" = '100000 100000'
            test "$(cat /sys/fs/cgroup/memory.max)" = 134217728
            test "$(cat /sys/fs/cgroup/pids.max)" = 128
            awk '$1 == "Seccomp:" { s = $2 } $1 == "Seccomp_filters:" { f = $2 }
                 $1 == "NoNewPrivs:" { n = $2 } END { exit !(s == 2 && f > 0 && n == 1) }' /proc/self/status
            printf '%s' '\#(marker)' >/workspace/private
            """#
        _ = try await sandboxCommand(first, member: worker.id, ["/bin/sh", "-ec", probe, "policy-probe", input.path])
        print("PASS: loopback-only networking, host paths and sockets absent, read-only input, VM and container limits, seccomp and no-new-privileges")

        let second = try await create([worker])
        try await withSandbox(second) {
            try await second.start(container: worker.id)
            _ = try await sandboxCommand(second, member: worker.id, [
                "/bin/sh", "-ec", "test ! -e /input/file; test ! -e /workspace/private",
            ])
            print("PASS: second live sandbox cannot read first sandbox's input or workspace")
        }

        try await CommandCancellation.verify(in: first, sibling: worker.id) { id in
            let code = try await waitOrCancel(first, container: id, timeout: 1)
            guard code == 124 else { throw ProbeError("Command did not time out: \(code)") }
            try await first.stop(container: id)
        }
        print("PASS: timeout stopped the command and its detached child; same-sandbox worker remained usable")

        try await first.start(container: producer.id)
        let result = try await first.wait(container: producer.id, timeoutInSeconds: 10)
        guard result.exitCode == 0 else { throw ProbeError("Artifact producer returned \(result.exitCode)") }
        try artifact.export(to: export)
        let exportBytes = try Data(contentsOf: export)
        let inputBytes = try Data(contentsOf: input)
        let attributes = try files.attributesOfItem(atPath: export.path)
        guard exportBytes == markerBytes, inputBytes == markerBytes, attributes[.posixPermissions] as? Int == 0o600 else {
            throw ProbeError("Export allowed input modification or returned the wrong bytes or permissions")
        }
        do {
            try artifact.export(to: export)
            throw ProbeError("Artifact export overwrote an existing file")
        } catch let failure as NSError where failure.domain == NSPOSIXErrorDomain && failure.code == Int(EEXIST) {
            // The exclusive create refused to overwrite the destination.
        }
        try files.removeItem(at: export)
        print("PASS: only the explicitly selected stdout artifact was exported, as a new mode-0600 file")
    }

    let failed = try await create([worker, SandboxContainer(id: "broken", image: acceptanceBusyBox, arguments: ["/missing-program"])])
    try await withSandbox(failed) {
        try await failed.start(container: worker.id)
        let started: Bool
        do {
            try await failed.start(container: "broken")
            started = true
        } catch let error as SandboxCleanupError {
            throw error
        } catch {
            started = false
        }
        guard !started else { throw ProbeError("Nonexistent guest program unexpectedly started") }
        _ = try await sandboxCommand(failed, member: worker.id, ["/bin/true"])
    }
    let afterFailure = try Set(files.contentsOfDirectory(atPath: work.path))
    guard afterFailure == baseline else { throw ProbeError("Partial start left sandbox files behind") }
    print("PASS: partial container start failure preserved the healthy sibling and removed the full VM on teardown")

    let invalidAssets = work.appendingPathComponent("bad-guest", isDirectory: true)
    try files.createDirectory(at: invalidAssets, withIntermediateDirectories: false)
    try Data("not a Linux boot image".utf8).write(to: invalidAssets.appendingPathComponent("kernel"))
    try files.createSymbolicLink(
        at: invalidAssets.appendingPathComponent("init-rootfs.tar.gz"),
        withDestinationURL: assets.appendingPathComponent("init-rootfs.tar.gz")
    )
    let booted: Bool
    do {
        let unexpected = try await PodSandbox.create(assetsDirectory: invalidAssets, workParent: work, containers: [worker])
        try await unexpected.remove()
        booted = true
    } catch let error as SandboxCleanupError {
        throw error
    } catch {
        booted = false
    }
    try files.removeItem(at: invalidAssets)
    let afterBootFailure = try Set(files.contentsOfDirectory(atPath: work.path))
    guard !booted, afterBootFailure == baseline else {
        throw ProbeError("Invalid guest boot succeeded or left sandbox files behind")
    }
    print("PASS: rejected an invalid guest boot and removed its root filesystems")

    let short = SandboxContainer(id: "short", image: acceptanceBusyBox, arguments: ["/bin/true"])
    for cycle in 1...20 {
        let sandbox = try await create([short])
        try await withSandbox(sandbox) {
            try await sandbox.start(container: short.id)
            let result = try await sandbox.wait(container: short.id, timeoutInSeconds: 15)
            guard result.exitCode == 0 else { throw ProbeError("Cycle \(cycle) returned \(result.exitCode)") }
            try await sandbox.stop()
            try await sandbox.stop()
        }
        try await sandbox.remove()
        let remaining = try Set(files.contentsOfDirectory(atPath: work.path))
        guard remaining == baseline else { throw ProbeError("Cycle \(cycle) left sandbox files behind") }
        print("PASS: repeated VM cleanup \(cycle)/20")
    }
    try await preparation.remove()
}
