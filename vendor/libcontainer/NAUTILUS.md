# Nautilus integration

This directory contains the library source, normalized Cargo manifest and tests from the crates.io release of libcontainer 0.8.0. Its license is Apache-2.0. libcgroups comes directly from crates.io. [Upstream release](https://github.com/youki-dev/youki/releases/tag/v0.8.0)

Nautilus carries one change in `src/container/tenant_builder.rs`. When building a process for CRI exec, preserve the original OCI seccomp profile, masked and read-only paths, rootfs propagation and resource settings. Recheck this change when updating the library. [Upstream seccomp fix under review](https://github.com/youki-dev/youki/pull/3799)
