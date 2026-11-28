import Containerization
import Foundation

let acceptanceBusyBox = "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
let acceptanceRedis = "docker.io/library/redis@sha256:5af80107c1eeecb484a2269b80a26d12bfa6521787da4663bd9f19f60a163483"

func sandboxCommand(_ sandbox: PodSandbox, member: String, _ command: [String]) async throws -> String {
    let capture = Capture()
    let result = try await sandbox.exec(container: member, arguments: command, timeoutInSeconds: 20) {
        $0.stdout = capture
        $0.stderr = capture
    }
    guard result.exitCode == 0 else {
        throw ProbeError("Sandbox \(sandbox.id) member \(member) returned \(result.exitCode): \(capture.text)")
    }
    return capture.text.trimmingCharacters(in: .whitespacesAndNewlines)
}

func verifyLifecycle(_ args: [String]) async throws {
    guard args.count == 3 else { throw ProbeError("Usage: nautilus-pod-probe verify-lifecycle ASSETS WORK_PARENT") }
    let assets = URL(fileURLWithPath: args[1], isDirectory: true)
    let work = URL(fileURLWithPath: args[2], isDirectory: true)
    let preparation = try ImagePreparation(assetsDirectory: assets, workParent: work)
    do {
        try await checkLifecycle(assets: assets, work: work, preparation: preparation)
    } catch {
        try await preparation.remove()
        throw error
    }
    try await preparation.remove()
}

func checkLifecycle(assets: URL, work: URL, preparation: ImagePreparation) async throws {
    let sentinel = UUID().uuidString.lowercased()
    let worker = SandboxContainer(id: "app", image: acceptanceBusyBox, arguments: ["sleep", "180"])
    let database = SandboxContainer(id: "db", image: acceptanceRedis, arguments: [
        "/usr/local/bin/redis-server", "--bind", "127.0.0.1", "--save", "", "--appendonly", "no", "--dir", "/workspace",
    ])
    let app = SandboxContainer(id: "app", image: acceptanceBusyBox, arguments: ["/bin/sh", "-ec", #"""
        test ! -e /workspace/result
        for attempt in 1 2 3 4 5 6 7 8 9 10; do
            response=$(printf 'SET sandbox \#(sentinel)\r\nGET sandbox\r\nQUIT\r\n' | nc -w 1 127.0.0.1 6379) || true
            if printf '%s\n' "$response" | grep -Fq -- '\#(sentinel)'; then
                printf '%s' '\#(sentinel)' > /workspace/result
                readlink /proc/self/ns/pid
                cat /proc/sys/kernel/random/boot_id
                exit 23
            fi
            sleep 1
        done
        exit 1
        """#])
    let appOutput = Capture()

    @Sendable func create(_ members: [SandboxContainer], captureApp: Bool = false) async throws -> PodSandbox {
        try await PodSandbox.create(
            assetsDirectory: assets,
            workParent: work,
            containers: members,
            preparation: preparation,
            configurePod: { _, config in config.volumes = [workspaceVolume] },
            configureContainer: { id, config in
                config.mounts.append(workspaceMount)
                if captureApp && id == "app" {
                    config.process.stdout = appOutput
                    config.process.stderr = appOutput
                }
            }
        )
    }

    let standalone = try await create([worker])
    let firstBoot = try await withSandbox(standalone) {
        let ready = await standalone.status()
        guard ready.phase == .ready, ready.containers.map(\.phase) == [.created] else {
            throw ProbeError("Sandbox returned before reaching READY with its member created")
        }
        try await standalone.start(container: worker.id)
        let other = try await create([worker])
        return try await withSandbox(other) {
            try await other.start(container: worker.id)
            let firstBoot = try await sandboxCommand(standalone, member: worker.id, ["cat", "/proc/sys/kernel/random/boot_id"])
            let otherBoot = try await sandboxCommand(other, member: worker.id, ["cat", "/proc/sys/kernel/random/boot_id"])
            guard standalone.id != other.id, firstBoot != otherBoot else { throw ProbeError("Standalone containers shared a VM") }
            try await standalone.stop()
            try await standalone.stop()
            let stopped = await standalone.status()
            guard stopped.phase == .stopped else { throw ProbeError("Stopped VM still reports READY") }
            let surviving = try await sandboxCommand(other, member: worker.id, ["cat", "/proc/sys/kernel/random/boot_id"])
            guard surviving == otherBoot else { throw ProbeError("Stopping one sandbox affected the other guest") }
            print("PASS: standalone sandboxes \(standalone.id) and \(other.id) own different VMs; second guest remained live")
            return firstBoot
        }
    }
    try await standalone.remove()
    let removed = await standalone.status()
    guard removed.phase == .removed, !FileManager.default.fileExists(atPath: standalone.workDirectory.path) else {
        throw ProbeError("Removed sandbox still reports live state or has a work directory")
    }

    let group = try await create([database, app], captureApp: true)
    try await withSandbox(group) {
        try await group.start(container: database.id)
        try await group.start(container: app.id)
        let result = try await group.wait(container: app.id, timeoutInSeconds: 30)
        guard result.exitCode == 23 else { throw ProbeError("Redis app exited \(result.exitCode): \(appOutput.text)") }
        let afterExit = await group.status()
        guard afterExit.containers.map(\.phase) == [.running, .exited], afterExit.containers.last?.exitCode == 23 else {
            throw ProbeError("Application exit was not reported independently of its sibling")
        }
        let appIdentity = appOutput.text.split(whereSeparator: \.isNewline)
        let dbPID = try await sandboxCommand(group, member: database.id, ["readlink", "/proc/self/ns/pid"])
        let dbBoot = try await sandboxCommand(group, member: database.id, ["cat", "/proc/sys/kernel/random/boot_id"])
        let artifact = try await sandboxCommand(group, member: database.id, ["cat", "/workspace/result"])
        let stored = try await sandboxCommand(group, member: database.id, ["/usr/local/bin/redis-cli", "--raw", "GET", "sandbox"])
        guard appIdentity.count == 2, appIdentity[0] != dbPID, appIdentity[1] == dbBoot, artifact == sentinel, stored == sentinel else {
            throw ProbeError("Pod VM identity, private PID namespaces, or shared Redis data did not match")
        }
        print("PASS: app and Redis share guest \(dbBoot), independent PID namespaces, Redis value and workspace artifact; database survived exit 23")
    }

    let rebuilt = try await create([worker])
    try await withSandbox(rebuilt) {
        try await rebuilt.start(container: worker.id)
        let newBoot = try await sandboxCommand(rebuilt, member: worker.id, ["cat", "/proc/sys/kernel/random/boot_id"])
        guard standalone.id != rebuilt.id, firstBoot != newBoot else { throw ProbeError("A rebuilt sandbox reused VM identity") }
        print("PASS: rebuilding the same manifest assigned a fresh sandbox and guest boot ID")
    }
}
