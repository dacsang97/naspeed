//! Contract tests for the frozen BenchmarkRun JSON schema (schema_version "1")
//! and for the ns_project fixture edit markers used by benchmark workloads.

use serde_json::{json, Map, Value};
use std::path::PathBuf;

const METRIC_KEYS: [&str; 10] = [
    "wall_s",
    "cpu_s",
    "peak_rss_bytes",
    "network_bytes",
    "logical_bytes",
    "allocated_physical_bytes",
    "cache_hits",
    "cache_misses",
    "targets_restarted",
    "targets_tested",
];

const TOP_LEVEL_KEYS: [&str; 12] = [
    "schema_version",
    "scenario",
    "repository",
    "environment",
    "cache_state",
    "command",
    "samples",
    "median",
    "output_digest",
    "test_summary",
    "logical_bytes",
    "allocated_bytes",
];

fn metrics_map(wall_s: f64) -> Map<String, Value> {
    let mut m = Map::new();
    for key in METRIC_KEYS {
        m.insert(key.to_string(), json!(wall_s));
    }
    m
}

fn sample(index: u64, warmup: bool, valid: bool, wall_s: f64) -> Value {
    json!({
        "index": index,
        "warmup": warmup,
        "valid": valid,
        "invalid_reason": Value::Null,
        "metrics": Value::Object(metrics_map(wall_s)),
    })
}

fn representative_run(samples: Vec<Value>, median: Value) -> Value {
    json!({
        "schema_version": "1",
        "scenario": "incremental-leaf-edit",
        "repository": {
            "path": "/tmp/ns_project",
            "revision": "abc123",
            "dirty": false,
        },
        "environment": {
            "os": "macos",
            "os_version": "23.6.0",
            "arch": "arm64",
            "filesystem": "apfs",
            "toolchains": { "cargo": "1.x" },
            "env_allowlist": ["PATH"],
        },
        "cache_state": { "kind": "cold", "detail": "fresh checkout" },
        "command": ["ns", "build"],
        "samples": samples,
        "median": median,
        "output_digest": "blake3:deadbeef",
        "test_summary": { "status": "pass", "digest": "blake3:cafe" },
        "logical_bytes": 1024,
        "allocated_bytes": 2048,
        "canonical_objects": 7,
    })
}

/// Returns Err(reason) when `run` violates the BenchmarkRun contract.
fn validate_run(run: &Value) -> Result<(), String> {
    let obj = run
        .as_object()
        .ok_or_else(|| "run is not an object".to_string())?;

    if obj.get("schema_version") != Some(&json!("1")) {
        return Err("schema_version must be \"1\"".to_string());
    }

    for key in TOP_LEVEL_KEYS {
        if !obj.contains_key(key) {
            return Err(format!("missing top-level key: {key}"));
        }
    }

    let check_metrics = |metrics: &Value, where_: &str| -> Result<(), String> {
        let mobj = metrics
            .as_object()
            .ok_or_else(|| format!("{where_}: metrics is not an object"))?;
        for key in METRIC_KEYS {
            let v = mobj
                .get(key)
                .ok_or_else(|| format!("{where_}: missing metric key: {key}"))?;
            if v.is_number() {
                continue;
            }
            let marker = v
                .as_object()
                .ok_or_else(|| format!("{where_}: metric {key} is neither number nor object"))?;
            if marker.get("status") != Some(&json!("unsupported")) {
                return Err(format!("{where_}: metric {key} object is not an unsupported marker"));
            }
            match marker.get("reason").and_then(Value::as_str) {
                Some(r) if !r.is_empty() => {}
                _ => {
                    return Err(format!(
                        "{where_}: metric {key} unsupported marker has empty/missing reason"
                    ))
                }
            }
        }
        Ok(())
    };

    let samples = obj["samples"]
        .as_array()
        .ok_or_else(|| "samples is not an array".to_string())?;
    for (i, s) in samples.iter().enumerate() {
        check_metrics(&s["metrics"], &format!("samples[{i}]"))?;
    }
    check_metrics(&obj["median"], "median")?;

    Ok(())
}

/// Median of each metric over valid, non-warmup samples. Numeric values only;
/// a metric with no numeric observations yields None for that key.
fn median_metrics(samples: &[Value]) -> Map<String, Value> {
    let mut out = Map::new();
    for key in METRIC_KEYS {
        let mut values: Vec<f64> = samples
            .iter()
            .filter(|s| {
                s["valid"].as_bool().unwrap_or(false) && !s["warmup"].as_bool().unwrap_or(false)
            })
            .filter_map(|s| s["metrics"][key].as_f64())
            .collect();
        if values.is_empty() {
            out.insert(
                key.to_string(),
                json!({"status": "unsupported", "reason": "no numeric observations"}),
            );
            continue;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = values.len();
        let med = if n % 2 == 1 {
            values[n / 2]
        } else {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        };
        out.insert(key.to_string(), json!(med));
    }
    out
}

#[test]
fn representative_run_validates() {
    let samples = vec![
        sample(0, true, true, 5.0),
        sample(1, false, true, 1.0),
        sample(2, false, true, 2.0),
        sample(3, false, true, 3.0),
    ];
    let median = Value::Object(median_metrics(&samples));
    let run = representative_run(samples, median);
    validate_run(&run).expect("representative run should validate");
}

#[test]
fn unsupported_metric_marker_validates() {
    let mut m = metrics_map(1.0);
    m.insert(
        "network_bytes".to_string(),
        json!({"status": "unsupported", "reason": "no network accounting on this host"}),
    );
    let s = json!({
        "index": 0,
        "warmup": false,
        "valid": true,
        "invalid_reason": Value::Null,
        "metrics": Value::Object(m),
    });
    let run = representative_run(vec![s.clone()], Value::Object(median_metrics(&[s])));
    validate_run(&run).expect("unsupported marker with reason should validate");
}

#[test]
fn missing_metric_key_rejected() {
    let mut m = metrics_map(1.0);
    m.remove("cache_hits");
    let s = json!({
        "index": 0, "warmup": false, "valid": true,
        "invalid_reason": Value::Null,
        "metrics": Value::Object(m),
    });
    let run = representative_run(vec![s.clone()], s["metrics"].clone());
    let err = validate_run(&run).unwrap_err();
    assert!(err.contains("cache_hits"), "unexpected error: {err}");
}

#[test]
fn unsupported_marker_empty_reason_rejected() {
    let mut m = metrics_map(1.0);
    m.insert(
        "network_bytes".to_string(),
        json!({"status": "unsupported", "reason": ""}),
    );
    let s = json!({
        "index": 0, "warmup": false, "valid": true,
        "invalid_reason": Value::Null,
        "metrics": Value::Object(m),
    });
    let run = representative_run(vec![s], Value::Object(metrics_map(1.0)));
    let err = validate_run(&run).unwrap_err();
    assert!(err.contains("reason"), "unexpected error: {err}");
}

#[test]
fn wrong_schema_version_rejected() {
    let mut run = representative_run(vec![], Value::Object(Map::new()));
    run["schema_version"] = json!("2");
    assert!(validate_run(&run).is_err());
    run["schema_version"] = json!(1);
    assert!(validate_run(&run).is_err());
}

#[test]
fn missing_top_level_key_rejected() {
    let mut run = representative_run(vec![], Value::Object(Map::new()));
    run.as_object_mut().unwrap().remove("output_digest");
    let err = validate_run(&run).unwrap_err();
    assert!(err.contains("output_digest"), "unexpected error: {err}");
}

#[test]
fn median_uses_valid_non_warmup_samples() {
    // 5 samples: 1 warmup + 1 invalid; median over the 3 valid non-warmup.
    let samples = vec![
        sample(0, true, true, 99.0),   // warmup: excluded
        sample(1, false, true, 4.0),   // valid
        sample(2, false, false, 0.01), // invalid: excluded
        sample(3, false, true, 2.0),   // valid
        sample(4, false, true, 8.0),   // valid
    ];
    let median = median_metrics(&samples);
    // Valid non-warmup wall_s values: {4.0, 2.0, 8.0} → median 4.0.
    assert_eq!(median["wall_s"], json!(4.0));
    assert_eq!(median["targets_tested"], json!(4.0));
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project")
}

#[test]
fn fixture_leaf_marker_present() {
    let lib = std::fs::read_to_string(fixture_root().join("crates/greeter/src/lib.rs"))
        .expect("read greeter lib.rs");
    assert!(lib.contains("// benchmark-leaf-edit-marker:"));
    assert!(lib.contains("pub fn greeting()"));
}

#[test]
fn fixture_core_marker_present() {
    let toml_src = std::fs::read_to_string(fixture_root().join("Cargo.toml"))
        .expect("read workspace Cargo.toml");
    assert!(
        toml_src.contains("# benchmark-core-edit-marker")
            || toml_src.contains("benchmark-core-edit-marker")
    );
    assert!(toml_src.contains("members"));
}

#[test]
fn fixture_hello_server_depends_on_greeter_by_path() {
    let toml_src = std::fs::read_to_string(
        fixture_root().join("apps/hello-server/Cargo.toml"),
    )
    .expect("read hello-server Cargo.toml");
    assert!(toml_src.contains("greeter"));
    assert!(toml_src.contains("path"));
    assert!(toml_src.contains("crates/greeter"));
}
