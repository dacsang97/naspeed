use crate::project::NsConfig;

use super::*;

const BASE_CONFIG: &str = r#"{
  "architecture": "amd64", "os": "linux", "created": "1970-01-01T00:00:00Z",
  "config": {"Env": ["PATH=/usr/bin", "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt"],
             "User": "0", "Cmd": ["/bin/sh"], "WorkingDir": "/",
             "ExposedPorts": {"9000/tcp": {}}},
  "rootfs": {"type": "layers", "diff_ids": ["sha256:aaaa", "sha256:bbbb"]},
  "history": [{"created_by": "a"}, {"created_by": "b"}]
}"#;

fn amd64() -> Platform {
    Platform::parse("linux/amd64").unwrap()
}

fn runtime(image_toml: &str) -> Result<RuntimeConfig> {
    let cfg = NsConfig::parse(&format!("[image]\n{image_toml}")).unwrap();
    RuntimeConfig::new(&cfg.image, "myapp")
}

fn derive(rt: &RuntimeConfig) -> Value {
    let labels = BTreeMap::from([(
        "org.opencontainers.image.version".to_string(),
        "0.3.1".to_string(),
    )]);
    let changes = ConfigChanges {
        diff_id: "sha256:cccc",
        runtime: rt,
        labels: &labels,
        created_epoch: 1_700_000_000,
    };
    serde_json::from_slice(&derive_config(BASE_CONFIG.as_bytes(), &amd64(), &changes).unwrap())
        .unwrap()
}

#[test]
fn defaults_put_binary_on_path_as_nonroot_without_cmd() {
    let rt = runtime("").unwrap();
    assert_eq!(rt.path, "/usr/bin/myapp");
    let out = derive(&rt);
    let cfg = &out["config"];
    assert_eq!(cfg["Entrypoint"], json!(["/usr/bin/myapp"]));
    assert!(cfg.get("Cmd").is_none(), "base Cmd dropped: {cfg}");
    assert_eq!(cfg["User"], json!("65532:65532"));
    assert_eq!(cfg["WorkingDir"], json!("/"), "base WorkingDir kept");
    assert_eq!(
        cfg["Env"],
        json!([
            "PATH=/usr/bin",
            "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt"
        ])
    );
    assert_eq!(cfg["ExposedPorts"], json!({"9000/tcp": {}}));
    assert!(cfg.get("Volumes").is_none());
    assert_eq!(
        cfg["Labels"]["org.opencontainers.image.version"],
        json!("0.3.1")
    );
    assert_eq!(
        out["rootfs"]["diff_ids"],
        json!(["sha256:aaaa", "sha256:bbbb", "sha256:cccc"])
    );
    assert_eq!(out["created"], json!("2023-11-14T22:13:20Z"));
    assert_eq!(out["history"].as_array().unwrap().len(), 3);

    let arm = Platform::parse("linux/arm64").unwrap();
    let labels = BTreeMap::new();
    let changes = ConfigChanges {
        diff_id: "sha256:cccc",
        runtime: &rt,
        labels: &labels,
        created_epoch: 0,
    };
    assert!(derive_config(BASE_CONFIG.as_bytes(), &arm, &changes).is_err());
}

#[test]
fn dockerfile_style_settings_reach_the_config() {
    let rt = runtime(
        r#"
path = "/app/{binary}"
cmd = ["serve", "-config", "/data/config.yaml"]
user = "nonroot:nonroot"
workdir = "/data"
env = ["PATH=/app:/usr/bin", "RUST_LOG=info"]
ports = ["8317", "53/udp"]
volumes = ["/data"]
"#,
    )
    .unwrap();
    let out = derive(&rt);
    let cfg = &out["config"];
    assert_eq!(cfg["Entrypoint"], json!(["/app/myapp"]));
    assert_eq!(cfg["Cmd"], json!(["serve", "-config", "/data/config.yaml"]));
    assert_eq!(cfg["User"], json!("nonroot:nonroot"));
    assert_eq!(cfg["WorkingDir"], json!("/data"));
    // Same key replaced in place, new keys appended.
    assert_eq!(
        cfg["Env"],
        json!([
            "PATH=/app:/usr/bin",
            "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
            "RUST_LOG=info"
        ])
    );
    assert_eq!(
        cfg["ExposedPorts"],
        json!({"9000/tcp": {}, "8317/tcp": {}, "53/udp": {}})
    );
    assert_eq!(cfg["Volumes"], json!({"/data": {}}));
}

#[test]
fn explicit_entrypoint_and_empty_user() {
    let rt = runtime("entrypoint = [\"/usr/bin/tini\", \"--\", \"/usr/bin/myapp\"]\nuser = \"\"\n")
        .unwrap();
    let cfg = derive(&rt)["config"].clone();
    assert_eq!(
        cfg["Entrypoint"],
        json!(["/usr/bin/tini", "--", "/usr/bin/myapp"])
    );
    assert!(cfg.get("User").is_none());
}

#[test]
fn invalid_settings_are_rejected() {
    for bad in [
        "path = \"usr/bin/{binary}\"",
        "path = \"/usr/bin/{bin}\"",
        "path = \"/usr/bin/\"",
        "workdir = \"data\"",
        "env = [\"NOVALUE\"]",
        "env = [\"=x\"]",
        "ports = [\"http\"]",
        "ports = [\"0\"]",
        "ports = [\"80/icmp\"]",
        "volumes = [\"data\"]",
    ] {
        assert!(runtime(bad).is_err(), "{bad}");
    }
}
