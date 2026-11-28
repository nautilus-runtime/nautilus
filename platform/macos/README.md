# Nautilus macOS PodSandbox probe

The probe registers two different Linux OCI images in one Apple Containerization `LinuxPod` and boots it directly with Virtualization.framework. It checks that both containers report the same guest boot ID, the client has its own writable rootfs, loopback carries a per-run token, both containers run with seccomp enabled, the client exits with code 23, and the VM reaches the stopped state.

The VM starts without an external network interface. The host downloads the pinned images and prepares the Linux root filesystems before launch. The pinned Apple Containerization version is `0.57.0`. Both members are registered before the VM boots. The guest uses runc as its OCI runtime to apply the seccomp profile.

## Run the probe

Use an Apple Silicon Mac with macOS 26 or newer, Swift 6.2 or newer, Python 3.9 or newer, and `zstd`. Start in `platform/macos`. The work directory contains several gigabytes of dependencies and guest images. Choose a volume with room for both.

```sh
probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/nautilus-m0.XXXXXXXX")"
rtk proxy bash ./prepare-assets.sh "$probe_dir/assets"
rtk proxy swift build --scratch-path "$probe_dir/build" --cache-path "$probe_dir/cache"
bin_dir="$(rtk proxy swift build --scratch-path "$probe_dir/build" --show-bin-path)"
rtk proxy codesign --force --sign - --entitlements virtualization.entitlements "$bin_dir/nautilus-pod-probe"
rtk proxy "$bin_dir/nautilus-pod-probe" "$probe_dir/assets" "$probe_dir"
```

A successful run prints a `PASS` line. The `Probe data` line identifies the run directory and its guest serial log. Asset preparation verifies the pinned Kata kernel archive, Apple `vminit` layer, and arm64 runc before creating the boot files.

## Run workloads

`run` creates an implicit sandbox and VM for one container. An optional command after `--` replaces the image entrypoint and command.

```sh
rtk proxy "$bin_dir/nautilus-pod-probe" run "$probe_dir/assets" "$probe_dir" \
  docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec \
  -- /bin/sh -c 'cat /proc/sys/kernel/random/boot_id; exit 23'
```

`pod ASSETS WORK_PARENT MANIFEST.json` creates an explicit sandbox. The manifest lists `containers` in start order and names the `foreground` container. Each member has an `id`, an OCI `image`, and optional `arguments`, `environment` (`NAME=value` strings), and `workingDirectory`. If `arguments` is absent, the image's entrypoint and command are used. Members share loopback and a guest-only `/workspace` tmpfs, with separate writable roots and PID namespaces.

The foreground command streams stdout, prints initial sandbox status and diagnostics to stderr, returns the selected container's exit code, and stops and removes its sandbox before returning. Internally, the sandbox exposes status, per-member start, wait and exec, and idempotent stop and remove. The local process owns the VM for the lifetime of the invocation.

```sh
rtk proxy "$bin_dir/nautilus-pod-probe" verify-lifecycle "$probe_dir/assets" "$probe_dir"
```

The lifecycle acceptance exercises two independent live VMs, stopping one while the other responds, and an application that writes and reads real data from Redis over loopback. It also checks workspace sharing, distinct PID namespaces, the application exit code, and fresh VM identity when rebuilding the same sandbox.

## Workload policy

Each sandbox gets 2 vCPUs, 512 MiB of memory, and a private 16 MiB guest tmpfs. Each container has a one-core CPU quota, a 128 MiB memory limit, a 128-task limit, default seccomp and `no_new_privs`. The guest has loopback only. The host downloads OCI images and provides the guest control connection. Host files, environment variables, and sockets are excluded from the container configuration by default.

Pass `--input FILE` to stage a single regular file, at most 1 MiB, in a private runtime directory. The container sees its read-only copy at `/input/file`. Pass `--output NEW_FILE` to export the selected foreground container's stdout, at most 1 MiB, to a new mode-0600 file. Paths are selected by the caller through command-line options. Pod manifests control container images, commands, and guest environment values.

Use `--timeout SECONDS` to stop the foreground command container and its descendants, then return exit status `124` after cleaning up the sandbox. When using the PodSandbox API directly, stopping a command container preserves its siblings. The cancellation boundary is the private PID namespace of a predeclared command container. A command run via `exec` inside a long-lived container has single-process signal semantics.

Concurrent invocations with the same `WORK_PARENT` hold an exclusive lock on each live session directory. The next invocation collects unlocked session directories left by an abruptly exited owner. Run on a private local filesystem.

```sh
rtk proxy "$bin_dir/nautilus-pod-probe" verify-policy "$probe_dir/assets" "$probe_dir"
rtk proxy python3 ./verify-owner-exit.py "$bin_dir/nautilus-pod-probe" "$probe_dir/assets" "$probe_dir"
```

Policy acceptance checks the guest network, host file and socket visibility, read-only input, explicit artifact export, guest and container resource settings, seccomp, detached-child cancellation, startup failures and 20 consecutive VM cleanup cycles. The owner-exit test uses two live VMs, stops one host process with `SIGKILL`, and checks stale storage recovery and the surviving workload.

## M0 validation

On 2026-11-28, an Apple M4 running macOS 27.2.1 (Darwin 27.2.0) passed two signed runs built with macOS SDK 27.2 and Swift 6.4.2. Both runs used Kata's Linux 6.18.56 kernel from release 4.3.1, Apple `vminit` 0.57.0, and runc 1.5.3. BusyBox served a per-run token over guest loopback, and Alpine retrieved it and exited with code 23. Both containers reported `Seccomp: 2` and one active filter. The final run reported guest boot ID `2d6f50ac-533e-4b2e-a6e7-fc4c67031aa6` for both containers, and the VM reached `.stopped`.

## M1 and M2 validation

The same Apple M4 host passed `verify-lifecycle`, `verify-policy` and `verify-owner-exit.py` with Virtualization.framework build `308.2.7`. The policy test completed all 20 cleanup cycles and verified both a failed container start and a rejected guest boot. The owner-exit test associated the owner’s guest disks with the VZ provider process, observed that process exit after `SIGKILL` of the owner, and confirmed stale storage was reclaimed while the peer workload finished normally. The signed CLI also returned exit code `23` for `run`, accepted a two-member `pod` manifest with an explicit environment and working directory, round-tripped the approved file to a mode-0600 export, and returned `124` after timeout and cleanup.

## References

- Lima documents native Linux guest boot under Apple virtualization. Colima shows how to select and configure VZ through Lima. [Lima VZ backend](https://lima-vm.io/docs/config/vmtype/vz/) · [Colima's Lima configuration](https://github.com/abiosoft/colima/blob/main/environment/vm/lima/yaml.go)
- Kiri uses a native macOS VM controller and a Linux guest agent. [Kiri's architecture](https://github.com/libracoder/kiri/blob/main/docs/development.md#architecture)
- [Apple LinuxPod API](https://github.com/apple/containerization/blob/0.57.0/Sources/Containerization/LinuxPod.swift) and [runc Pod integration tests](https://github.com/apple/containerization/blob/0.57.0/Sources/Integration/RuncPodTests.swift) provide the Pod and seccomp implementation used by the probe. [Apple entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.virtualization)
