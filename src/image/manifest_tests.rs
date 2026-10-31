use serde_json::json;

use super::*;

#[test]
fn manifest_and_index_shape_and_media_types() {
    let config = Descriptor::new(OCI_CONFIG, "sha256:c0", 10);
    let layers = vec![
        Descriptor::new(OCI_LAYER_GZIP, "sha256:l1", 100),
        Descriptor::new(OCI_LAYER_GZIP, "sha256:l2", 200),
    ];
    let manifest: Value = serde_json::from_slice(&image_manifest(config, layers).unwrap()).unwrap();
    assert_eq!(
        manifest,
        json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {"mediaType": OCI_CONFIG, "digest": "sha256:c0", "size": 10},
            "layers": [
                {"mediaType": OCI_LAYER_GZIP, "digest": "sha256:l1", "size": 100},
                {"mediaType": OCI_LAYER_GZIP, "digest": "sha256:l2", "size": 200}
            ]
        })
    );
    let mut entry = Descriptor::new(OCI_MANIFEST, "sha256:m1", 300);
    entry.platform = Some(PlatformSpec {
        architecture: "arm64".into(),
        os: "linux".into(),
        variant: None,
    });
    let index: Value = serde_json::from_slice(&image_index(vec![entry]).unwrap()).unwrap();
    assert_eq!(
        index,
        json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [{
                "mediaType": OCI_MANIFEST, "digest": "sha256:m1", "size": 300,
                "platform": {"architecture": "arm64", "os": "linux"}
            }]
        })
    );
}

#[test]
fn accepts_docker_v2_base_and_relabels_layers() {
    let list = json!({
        "schemaVersion": 2, "mediaType": DOCKER_LIST,
        "manifests": [
            {"mediaType": DOCKER_MANIFEST, "digest": "sha256:amd", "size": 1,
             "platform": {"architecture": "amd64", "os": "linux"}},
            {"mediaType": DOCKER_MANIFEST, "digest": "sha256:arm", "size": 1,
             "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}}
        ]
    });
    let BaseManifest::Index(index) = parse_base(list.to_string().as_bytes(), "").unwrap() else {
        panic!("expected an index");
    };
    let arm = Platform::parse("linux/arm64").unwrap();
    assert_eq!(select_platform(&index, &arm).unwrap(), "sha256:arm");

    let image = json!({
        "schemaVersion": 2, "mediaType": DOCKER_MANIFEST,
        "config": {"mediaType": DOCKER_CONFIG, "digest": "sha256:cfg", "size": 5},
        "layers": [{"mediaType": DOCKER_LAYER_GZIP, "digest": "sha256:l1", "size": 7}]
    });
    let BaseManifest::Image(image) = parse_base(image.to_string().as_bytes(), "").unwrap() else {
        panic!("expected an image manifest");
    };
    let layers = oci_layers(&image).unwrap();
    assert_eq!(layers[0].media_type, OCI_LAYER_GZIP);
    assert_eq!(
        (layers[0].digest.as_str(), layers[0].size),
        ("sha256:l1", 7)
    );
}

#[test]
fn rfc3339_dates() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
}
