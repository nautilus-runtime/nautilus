use std::process::Command;

#[test]
fn shipped_configuration_matches_runtime_defaults() {
    let sample: toml::Value = toml::from_str(include_str!("../nautilus.toml")).unwrap();
    assert_eq!(
        sample,
        toml::Value::try_from(nautilus::Config::default()).unwrap()
    );
}

#[test]
fn explicit_missing_or_invalid_configuration_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let socket = dir.path().join("runtime.sock");
    for contents in [None, Some("invalid [")] {
        if let Some(value) = contents {
            std::fs::write(&config, value).unwrap();
        }
        let result = Command::new(env!("CARGO_BIN_EXE_nautilusd"))
            .arg("--config")
            .arg(&config)
            .arg("--socket")
            .arg(&socket)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(config.to_str().unwrap()));
        assert!(!socket.exists());
    }
}
