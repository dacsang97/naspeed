//! Replace machine- and target-dir-specific absolute paths with stable
//! tokens, so keys, `.d` files and rustc's JSON messages stored from one
//! `target/` can be replayed into another.

use std::path::{Path, PathBuf};

pub const PROFILE_TOKEN: &str = "@NASPEED_PROFILE_DIR@";
pub const CARGO_HOME_TOKEN: &str = "@NASPEED_CARGO_HOME@";
/// The workspace root (rustc's cwd for workspace crates).
pub const WORKSPACE_TOKEN: &str = "@NASPEED_WORKSPACE@";

#[derive(Debug, Clone)]
pub struct Placeholders {
    /// (absolute path as text, token); longest path first so nested paths
    /// are replaced before their parents.
    pairs: Vec<(String, &'static str)>,
    /// As-given paths, used when decoding.
    profile: String,
    cargo_home: String,
    workspace: Option<String>,
}

fn variants(path: &Path) -> Vec<String> {
    let mut out = vec![path.to_string_lossy().trim_end_matches('/').to_owned()];
    if let Ok(canonical) = std::fs::canonicalize(path) {
        let canonical = canonical.to_string_lossy().trim_end_matches('/').to_owned();
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out.retain(|p| !p.is_empty() && p != "/");
    out
}

impl Placeholders {
    /// `profile_dir` is the parent of rustc's `--out-dir` (`target/debug`,
    /// `target/<triple>/release`, …): it covers both `deps/` and the build
    /// script `OUT_DIR`s under `build/`.
    pub fn new(profile_dir: &Path, cargo_home: &Path) -> Placeholders {
        let mut pairs = Vec::new();
        for p in variants(profile_dir) {
            pairs.push((p, PROFILE_TOKEN));
        }
        for p in variants(cargo_home) {
            pairs.push((p, CARGO_HOME_TOKEN));
        }
        pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
        Placeholders {
            pairs,
            profile: profile_dir
                .to_string_lossy()
                .trim_end_matches('/')
                .to_owned(),
            cargo_home: cargo_home
                .to_string_lossy()
                .trim_end_matches('/')
                .to_owned(),
            workspace: None,
        }
    }

    /// Also map the workspace root, for workspace crates. The profile dir
    /// usually lies inside it; longest-first replacement keeps it distinct.
    pub fn with_workspace(mut self, root: &Path) -> Placeholders {
        for p in variants(root) {
            self.pairs.push((p, WORKSPACE_TOKEN));
        }
        self.pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
        self.workspace = Some(root.to_string_lossy().trim_end_matches('/').to_owned());
        self
    }

    /// Absolute paths → tokens. Also rewrites the JSON-escaped form so
    /// paths inside rustc's `--error-format=json` messages are covered.
    pub fn encode(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (path, token) in &self.pairs {
            out = replace_path(&out, path, token);
            let escaped = json_escape(path);
            if escaped != *path {
                out = replace_path(&out, &escaped, token);
            }
        }
        out
    }

    /// Tokens → this target dir's paths (first, i.e. as-given, variant).
    pub fn decode(&self, text: &str) -> String {
        self.decode_with(text, str::to_owned)
    }

    /// `decode` for a rustc dep-info file: `# env-dep:NAME=value` lines hold
    /// escaped values (`\\` for a backslash), so a Windows path must go back
    /// escaped there, or cargo rejects the file as unparseable.
    pub fn decode_dep_info(&self, text: &str) -> String {
        text.split_inclusive('\n')
            .map(|line| {
                if line.starts_with("# env-dep:") {
                    self.decode_with(line, |p| p.replace('\\', "\\\\").replace('\n', "\\n"))
                } else {
                    self.decode(line)
                }
            })
            .collect()
    }

    fn decode_with(&self, text: &str, escape: impl Fn(&str) -> String) -> String {
        let mut out = text.to_owned();
        for token in [PROFILE_TOKEN, CARGO_HOME_TOKEN, WORKSPACE_TOKEN] {
            if let Some(path) = self.primary(token) {
                out = out.replace(token, &escape(path));
            }
        }
        out
    }

    /// `decode` for text inside JSON string literals.
    pub fn decode_json(&self, text: &str) -> String {
        self.decode_with(text, json_escape)
    }

    fn primary(&self, token: &str) -> Option<&str> {
        match token {
            PROFILE_TOKEN => Some(self.profile.as_str()),
            CARGO_HOME_TOKEN => Some(self.cargo_home.as_str()),
            WORKSPACE_TOKEN => self.workspace.as_deref(),
            _ => None,
        }
    }

    pub fn profile_dir(&self) -> PathBuf {
        PathBuf::from(&self.profile)
    }
}

/// Replace `path` only where it is a whole path prefix (followed by `/`,
/// end, or a non-path character), so `/a/target` does not match `/a/target2`.
fn replace_path(text: &str, path: &str, token: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(path) {
        let after = &rest[pos + path.len()..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'));
        out.push_str(&rest[..pos]);
        if boundary {
            out.push_str(token);
        } else {
            out.push_str(path);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn json_escape(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""));
    quoted[1..quoted.len() - 1].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_info_env_values_stay_escaped() {
        let from = Placeholders::new(Path::new(r"D:\w\t1\debug"), Path::new(r"C:\h\.cargo"));
        let to = Placeholders::new(Path::new(r"D:\w\t2\debug"), Path::new(r"C:\h\.cargo"));
        let raw = "D:\\w\\t1\\debug\\deps\\x.d: C:\\h\\.cargo\\src\\lib.rs\n\n# env-dep:OUT_DIR=D:\\\\w\\\\t1\\\\debug\\\\build\\\\out\n";
        let restored = to.decode_dep_info(&from.encode(raw));
        assert_eq!(
            restored,
            "D:\\w\\t2\\debug\\deps\\x.d: C:\\h\\.cargo\\src\\lib.rs\n\n# env-dep:OUT_DIR=D:\\\\w\\\\t2\\\\debug\\\\build\\\\out\n"
        );
        // Unix paths have nothing to escape: same as `decode`.
        let unix = Placeholders::new(Path::new("/w/t/debug"), Path::new("/h/.cargo"));
        let text = "# env-dep:OUT_DIR=@NASPEED_PROFILE_DIR@/build/out\n";
        assert_eq!(unix.decode_dep_info(text), unix.decode(text));
    }

    fn ph(profile: &str) -> Placeholders {
        Placeholders::new(Path::new(profile), Path::new("/home/dev/.cargo"))
    }

    #[test]
    fn round_trip_between_target_dirs() {
        let a = ph("/work/a/target/debug");
        let b = ph("/work/b/target/debug");
        let dep = "/work/a/target/debug/deps/libserde-1.rlib: /home/dev/.cargo/registry/src/x/serde-1/src/lib.rs /work/a/target/debug/build/serde-2/out/private.rs\n# env-dep:OUT_DIR=/work/a/target/debug/build/serde-2/out\n";
        let encoded = a.encode(dep);
        assert!(!encoded.contains("/work/a"));
        assert!(!encoded.contains("/home/dev"));
        assert_eq!(encoded, b.encode(&dep.replace("/work/a", "/work/b")));
        assert_eq!(b.decode(&encoded), dep.replace("/work/a", "/work/b"));
    }

    #[test]
    fn does_not_replace_sibling_prefix() {
        let a = ph("/work/a/target/debug");
        assert_eq!(
            a.encode("/work/a/target/debug2/x"),
            "/work/a/target/debug2/x"
        );
        assert_eq!(a.encode("/work/a/target/debug"), PROFILE_TOKEN);
    }

    #[test]
    fn json_paths_with_escapes() {
        let a = ph("/work/a b\"q/target/debug");
        let msg = serde_json::json!({"artifact": "/work/a b\"q/target/debug/deps/libx.rmeta"})
            .to_string();
        let enc = a.encode(&msg);
        assert!(enc.contains(PROFILE_TOKEN), "{enc}");
        let b = ph("/work/c\"d/target/debug");
        let dec: serde_json::Value = serde_json::from_str(&b.decode_json(&enc)).unwrap();
        assert_eq!(dec["artifact"], "/work/c\"d/target/debug/deps/libx.rmeta");
    }
}
