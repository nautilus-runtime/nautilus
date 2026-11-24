# Nautilus

Use `rtk` for shell commands in workspaces configured with the user's local `~/.codex/RTK.md`.

Write GitHub issue and pull request titles, descriptions and review comments in English.

## Runtime changes

- Read [the architecture](docs/ARCHITECTURE.md) before changing runtime behaviour. Workload commands and kubelet use the daemon's CRI socket. The local VM probe runs directly on the node.
- For CRI changes, compare the pinned upstream definition in [the VM contract](docs/CRI_VM_CONTRACT.md) with [the vendored protobuf](proto/cri-api/runtime/v1/api.proto). Cargo generates the Rust bindings from that file.
- For Pod-level VMs, use [the VM design and implementation plan](docs/VM_RUNTIME.md) to find the lifecycle owner, the current implementation scope and its acceptance tests.
- When a VM design involves a tradeoff, compare the relevant stable Kata Containers and Firecracker implementations. Record the selected behaviour, upstream versions and a CRI acceptance case in [the VM design](docs/VM_RUNTIME.md).
- Track VM implementation in [the parent GitHub issue](https://github.com/nautilus-runtime/nautilus/issues/1). Its sub-issues carry the acceptance criteria and blocking dependencies.
- Keep new persisted fields readable from older sled records with `#[serde(default)]`. Preserve idempotent stop and remove operations so kubelet can retry cleanup.

## Verification

- Run Rust checks on Linux. From the repository root use `rtk cargo fmt --check`, `rtk cargo clippy --locked -- -D warnings` and `rtk cargo test --locked`. Cargo defaults to `x86_64-unknown-linux-gnu`. Select the corresponding target explicitly on other Linux architectures.
- After a CRI-visible change, run the full `critest` suite against the test daemon's socket for both runtime and image endpoints. Record the tool version and pass, fail and skip counts alongside [the conformance baseline](docs/CRITEST_RESULTS.md).
- For VM lifecycle changes, record the VMM version and running host kernel, and exercise the changed behaviour through CRI. A guest boot establishes host compatibility. CRI acceptance also exercises the daemon's routing, status and cleanup paths.
