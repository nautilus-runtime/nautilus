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

## M0 validation

On 2026-11-28, an Apple M4 running macOS 27.2.1 (Darwin 27.2.0) passed two signed runs built with macOS SDK 27.2 and Swift 6.4.2. Both runs used Kata's Linux 6.18.56 kernel from release 4.3.1, Apple `vminit` 0.57.0, and runc 1.5.3. BusyBox served a per-run token over guest loopback, and Alpine retrieved it and exited with code 23. Both containers reported `Seccomp: 2` and one active filter. The final run reported guest boot ID `2d6f50ac-533e-4b2e-a6e7-fc4c67031aa6` for both containers, and the VM reached `.stopped`.

## References

- Lima documents native Linux guest boot under Apple virtualization. Colima shows how to select and configure VZ through Lima. [Lima VZ backend](https://lima-vm.io/docs/config/vmtype/vz/) · [Colima's Lima configuration](https://github.com/abiosoft/colima/blob/main/environment/vm/lima/yaml.go)
- Kiri uses a native macOS VM controller and a Linux guest agent. [Kiri's architecture](https://github.com/libracoder/kiri/blob/main/docs/development.md#architecture)
- [Apple LinuxPod API](https://github.com/apple/containerization/blob/0.57.0/Sources/Containerization/LinuxPod.swift) and [runc Pod integration tests](https://github.com/apple/containerization/blob/0.57.0/Sources/Integration/RuncPodTests.swift) provide the Pod and seccomp implementation used by the probe. [Apple entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.virtualization)
