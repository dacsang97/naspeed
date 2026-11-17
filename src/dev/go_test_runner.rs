//! `go test -json` command construction and test2json summaries.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct GoTestSummary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

#[derive(Deserialize)]
struct TestEvent {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package", default)]
    package: String,
    #[serde(rename = "Test")]
    test: Option<String>,
    #[serde(rename = "Output")]
    output: Option<String>,
}

pub fn command(root: &Path, packages: &BTreeSet<String>) -> Command {
    let mut command = Command::new("go");
    command
        .args(["test", "-json"])
        .args(packages)
        .current_dir(root);
    command
}

pub fn summarize(output: &str) -> GoTestSummary {
    let mut summary = GoTestSummary::default();
    let mut packages_with_failed_tests = HashSet::new();
    let mut pending_output = Vec::new();
    for line in output.lines() {
        let Ok(event) = serde_json::from_str::<TestEvent>(line) else {
            if !line.trim().is_empty() {
                pending_output.push(line.to_string());
            }
            continue;
        };
        if let Some(text) = event.output {
            let text = text.trim_end();
            if !text.is_empty() {
                pending_output.push(text.to_string());
            }
        }
        match (event.action.as_str(), event.test) {
            ("pass", Some(_)) => summary.passed += 1,
            ("skip", Some(_)) => summary.skipped += 1,
            ("fail", test) => {
                if test.is_none() && packages_with_failed_tests.contains(&event.package) {
                    continue;
                }
                summary.failed += 1;
                let label = match test {
                    Some(test) if event.package.is_empty() => test,
                    Some(test) => {
                        packages_with_failed_tests.insert(event.package.clone());
                        format!("{}::{test}", event.package)
                    }
                    None if event.package.is_empty() => "Go test package".to_string(),
                    None => event.package,
                };
                let detail = pending_output[pending_output.len().saturating_sub(20)..].join("\n");
                summary.failures.push(if detail.is_empty() {
                    label
                } else {
                    format!("{label}\n{detail}")
                });
            }
            _ => {}
        }
    }
    summary
}

pub fn readable_output(output: &str) -> String {
    output
        .lines()
        .filter_map(|line| match serde_json::from_str::<TestEvent>(line) {
            Ok(event) => event.output,
            Err(_) => Some(line.to_string()),
        })
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_test2json_events() {
        let output = concat!(
            "{\"Action\":\"pass\",\"Package\":\"example/a\",\"Test\":\"TestOK\"}\n",
            "{\"Action\":\"output\",\"Package\":\"example/a\",\"Test\":\"TestBad\",\"Output\":\"want 1 got 2\\n\"}\n",
            "{\"Action\":\"fail\",\"Package\":\"example/a\",\"Test\":\"TestBad\"}\n",
            "{\"Action\":\"skip\",\"Package\":\"example/a\",\"Test\":\"TestSkip\"}\n"
        );
        let summary = summarize(output);
        assert_eq!((summary.passed, summary.failed, summary.skipped), (1, 1, 1));
        assert!(summary.failures[0].contains("example/a::TestBad"));
        assert!(summary.failures[0].contains("want 1 got 2"));
    }

    #[test]
    fn reports_package_build_failures_without_raw_json() {
        let output = concat!(
            "{\"Action\":\"output\",\"Package\":\"example/a\",\"Output\":\"./a_test.go:4: undefined: missing\\n\"}\n",
            "{\"Action\":\"fail\",\"Package\":\"example/a\",\"Elapsed\":0}\n"
        );
        let summary = summarize(output);
        assert_eq!(summary.failed, 1);
        assert!(summary.failures[0].contains("example/a"));
        assert!(summary.failures[0].contains("undefined: missing"));
        assert_eq!(readable_output(output), "./a_test.go:4: undefined: missing");
    }
}
