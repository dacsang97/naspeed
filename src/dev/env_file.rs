//! `.env` parsing for the dev process: `KEY=VALUE` lines, `#` comments,
//! optional `export ` prefix and single/double quotes around the value.
//! Read again before every start, so editing it only needs a restart.

use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn parse(text: &str) -> Result<Vec<(String, String)>> {
    let mut vars = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            bail!("line {}: expected KEY=VALUE, got {raw:?}", i + 1);
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("line {}: invalid variable name {key:?}", i + 1);
        }
        let value = value.trim();
        let unquoted = [('"', '"'), ('\'', '\'')]
            .iter()
            .find_map(|(open, close)| value.strip_prefix(*open)?.strip_suffix(*close))
            .unwrap_or(value);
        vars.push((key.to_string(), unquoted.to_string()));
    }
    Ok(vars)
}

/// Variables from `path`; a missing file means none (it may be created later).
pub fn load(path: &Path) -> Result<Vec<(String, String)>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        let vars = parse("# c\n\nA=1\nexport B = two words \nC=\"q=x\"\nD='s'\nE=\n").unwrap();
        let want = [
            ("A", "1"),
            ("B", "two words"),
            ("C", "q=x"),
            ("D", "s"),
            ("E", ""),
        ];
        assert_eq!(vars, want.map(|(k, v)| (k.to_string(), v.to_string())));
    }

    #[test]
    fn rejects_garbage_with_line_number() {
        let err = parse("A=1\nnot a pair\n").unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        assert!(parse("BAD KEY=1").is_err());
    }
}
