# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Nautilus is a CRI-compatible container runtime in Rust: a single daemon
(**nautilusd**) serving CRI gRPC over a unix socket, plus a docker-style
client (**nautilus**). Differentiator: multi-sandbox runtime — runc containers
(libcontainer in-process) and WASM workloads (wasmtime) behind one `Sandbox`
trait, with hot-pluggable Wasm lifecycle policies. Current status: MVP
complete, critest v1.31.1 **96/0 green**.

**Docs**: `docs/ARCHITECTURE.md` (authoritative architecture, matches code) ·
`docs/DEVELOPMENT_ROADMAP.md` (status + plans) · `docs/CRITEST_RESULTS.md`
(critest baseline history). Read ARCHITECTURE.md before non-trivial changes.

## Build & Test

`.cargo/config.toml` defaults to the `x86_64-unknown-linux-gnu` target —
nautilus is Linux-first (libcontainer/wasmtime paths require Linux). On an
x86_64 Linux box `cargo build` just works; elsewhere pass `--target` explicitly.

```bash
cargo build --release
cargo test                                                  # unit + integration
cargo clippy -- -D warnings                                 # zero-warning baseline
scripts/verify_remote.sh        # e2e on a remote Linux host: sync + build + 13-assertion smoke
scripts/critest_runner.sh       # CRI conformance (critest v1.31.1, baseline 96/0)
scripts/measure_memory.sh       # idle memory (baseline PSS 10 MiB)
```

Remote verification scripts run against any SSH-reachable Linux host: set
`NAUTILUS_REMOTE=<host>` (and `NAUTILUS_PROXY=<url>` if that host needs an
egress proxy). See `scripts/README.md` for the full testing guide.

Before running critest check `/proc/pressure/cpu` and for runaway workload
processes — CPU starvation causes random hangs that look like code bugs.
Prefer letting tests finish naturally; `kill -9` leaks container workloads.

## Architecture Map

```
src/main.rs              daemon entry: args, config, wiring, SIGINT/SIGTERM graceful exit
src/bin/nautilus.rs      docker-style client (CRI gRPC over UDS)
src/core/                config (TOML, serde; unknown sections ignored), error, sled state
src/cri/                 runtime_service, image_service, streaming (SPDY exec/attach/portforward), version
src/sandbox/             mod.rs: Sandbox trait + ResourceLimits; runc_backend (libcontainer,
                         exec-runc fallback via runtime.runc.driver); wasm_backend; spec.rs
                         (CRI security context → OCI: seccomp/AppArmor/caps/NnP)
src/runtime/             pod.rs / container.rs state (pod-level net/pid/ipc ns holders)
src/network/             podnet (netns holder), cni (chain executor), bridge, publish (hostPort), sysctl
src/storage/             pull.rs (native oci-distribution, content-addressed layers, auth),
                         unpack.rs (whiteout), image.rs (tag/digest merge, untag semantics)
src/policy/              Wasm policy engine (hooks, hot reload, fuel + timeout limits)
src/proto/               generated CRI protobuf code (k8s master, 43 RPCs)
tests/                   integration, e2e_lifecycle, network, image, policy,
                         security_spec, sysctl + tests/critest_integration.md
```

Key implementation facts that are easy to get wrong:
- libcontainer is pinned via `[patch.crates-io]` to the **Bevisy/youki fork**
  (rev `2b4cdda` = 2867015 + recursive rootfs propagation). Keep Cargo.toml and
  Cargo.lock revs in sync.
- Workloads are **exec'd directly** (no shell wrapper); container stdio uses
  `pipe2` + dedicated blocking threads with EINTR retry (not AsyncFd — it
  freezes the gRPC accept loop after fork).
- Streaming sessions carry all parameters in the URL token (critest doesn't
  send query params); the 101 response must echo `X-Stream-Protocol-Version`.
- Idempotency: Stop/Remove of missing or already-stopped pods/containers must
  return success (kubelet cleanup path depends on it).
- Unimplemented RPCs return `Status::unimplemented` — never `unimplemented!()`
  (the macro panics the task).

## Conventions

- New persisted state fields need `#[serde(default)]` (old sled state files
  must stay loadable); config unknown keys are ignored for backward compat.
- `cargo clippy -D warnings` and `cargo fmt` are kept clean — don't regress.
- After any CRI-visible change, run the full critest suite; the baseline must
  not regress from 96/0 (see CRITEST_RESULTS.md for how each fix landed).
- Doc files are uppercase in `docs/`; user-facing docs are written in Chinese,
  README/CLAUDE.md in English.
