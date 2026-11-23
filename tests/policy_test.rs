// P1b: policy engine sequencing — allow → deny → allow must never wedge.
#[tokio::test]
async fn policy_deny_then_allow() {
    let mut cfg = nautilus::Config::default();
    cfg.policy.enabled = true;
    cfg.policy.fail_closed = true;
    // Locate the demo-policy module relative to the crate, not a hardcoded
    // home directory; it must exist or the engine would silently fail open.
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/policy-deny-label/target/wasm32-unknown-unknown/release/policy_deny_label.wasm"
    );
    assert!(
        std::path::Path::new(wasm_path).exists(),
        "demo policy wasm missing at {wasm_path}; run: cargo build --manifest-path examples/policy-deny-label/Cargo.toml --target wasm32-unknown-unknown --release"
    );
    cfg.policy.path = wasm_path.into();

    let engine = nautilus::policy::PolicyEngine::new(&cfg);
    assert!(engine.hook_active("create_container"));

    let mut labels_allow = std::collections::HashMap::new();
    labels_allow.insert("team".to_string(), "edge".to_string());
    let mut labels_deny = std::collections::HashMap::new();
    labels_deny.insert("nautilus.policy/deny".to_string(), "true".to_string());
    let empty = std::collections::HashMap::new();
    // This text is an argument, so a label-only policy must allow it.
    let args = vec![r#"quoted "nautilus.policy/deny""#.to_string()];

    for round in 0..3 {
        eprintln!("round {round}: allow begin");
        // allow
        let r = engine
            .enforce(&nautilus::policy::PolicyRequest {
                hook: "create_container",
                pod_id: "p1",
                container_id: &format!("c{round}"),
                runtime_handler: "runc",
                image: "busybox",
                labels: &labels_allow,
                annotations: &empty,
                args: &args,
            })
            .await;
        assert!(r.is_ok(), "round {round}: allow failed: {r:?}");
        eprintln!("round {round}: deny begin");

        // deny
        let r = engine
            .enforce(&nautilus::policy::PolicyRequest {
                hook: "create_container",
                pod_id: "p1",
                container_id: &format!("c{round}d"),
                runtime_handler: "runc",
                image: "busybox",
                labels: &labels_deny,
                annotations: &empty,
                args: &args,
            })
            .await;
        match r {
            Err(nautilus::Error::Cri(m)) => assert!(m.starts_with("denied by policy"), "{m}"),
            other => panic!("round {round}: expected denial, got {other:?}"),
        }
        eprintln!("round {round}: done");
    }
}
