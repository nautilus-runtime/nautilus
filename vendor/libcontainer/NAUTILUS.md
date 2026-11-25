# Nautilus integration

This directory contains the library source, normalized Cargo manifest and tests from the crates.io release of libcontainer 0.8.0. Its license is Apache-2.0. libcgroups comes directly from crates.io. [Upstream release](https://github.com/youki-dev/youki/releases/tag/v0.8.0)

Nautilus preserves the original OCI seccomp profile, masked and read-only paths, rootfs propagation and resource settings in `src/container/tenant_builder.rs` when building a process for CRI exec. [Upstream seccomp fix under review](https://github.com/youki-dev/youki/pull/3799)

In `src/container/container_delete.rs`, retry an owned cgroup v2 directory that remains busy after libcgroups has signalled its processes and exhausted its deletion attempts. Recheck both changes when updating libcontainer. [Nautilus issue #57](https://github.com/nautilus-runtime/nautilus/issues/57)
