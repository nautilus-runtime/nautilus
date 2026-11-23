//! In-process Wasm policy engine (P1b).
//!
//! Nautilus' differentiator vs NRI-style external hooks: policies are Wasm
//! modules evaluated **inside** the runtime process with microsecond latency,
//! no sidecar, no IPC. A policy sees a JSON description of the request and
//! answers allow/deny (+ reason). Modules hot-reload on file change.
//!
//! # Policy ABI (wasm32-unknown-unknown core module)
//!
//! Required exports:
//! - `memory`
//! - `alloc(len: i32) -> ptr`              — reserve `len` bytes for payload
//! - `evaluate(ptr: i32, len: i32) -> i32` — 0 = allow, non-zero = deny
//!
//! Optional exports (queried after a deny):
//! - `reason_ptr() -> i32`, `reason_len() -> i32` — UTF-8 denial reason

use crate::core::{Error, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use wasmtime::{Engine, Module};

/// Request context handed to the policy as JSON.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PolicyRequest<'a> {
    /// Hook name: "run_pod_sandbox" | "create_container" | "start_container".
    pub hook: &'a str,
    pub pod_id: &'a str,
    pub container_id: &'a str,
    pub runtime_handler: &'a str,
    pub image: &'a str,
    pub labels: &'a HashMap<String, String>,
    pub annotations: &'a HashMap<String, String>,
    pub args: &'a [String],
}

struct CompiledPolicy {
    mtime: SystemTime,
    len: u64,
    module: Module,
}

pub struct PolicyEngine {
    enabled: bool,
    path: PathBuf,
    hooks: Vec<String>,
    fail_closed: bool,
    fuel_limit: u64,
    timeout: std::time::Duration,
    engine: Engine,
    cache: Mutex<Option<CompiledPolicy>>,
}

/// Outcome of one evaluation.
enum Verdict {
    Allow,
    Deny(String),
}

impl PolicyEngine {
    pub fn new(config: &crate::core::config::Config) -> Self {
        let cfg = &config.policy;
        // Fuel accounting makes every evaluation finitely terminating even if
        // a policy contains an infinite loop.
        let mut wcfg = wasmtime::Config::new();
        wcfg.consume_fuel(true);
        let engine = match Engine::new(&wcfg) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "fuel-configured engine failed; falling back");
                Engine::default()
            }
        };
        Self {
            enabled: cfg.enabled && !cfg.path.is_empty(),
            path: PathBuf::from(&cfg.path),
            hooks: cfg.hooks.clone(),
            fail_closed: cfg.fail_closed,
            fuel_limit: cfg.fuel_limit,
            timeout: std::time::Duration::from_millis(cfg.timeout_ms.max(1)),
            engine,
            cache: Mutex::new(None),
        }
    }

    /// Is this hook active?
    pub fn hook_active(&self, hook: &str) -> bool {
        self.enabled && self.hooks.iter().any(|h| h == hook)
    }

    /// Evaluate the policy; Ok(()) allows, Err(Cri("denied by policy: …")) denies.
    async fn evaluate(&self, req: &PolicyRequest<'_>) -> Result<()> {
        if !self.hook_active(req.hook) {
            return Ok(());
        }

        // 1. Get an up-to-date compiled module (stat is cheap; compile only
        //    on first use or file change → hot reload).
        let module = self.load_module().await?;

        // 2. Instantiate per evaluation. Instance/Func live in a Store in
        //    wasmtime; tiny policies instantiate in tens of microseconds.
        let payload = serde_json::to_vec(req)?;
        let eval_future = async {
            let mut store = wasmtime::Store::new(&self.engine, ());
            store
                .set_fuel(self.fuel_limit)
                .map_err(|e| Error::Cri(format!("policy fuel setup: {e}")))?;
            let linker = wasmtime::Linker::<()>::new(&self.engine);
            let instance = linker
                .instantiate(&mut store, &module)
                .map_err(|e| Error::Cri(format!("instantiate policy: {e}")))?;
            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| Error::Cri("policy has no exported memory".into()))?;
            let alloc = instance
                .get_typed_func::<i32, i32>(&mut store, "alloc")
                .map_err(|e| Error::Cri(format!("policy export alloc: {e}")))?;
            let evaluate = instance
                .get_typed_func::<(i32, i32), i32>(&mut store, "evaluate")
                .map_err(|e| Error::Cri(format!("policy export evaluate: {e}")))?;

            let ptr = alloc
                .call(&mut store, payload.len() as i32)
                .map_err(|e| Error::Cri(format!("policy alloc trap: {e}")))?;
            memory
                .write(&mut store, ptr as usize, &payload)
                .map_err(|e| Error::Cri(format!("policy write payload: {e}")))?;
            let deny_code = evaluate
                .call(&mut store, (ptr, payload.len() as i32))
                .map_err(|e| Error::Cri(format!("policy evaluate error (trap or fuel exhausted): {e}")))?;

            if deny_code == 0 {
                return Ok(Verdict::Allow);
            } else {
                let mut reason: Option<String> = None;
                if let (Ok(rp), Ok(rl)) = (
                    instance.get_typed_func::<(), i32>(&mut store, "reason_ptr"),
                    instance.get_typed_func::<(), i32>(&mut store, "reason_len"),
                ) {
                    if let (Ok(p), Ok(l)) = (rp.call(&mut store, ()), rl.call(&mut store, ())) {
                        if p > 0 && l > 0 {
                            let mut buf = vec![0u8; l as usize];
                            if memory.read(&store, p as usize, &mut buf).is_ok() {
                                reason = Some(
                                    String::from_utf8_lossy(&buf)
                                        .trim_end_matches('\0')
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
                let reason = reason.unwrap_or_else(|| "no reason given".to_string());
                Ok(Verdict::Deny(reason))
            }
        };
        // Wall-clock backstop (fuel is the hard guarantee; this covers
        // pathological instantiation stalls). On timeout we report an engine
        // failure so fail-open/fail-closed policy applies.
        let verdict = match tokio::time::timeout(self.timeout, eval_future).await {
            Ok(v) => v,
            Err(_) => Err(Error::Cri("policy evaluation timed out".into())),
        };

        let verdict = verdict?;
        match verdict {
            Verdict::Allow => {
                tracing::debug!(hook = %req.hook, "policy allowed");
                Ok(())
            }
            Verdict::Deny(reason) => {
                tracing::info!(hook = %req.hook, %reason, "policy denied");
                Err(Error::Cri(format!("denied by policy: {reason}")))
            }
        }
    }

    /// Compile-on-demand with mtime+size based hot reload.
    async fn load_module(&self) -> Result<Module> {
        let meta = tokio::fs::metadata(&self.path).await.map_err(|e| {
            Error::Cri(format!("policy module {}: {e}", self.path.display()))
        })?;
        let mtime = meta
            .modified()
            .map_err(|e| Error::Cri(format!("policy mtime: {e}")))?;

        {
            let guard = self.cache.lock().unwrap();
            if let Some(c) = guard.as_ref() {
                if c.mtime == mtime && c.len == meta.len() {
                    return Ok(c.module.clone());
                }
            }
        }

        let wasm = tokio::fs::read(&self.path)
            .await
            .map_err(|e| Error::Cri(format!("read policy {}: {e}", self.path.display())))?;
        let module = Module::from_binary(&self.engine, &wasm)
            .map_err(|e| Error::Cri(format!("compile policy: {e}")))?;
        {
            let mut guard = self.cache.lock().unwrap();
            *guard = Some(CompiledPolicy {
                mtime,
                len: meta.len(),
                module: module.clone(),
            });
        }
        tracing::info!(path = %self.path.display(), "policy module loaded");
        Ok(module)
    }

    /// Public entry used by call sites: fail-open on engine errors unless
    /// `fail_closed`; genuine denials always propagate.
    pub async fn enforce(&self, req: &PolicyRequest<'_>) -> Result<()> {
        match self.evaluate(req).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let denied =
                    matches!(&e, Error::Cri(m) if m.starts_with("denied by policy"));
                if denied {
                    Err(e)
                } else if self.fail_closed {
                    Err(Error::Cri(format!(
                        "policy engine failure (fail-closed): {e}"
                    )))
                } else {
                    tracing::warn!(error = %e, "policy engine error; failing open");
                    Ok(())
                }
            }
        }
    }
}
