//! Decide whether a rustc invocation may go through the store. Anything we
//! are not sure about is passed through untouched.

use std::path::{Path, PathBuf};

use super::Env;
use super::rustc_args::RustcInvocation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// `$CARGO_HOME/registry/src/<index>/<pkg>-<ver>`
    Registry,
    /// `$CARGO_HOME/git/checkouts/<repo>/<rev>`
    Git,
    /// Workspace members and other path dependencies. Mutable sources: the
    /// key hashes exactly the files rustc read last time (from dep-info).
    /// `source_root` is the workspace root (rustc's cwd).
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// `lib` / `rlib` / `proc-macro` into `<profile>/deps`.
    Library,
    /// A registry crate's `build.rs` compiled to
    /// `<profile>/build/<pkg>-<hash>/build_script_build-<hash>`.
    BuildScript,
}

#[derive(Debug, Clone)]
pub struct Cacheable {
    pub kind: UnitKind,
    pub input: PathBuf,
    /// Package dir (registry) or whole checkout (git) whose content is hashed.
    pub source_root: PathBuf,
    pub source_kind: SourceKind,
    pub out_dir: PathBuf,
    /// Parent of `out_dir`: `target/<profile>` or `target/<triple>/<profile>`.
    pub profile_dir: PathBuf,
    pub output_stem: String,
}

/// `RUSTC_*` variables that do not change what rustc produces.
/// `RUSTC_WORKSPACE_WRAPPER` (set by `cargo clippy`) only applies to
/// workspace members; for registry crates cargo still runs plain rustc,
/// which `is_rustc_program` checks.
const BENIGN_RUSTC_ENV: &[&str] = &[
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_LINKER",
    "RUSTC_WORKSPACE_WRAPPER",
];

const ALLOWED_CRATE_TYPES: &[&str] = &["lib", "rlib", "proc-macro"];
const ALLOWED_EMIT: &[&str] = &["link", "metadata", "dep-info"];

pub fn is_rustc_program(program: &Path) -> bool {
    matches!(
        program.file_name().and_then(|n| n.to_str()),
        Some("rustc") | Some("rustc.exe")
    )
}

pub fn check(
    program: &Path,
    inv: &RustcInvocation,
    cwd: &Path,
    env: &Env,
    cargo_home: &Path,
    allow_local: bool,
) -> Result<Cacheable, String> {
    // With RUSTC_WORKSPACE_WRAPPER (clippy-driver, …) argv[1] is that tool.
    if !is_rustc_program(program) {
        return Err(format!("program is not rustc: {}", program.display()));
    }
    if let Some(flag) = inv.unknown_flags.first() {
        return Err(format!("unknown flag {flag}"));
    }
    if inv.print || inv.output_file || inv.test || inv.unstable {
        return Err("print/-o/--test/-Z".into());
    }
    let [input] = inv.inputs.as_slice() else {
        return Err(format!("{} inputs", inv.inputs.len()));
    };
    if input == "-" {
        return Err("stdin input".into());
    }
    let input = cwd.join(input);
    let (source_kind, source_root) = match source_root(&input, cargo_home) {
        Some(found) => found,
        None if allow_local => (SourceKind::Local, cwd.to_path_buf()),
        None => return Err("input outside registry/git checkouts".into()),
    };
    // Cargo only asks for incremental on local crates; outputs are the
    // same as a non-incremental compile, so a hit is still valid.
    if inv.incremental && source_kind != SourceKind::Local {
        return Err("incremental".into());
    }
    if source_kind != SourceKind::Local && env.contains_key("CARGO_PRIMARY_PACKAGE") {
        return Err("workspace member (CARGO_PRIMARY_PACKAGE)".into());
    }
    if let Some(var) = env
        .keys()
        .find(|k| k.starts_with("RUSTC_") && !BENIGN_RUSTC_ENV.contains(&k.as_str()))
    {
        return Err(format!("env {var}"));
    }
    // `build_script_build`, or `build_script_<stem>` for `build = "x/main.rs"`.
    // Local build scripts are not cached: their run would need the whole
    // (mutable) package hashed.
    let build_script = inv
        .crate_name
        .as_deref()
        .is_some_and(|n| n.starts_with("build_script_"))
        && inv.crate_types == ["bin"]
        && source_kind != SourceKind::Local;
    if !build_script
        && (inv.crate_types.is_empty()
            || inv
                .crate_types
                .iter()
                .any(|t| !ALLOWED_CRATE_TYPES.contains(&t.as_str())))
    {
        return Err(format!("crate types {:?}", inv.crate_types));
    }
    if inv.emit.len() != 1 || inv.emit_kinds().any(|k| !ALLOWED_EMIT.contains(&k)) {
        return Err(format!("emit {:?}", inv.emit));
    }
    if !inv.emit_kinds().any(|k| k == "link" || k == "metadata") {
        return Err("no link/metadata output".into());
    }
    let Some(output_stem) = inv.output_stem() else {
        return Err("no crate name".into());
    };
    let Some(out_dir) = inv.out_dir.as_ref().map(|d| cwd.join(d)) else {
        return Err("no --out-dir".into());
    };
    fn dir_name(p: Option<&Path>) -> Option<&str> {
        p.and_then(Path::file_name).and_then(|n| n.to_str())
    }
    let (kind, profile_dir) = if build_script {
        // <profile>/build/<pkg>-<hash>
        let units = out_dir.parent();
        if dir_name(units) != Some("build") {
            return Err("build script --out-dir is not <profile>/build/<unit>".into());
        }
        (UnitKind::BuildScript, units.and_then(Path::parent))
    } else {
        if dir_name(Some(&out_dir)) != Some("deps") {
            return Err("--out-dir is not <profile>/deps".into());
        }
        (UnitKind::Library, out_dir.parent())
    };
    let profile_dir = profile_dir.map(Path::to_path_buf).unwrap_or_default();

    Ok(Cacheable {
        kind,
        input,
        source_root,
        source_kind,
        out_dir,
        profile_dir,
        output_stem,
    })
}

/// Package dir for registry sources, checkout dir for git sources.
pub fn source_root(input: &Path, cargo_home: &Path) -> Option<(SourceKind, PathBuf)> {
    let homes = [
        cargo_home.to_path_buf(),
        std::fs::canonicalize(cargo_home).unwrap_or_else(|_| cargo_home.to_path_buf()),
    ];
    for home in &homes {
        for (kind, sub) in [
            (SourceKind::Registry, home.join("registry").join("src")),
            (SourceKind::Git, home.join("git").join("checkouts")),
        ] {
            if let Ok(rest) = input.strip_prefix(&sub) {
                // registry: <index>/<pkg-ver>/…   git: <repo>/<rev>/…
                let mut parts = rest.components();
                let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
                    continue;
                };
                if parts.next().is_none() {
                    continue;
                }
                return Some((kind, sub.join(a).join(b)));
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::rustc_args::parse;
    use super::*;
    use std::ffi::OsString;

    pub(crate) fn fixture(name: &str) -> (Vec<OsString>, Env, PathBuf) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rustc_args");
        let args = std::fs::read_to_string(dir.join(format!("{name}.args"))).unwrap();
        let env = std::fs::read_to_string(dir.join(format!("{name}.env"))).unwrap();
        let cwd = std::fs::read_to_string(dir.join(format!("{name}.cwd"))).unwrap();
        (
            args.lines().map(OsString::from).collect(),
            env.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            PathBuf::from(cwd.trim()),
        )
    }

    fn decide(name: &str) -> Result<Cacheable, String> {
        let (args, env, cwd) = fixture(name);
        check(
            Path::new("rustc"),
            &parse(&args),
            &cwd,
            &env,
            Path::new("/home/dev/.cargo"),
            false,
        )
    }

    #[test]
    fn registry_crates_are_cacheable() {
        for name in [
            "itoa_ta",
            "serde_tb",
            "tokio_ta",
            "proc_macro2_ta",
            "serde_derive_ta",
        ] {
            let c = decide(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(c.source_kind, SourceKind::Registry);
            assert!(c.profile_dir.ends_with("debug"), "{name}");
        }
        let c = decide("serde_ta").unwrap();
        assert!(
            c.source_root
                .ends_with("index.crates.io-1949cf8c6b5b557f/serde-1.0.229")
        );
    }

    #[test]
    fn workspace_crate_is_not_cacheable() {
        assert!(decide("probe2_ta").is_err());
    }

    #[test]
    fn workspace_lib_is_local_when_allowed_but_bins_never() {
        let (args, env, cwd) = fixture("probe2_ta");
        let mut lib_args = args.clone();
        for a in lib_args.iter_mut() {
            if a == "bin" {
                *a = "lib".into();
            }
        }
        lib_args.push("-C".into());
        lib_args.push("incremental=/work/scratch/ta/debug/incremental".into());
        let home = Path::new("/home/dev/.cargo");
        let c = check(
            Path::new("rustc"),
            &parse(&lib_args),
            &cwd,
            &env,
            home,
            true,
        )
        .unwrap();
        assert_eq!(c.source_kind, SourceKind::Local);
        assert_eq!(c.source_root, cwd);
        // The fixture's own crate type is bin: never cached.
        assert!(check(Path::new("rustc"), &parse(&args), &cwd, &env, home, true).is_err());
    }

    #[test]
    fn stdin_print_and_workspace_wrapper_pass_through() {
        let (_, env, cwd) = fixture("itoa_ta");
        let home = Path::new("/home/dev/.cargo");
        let stdin = parse(
            &[
                "-",
                "--crate-name",
                "___",
                "--print=file-names",
                "--crate-type",
                "lib",
            ]
            .map(OsString::from),
        );
        assert!(check(Path::new("rustc"), &stdin, &cwd, &env, home, false).is_err());

        let (args, _, _) = fixture("itoa_ta");
        let inv = parse(&args);
        assert!(check(Path::new("/x/clippy-driver"), &inv, &cwd, &env, home, false).is_err());

        let mut env2 = env.clone();
        env2.insert("RUSTC_BOOTSTRAP".into(), "1".into());
        assert!(check(Path::new("rustc"), &inv, &cwd, &env2, home, false).is_err());

        let mut env3 = env.clone();
        env3.insert("RUSTC_WORKSPACE_WRAPPER".into(), "/x/clippy-driver".into());
        assert!(check(Path::new("rustc"), &inv, &cwd, &env3, home, false).is_ok());
    }

    #[test]
    fn git_checkout_root() {
        let (kind, root) = source_root(
            Path::new("/h/.cargo/git/checkouts/rig-abc/1a2b3c4/rig-core/src/lib.rs"),
            Path::new("/h/.cargo"),
        )
        .unwrap();
        assert_eq!(kind, SourceKind::Git);
        assert_eq!(root, Path::new("/h/.cargo/git/checkouts/rig-abc/1a2b3c4"));
    }
}
