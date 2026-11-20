use std::process::Command;

#[test]
fn check_validates_without_binding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "invalid").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-server"))
        .arg("--check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid configuration"));
    let key = dir.path().join("key");
    std::fs::write(&key, "synthetic-key").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        &path,
        format!(
            r#"
[server]
listen = "{address}"
client_keys = ["file:{}"]
[[credentials]]
id = "test"
kind = "api_key"
provider = "openai"
key = "file:{}"
[[models]]
name = "test"
[[models.deployments]]
id = "test"
credential = "test"
base_url = "http://127.0.0.1:1"
upstream_model = "test"
"#,
            key.display(),
            key.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-server"))
        .arg("--check")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        address.to_string()
    );
}
