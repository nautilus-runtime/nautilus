enum CommandCancellation {
    static let containerID = "cancel-command"

    static func container(image: String) -> SandboxContainer {
        SandboxContainer(
            id: containerID,
            image: image,
            arguments: ["/bin/sh", "-ec", #"""
                /bin/busybox setsid /bin/sh -ec '
                    trap "" HUP TERM
                    awk "\$1 == \$6 { detached = 1 } END { exit !detached }" "/proc/$$/stat"
                    exec 3>>/workspace/.nautilus-cancel-heartbeat
                    while :; do
                        printf x >&3
                        sleep 1
                    done
                ' </dev/null >/dev/null 2>&1 &
                wait "$!"
                """#]
        )
    }

    static func verify(
        in sandbox: PodSandbox,
        sibling: String,
        cancel: (String) async throws -> Void
    ) async throws {
        try await sandbox.start(container: containerID)
        _ = try await sandboxCommand(sandbox, member: sibling, ["/bin/sh", "-ec", #"""
            heartbeat=/workspace/.nautilus-cancel-heartbeat
            for attempt in 1 2 3 4 5 6; do
                if [ -s "$heartbeat" ]; then
                    before=$(wc -c < "$heartbeat")
                    sleep 2
                    if [ "$(wc -c < "$heartbeat")" -gt "$before" ]; then
                        exit 0
                    fi
                else
                    sleep 1
                fi
            done
            echo "detached command child did not produce a live heartbeat" >&2
            exit 1
            """#])

        // An OCI exec signal targets one PID. Use the command container's private PID namespace as the cancellation scope.
        try await cancel(containerID)
        _ = try await sandboxCommand(sandbox, member: sibling, ["/bin/sh", "-ec", #"""
            heartbeat=/workspace/.nautilus-cancel-heartbeat
            before=$(wc -c < "$heartbeat")
            sleep 2
            if [ "$before" -ne "$(wc -c < "$heartbeat")" ]; then
                echo "detached command child kept writing after cancellation" >&2
                exit 1
            fi
            """#])
    }
}
