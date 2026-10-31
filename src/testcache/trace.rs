//! Runtime input tracking for `ns test`, the way `go test` records what a
//! test read: the tracer library (`trace/naspeed_trace.c`, built by
//! `build.rs` and embedded here) is inserted into the test binary and logs
//! every environment variable, file, stat, directory listing and executable
//! the test touches. After a passing run naspeed snapshots those inputs; a
//! stored pass replays only while every one of them still looks the same.
//!
//! Environment values are stored as hashes, never in clear (they can be
//! secrets). Paths naspeed cannot meaningfully track (system libraries,
//! temp dirs, the target dir, the cargo registry) are left out.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::content_hash::hash_file;
use crate::wrapper::Env;

/// The tracer for this target, or empty when it could not be built.
static TRACE_LIB: &[u8] = include_bytes!(env!("NASPEED_TRACE_LIB"));

pub const LOG_ENV: &str = "NASPEED_TRACE_LOG";
/// `NASPEED_TEST_TRACE=0` turns tracking off (declared inputs only).
pub const TRACE_SWITCH_ENV: &str = "NASPEED_TEST_TRACE";

#[cfg(target_os = "macos")]
const INSERT_ENV: &str = "DYLD_INSERT_LIBRARIES";
#[cfg(not(target_os = "macos"))]
const INSERT_ENV: &str = "LD_PRELOAD";
#[cfg(target_os = "macos")]
const LIB_EXT: &str = "dylib";
#[cfg(not(target_os = "macos"))]
const LIB_EXT: &str = "so";

/// One recorded input and what it looked like after the passing run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Input {
    /// Environment variable: hash of the value, `None` when unset.
    Env { name: String, value: Option<String> },
    /// File read (or executed): content hash, `None` when absent.
    File { path: PathBuf, hash: Option<String> },
    /// Path stat'ed: `file`, `dir`, `other` or `absent`.
    Stat { path: PathBuf, kind: String },
    /// Directory listed: hash of the sorted entry names, `None` when absent.
    Dir {
        path: PathBuf,
        names: Option<String>,
    },
}

impl Input {
    /// Human-readable name for `--explain`.
    pub fn describe(&self) -> String {
        match self {
            Input::Env { name, .. } => format!("env {name}"),
            Input::File { path, .. } => format!("file {}", path.display()),
            Input::Stat { path, .. } => format!("path {}", path.display()),
            Input::Dir { path, .. } => format!("dir {}", path.display()),
        }
    }

    /// The same input as it is now.
    fn current(&self, env: &Env) -> Input {
        match self {
            Input::Env { name, .. } => env_input(name, env),
            Input::File { path, .. } => file_input(path),
            Input::Stat { path, .. } => stat_input(path),
            Input::Dir { path, .. } => dir_input(path),
        }
    }
}

fn hash_value(v: &str) -> String {
    blake3::hash(v.as_bytes()).to_hex()[..32].to_string()
}

fn env_input(name: &str, env: &Env) -> Input {
    Input::Env {
        name: name.to_owned(),
        value: env.get(name).map(|v| hash_value(v)),
    }
}

fn file_input(path: &Path) -> Input {
    let hash = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => hash_file(path).ok().map(|h| h.to_hex().to_string()),
        // A directory opened for reading (std::fs::read_dir on some libcs).
        Ok(m) if m.is_dir() => return dir_input(path),
        _ => None,
    };
    Input::File {
        path: path.to_owned(),
        hash,
    }
}

fn stat_input(path: &Path) -> Input {
    let kind = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => "file",
        Ok(m) if m.is_dir() => "dir",
        Ok(_) => "other",
        Err(_) => "absent",
    };
    Input::Stat {
        path: path.to_owned(),
        kind: kind.into(),
    }
}

fn dir_input(path: &Path) -> Input {
    let names = std::fs::read_dir(path).ok().map(|entries| {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        hash_value(&names.join("\n"))
    });
    Input::Dir {
        path: path.to_owned(),
        names,
    }
}

/// Write the embedded tracer into the store (once per tracer build) and
/// return its path; `None` when this naspeed has no tracer.
pub fn library(store: &Path) -> Option<PathBuf> {
    if TRACE_LIB.is_empty()
        || std::env::var(TRACE_SWITCH_ENV).is_ok_and(|v| v == "0" || v == "false")
    {
        return None;
    }
    let id = &blake3::hash(TRACE_LIB).to_hex()[..16];
    let path = store
        .join("lib")
        .join(format!("naspeed-trace-{id}.{LIB_EXT}"));
    if !path.is_file() {
        let dir = path.parent()?;
        std::fs::create_dir_all(dir).ok()?;
        let tmp = dir.join(format!(".trace.{}.tmp", std::process::id()));
        std::fs::write(&tmp, TRACE_LIB).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    Some(path)
}

/// Environment additions that insert the tracer and point it at `log`.
pub fn child_env(lib: &Path, log: &Path, env: &Env) -> Vec<(OsString, OsString)> {
    let mut insert = OsString::from(lib);
    if let Some(existing) = env.get(INSERT_ENV).filter(|v| !v.is_empty()) {
        insert.push(":");
        insert.push(existing);
    }
    vec![
        (INSERT_ENV.into(), insert),
        (LOG_ENV.into(), log.as_os_str().to_owned()),
    ]
}

/// Variables that cannot change a test's verdict, or that naspeed itself
/// sets, so recording them would only cause pointless re-runs.
fn ignored_env(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "RUST_BACKTRACE",
        "RUST_LIB_BACKTRACE",
        "RUST_MIN_STACK",
        "TMPDIR",
        "TMP",
        "TEMP",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "PATH",
    ];
    EXACT.contains(&name)
        || [
            "NASPEED_",
            "DYLD_",
            "LD_",
            "RUST_TEST_",
            "CARGO_PKG_",
            "TERMINFO",
            "Malloc",
            "__CF",
            "XPC_",
        ]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Path prefixes whose content naspeed does not track.
pub struct Exclusions {
    prefixes: Vec<PathBuf>,
}

impl Exclusions {
    pub fn new(target_dir: &Path, store: &Path, env: &Env) -> Exclusions {
        let mut prefixes: Vec<PathBuf> = [
            "/dev",
            "/proc",
            "/sys",
            "/System",
            "/usr/lib",
            "/usr/libexec",
            "/usr/share",
            "/private/var/db",
            "/private/var/folders",
            "/var/folders",
            "/tmp",
            "/private/tmp",
            "/etc/ld.so.cache",
            "/lib",
            "/lib64",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        prefixes.push(target_dir.to_owned());
        prefixes.push(store.to_owned());
        prefixes.push(std::env::temp_dir());
        let home = env.get("HOME").map(PathBuf::from);
        let cargo_home = env
            .get("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".cargo")));
        if let Some(c) = cargo_home {
            // Registry and git sources are immutable per version; the
            // version is already in Cargo.lock and the binary.
            prefixes.push(c.join("registry"));
            prefixes.push(c.join("git"));
        }
        let rustup = env
            .get("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".rustup")));
        prefixes.extend(rustup);
        Exclusions { prefixes }
    }

    fn excluded(&self, path: &Path) -> bool {
        // Terminal capability lookups (libtest's colour support).
        path.components()
            .any(|c| c.as_os_str() == "terminfo" || c.as_os_str() == ".terminfo")
            || self.prefixes.iter().any(|p| path.starts_with(p))
    }
}

/// Lexically normalise `a/./b/../c` (no filesystem access, symlinks kept).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Programs the tracer cannot follow into: macOS strips `DYLD_*` when it
/// starts a SIP-protected binary, so what those read is not recorded.
fn untraceable_program(path: &Path) -> bool {
    cfg!(target_os = "macos")
        && [
            "/bin",
            "/sbin",
            "/usr/bin",
            "/usr/sbin",
            "/usr/libexec",
            "/System",
        ]
        .iter()
        .any(|p| path.starts_with(p))
}

/// What a traced run read.
pub struct Traced {
    pub inputs: Vec<Input>,
    /// The test started a program the tracer cannot follow; its reads are
    /// missing from `inputs`.
    pub partial: bool,
}

/// Turn the tracer's log into the snapshot of inputs; `None` when the log
/// shows the tracer never loaded (e.g. a statically linked test binary).
pub fn collect(log: &Path, binary: &Path, env: &Env, excl: &Exclusions) -> Option<Traced> {
    let text = std::fs::read_to_string(log).ok()?;
    let mut loaded = false;
    let mut partial = false;
    let mut envs = BTreeSet::new();
    let mut paths: BTreeMap<PathBuf, char> = BTreeMap::new();
    // Rank of what a path needs: content beats listing beats existence.
    let rank = |k: char| match k {
        'R' | 'X' => 3,
        'D' => 2,
        _ => 1,
    };
    let search_path: Vec<PathBuf> = env
        .get("PATH")
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    for line in text.lines() {
        let (kind, rest) = match line.split_once('\t') {
            Some((k, r)) => (k.chars().next().unwrap_or(' '), r),
            None => (line.chars().next().unwrap_or(' '), ""),
        };
        match kind {
            'I' => loaded = true,
            'E' if !rest.is_empty() && !ignored_env(rest) => {
                envs.insert(rest.to_owned());
            }
            'R' | 'S' | 'D' | 'X' if !rest.is_empty() => {
                let candidates: Vec<PathBuf> = if kind == 'X' && !rest.contains('/') {
                    // Bare program name: every PATH entry that could hold it.
                    search_path.iter().map(|d| d.join(rest)).collect()
                } else {
                    vec![normalize(Path::new(rest))]
                };
                for p in candidates {
                    if kind == 'X' && untraceable_program(&p) && p.is_file() {
                        partial = true;
                    }
                    if p.is_absolute() && !excl.excluded(&p) && p != binary {
                        let e = paths.entry(p).or_insert(kind);
                        if rank(kind) > rank(*e) {
                            *e = kind;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !loaded {
        return None;
    }
    let mut inputs: Vec<Input> = envs.iter().map(|n| env_input(n, env)).collect();
    for (path, kind) in paths {
        inputs.push(match kind {
            'R' | 'X' => file_input(&path),
            'D' => dir_input(&path),
            _ => stat_input(&path),
        });
    }
    Some(Traced { inputs, partial })
}

/// The first recorded input that no longer looks the same, if any.
pub fn changed<'a>(inputs: &'a [Input], env: &Env) -> Option<&'a Input> {
    inputs.iter().find(|i| i.current(env) != **i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_becomes_inputs_without_system_or_target_paths() {
        // Not under the temp dir: temp paths are excluded on purpose.
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("trace-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("ws");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/a.txt"), "a").unwrap();
        let target = root.join("target");
        let log = base.join("log");
        std::fs::write(
            &log,
            format!(
                "I\t\nE\tAPP_MODE\nE\tTERM\nR\t{r}/data/a.txt\nS\t{r}/data/./missing\nD\t{r}/data\nR\t{t}/debug/x\nR\t/usr/lib/libSystem.B.dylib\nS\t{r}/data/a.txt\n",
                r = root.display(),
                t = target.display()
            ),
        )
        .unwrap();
        let mut env = Env::new();
        env.insert("APP_MODE".into(), "good".into());
        env.insert("HOME".into(), "/nonexistent-home".into());
        let excl = Exclusions::new(&target, Path::new("/store"), &env);
        let inputs = collect(&log, Path::new("/bin/test"), &env, &excl)
            .unwrap()
            .inputs;
        let names: Vec<String> = inputs.iter().map(Input::describe).collect();
        assert_eq!(
            names,
            vec![
                "env APP_MODE".to_string(),
                format!("dir {}", root.join("data").display()),
                format!("file {}", root.join("data/a.txt").display()),
                format!("path {}", root.join("data/missing").display()),
            ]
        );
        assert!(changed(&inputs, &env).is_none());

        env.insert("APP_MODE".into(), "bad".into());
        assert_eq!(changed(&inputs, &env).unwrap().describe(), "env APP_MODE");
        env.insert("APP_MODE".into(), "good".into());
        std::fs::write(root.join("data/a.txt"), "b").unwrap();
        assert!(
            changed(&inputs, &env)
                .unwrap()
                .describe()
                .starts_with("file")
        );
        std::fs::write(root.join("data/a.txt"), "a").unwrap();
        std::fs::write(root.join("data/missing"), "now here").unwrap();
        // Listing and the probed path both changed; the dir is checked first.
        assert!(changed(&inputs, &env).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn starting_a_sip_program_marks_the_run_partial() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("trace-sip-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("log");
        std::fs::write(&log, "I\t\nX\t/bin/sh\n").unwrap();
        let env = Env::new();
        let excl = Exclusions::new(&base.join("target"), Path::new("/s"), &env);
        assert!(collect(&log, Path::new("/b"), &env, &excl).unwrap().partial);
        std::fs::write(&log, "I\t\n").unwrap();
        assert!(!collect(&log, Path::new("/b"), &env, &excl).unwrap().partial);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_log_without_the_loaded_marker_means_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(&log, "E\tX\n").unwrap();
        let env = Env::new();
        let excl = Exclusions::new(Path::new("/t"), Path::new("/s"), &env);
        assert!(collect(&log, Path::new("/b"), &env, &excl).is_none());
    }
}
