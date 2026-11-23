use llmux_config::RuntimeConfig;

#[tokio::test]
async fn freebuff_deployment_initializes() {
    let secrets = tempfile::tempdir().unwrap();
    let secret = secrets.path().join("key");
    std::fs::write(&secret, "synthetic-freebuff").unwrap();
    let config = RuntimeConfig::parse(&format!(
        r#"
[server]
client_keys = ["file:{secret}"]

[[credentials]]
id = "freebuff"
kind = "api_key"
provider = "freebuff"
key = "file:{secret}"

[[models]]
name = "freebuff/deepseek-v4-flash"
[[models.deployments]]
id = "freebuff"
credential = "freebuff"
base_url = "https://www.codebuff.com/api/v1"
upstream_model = "deepseek/deepseek-v4-flash"
"#,
        secret = secret.display(),
    ))
    .unwrap();
    let _app = llmux_server::app(config).await.unwrap();
}
