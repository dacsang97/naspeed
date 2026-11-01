//! `setup` / `uninstall` against a throwaway CARGO_HOME and config dir.

use std::path::{Path, PathBuf};
use std::process::Command;

const NASPEED: &str = env!("CARGO_BIN_EXE_naspeed");

const ORIGINAL: &str = "# Existing user config.\n[build]\nrustc-wrapper = \"sccache\"\njobs = 2\nincremental = false\n\n[env]\nSCCACHE_IDLE_TIMEOUT = \"600\"\n";

struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
    cargo_home: PathBuf,
    config: PathBuf,
    store: PathBuf,
}

impl Home {
    fn new(config_text: Option<&str>) -> Home {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cargo_home = home.join(".cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        if let Some(text) = config_text {
            std::fs::write(cargo_home.join("config.toml"), text).unwrap();
        }
        let store = dir.path().join("store");
        Home {
            config: home.join(".config/naspeed"),
            home,
            cargo_home,
            store,
            _dir: dir,
        }
    }

    fn naspeed(&self, args: &[&str]) -> String {
        let out = Command::new(NASPEED)
            .env("NO_COLOR", "1")
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("NASPEED_CONFIG_DIR", &self.config)
            .env_remove("RUSTC_WRAPPER")
            .env_remove("NASPEED_STORE")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "naspeed {args:?} failed: {text}");
        text
    }

    fn setup(&self) -> String {
        self.naspeed(&["setup", "--store", self.store.to_str().unwrap()])
    }

    fn config_text(&self) -> Option<String> {
        std::fs::read_to_string(self.cargo_home.join("config.toml")).ok()
    }

    fn first_backup(&self) -> PathBuf {
        self.config.join("backups/config.toml.before-naspeed")
    }
}

fn bin(home: &Home) -> PathBuf {
    home.cargo_home
        .join("bin")
        .join(format!("naspeed{}", std::env::consts::EXE_SUFFIX))
}

/// `build.rustc-wrapper` as a string (TOML may quote a Windows path either way).
fn rustc_wrapper(cfg: &str) -> Option<String> {
    let doc: toml_edit::DocumentMut = cfg.parse().unwrap();
    doc.get("build")?
        .get("rustc-wrapper")?
        .as_str()
        .map(str::to_owned)
}

/// The `ns` alias: a symlink to `naspeed` on Unix, a hard link (or copy)
/// with the same bytes on Windows.
fn assert_ns_alias(ns: &Path, bin: &Path) {
    if cfg!(windows) {
        assert_eq!(std::fs::read(ns).unwrap(), std::fs::read(bin).unwrap());
    } else {
        assert_eq!(std::fs::read_link(ns).unwrap(), PathBuf::from("naspeed"));
    }
}

#[test]
fn setup_then_uninstall_is_byte_identical() {
    let h = Home::new(Some(ORIGINAL));
    let out = h.setup();
    let cfg = h.config_text().unwrap();
    assert_eq!(
        rustc_wrapper(&cfg).as_deref(),
        Some(bin(&h).to_string_lossy().as_ref()),
        "{cfg}"
    );
    assert!(cfg.contains("jobs = 4"));
    assert!(
        cfg.contains("incremental = false"),
        "incremental kept off by default"
    );
    assert!(!cfg.contains("SCCACHE_"));
    assert!(cfg.starts_with("# Existing user config."));
    assert!(bin(&h).is_file());
    let ns = h
        .cargo_home
        .join("bin")
        .join(format!("ns{}", std::env::consts::EXE_SUFFIX));
    assert_ns_alias(&ns, &bin(&h));
    // Through the alias: CLI only, never wrapper mode.
    let help_out = Command::new(&ns).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help_out.stdout);
    for cmd in ["dev", "release", "image", "ci", "cache", "setup", "gc"] {
        assert!(help.contains(cmd), "ns --help lacks {cmd}: {help}");
    }
    let rustc_out = Command::new(&ns)
        .args(["rustc", "--version"])
        .output()
        .unwrap();
    assert!(!rustc_out.status.success(), "ns rustc must not run rustc");
    assert!(out.contains("incremental = false kept"), "{out}");

    h.naspeed(&["uninstall"]);
    assert_eq!(h.config_text().unwrap(), ORIGINAL);
    assert!(!bin(&h).exists());
    assert!(ns.symlink_metadata().is_err(), "ns alias removed");
}

#[test]
fn uninstall_keeps_later_owner_edits() {
    let h = Home::new(Some(ORIGINAL));
    h.setup();
    let edited = h
        .config_text()
        .unwrap()
        .replace("[env]", "[env]\nMY_VAR = \"1\"");
    std::fs::write(h.cargo_home.join("config.toml"), edited).unwrap();
    h.naspeed(&["uninstall"]);
    let cfg = h.config_text().unwrap();
    assert!(cfg.contains("MY_VAR = \"1\""), "{cfg}");
    assert!(cfg.contains("rustc-wrapper = \"sccache\""), "{cfg}");
    assert!(cfg.contains("jobs = 2"));
    assert!(cfg.contains("SCCACHE_IDLE_TIMEOUT = \"600\""));
}

#[test]
fn second_setup_keeps_first_backup_and_is_idempotent() {
    let h = Home::new(Some(ORIGINAL));
    h.setup();
    let after_first = h.config_text().unwrap();
    let out = h.setup();
    assert!(out.contains("cargo config already set"), "{out}");
    assert_eq!(h.config_text().unwrap(), after_first);
    assert_eq!(std::fs::read_to_string(h.first_backup()).unwrap(), ORIGINAL);
    let backups = std::fs::read_dir(h.config.join("backups")).unwrap().count();
    assert_eq!(backups, 2, "first backup + one timestamped backup");
    h.naspeed(&["uninstall"]);
    assert_eq!(h.config_text().unwrap(), ORIGINAL);
}

#[test]
fn incremental_and_deps_debug_are_set_and_undone() {
    let h = Home::new(Some(ORIGINAL));
    let store = h.store.to_str().unwrap().to_owned();
    h.naspeed(&[
        "setup",
        "--store",
        &store,
        "--enable-incremental",
        "--deps-debug",
        "line-tables-only",
    ]);
    let cfg = h.config_text().unwrap();
    assert!(!cfg.contains("incremental"), "{cfg}");
    assert!(cfg.contains("[profile.dev.package.\"*\"]"), "{cfg}");
    assert!(cfg.contains("debug = \"line-tables-only\""), "{cfg}");
    h.naspeed(&["uninstall"]);
    assert_eq!(h.config_text().unwrap(), ORIGINAL);
}

#[test]
fn missing_config_is_removed_again() {
    let h = Home::new(None);
    h.setup();
    assert!(h.config_text().is_some());
    h.naspeed(&["uninstall"]);
    assert!(h.config_text().is_none());
}

#[test]
fn uninstall_without_state_uses_first_backup() {
    let h = Home::new(Some(ORIGINAL));
    h.setup();
    std::fs::remove_file(h.config.join("setup-state.json")).unwrap();
    h.naspeed(&["uninstall"]);
    assert_eq!(h.config_text().unwrap(), ORIGINAL);
}

#[test]
fn status_runs_and_reports_store() {
    let h = Home::new(Some(ORIGINAL));
    h.setup();
    let out = h.naspeed(&["status"]);
    assert!(out.contains("cargo wraps rustc with naspeed"), "{out}");
    assert!(
        out.contains(&format!(
            "path={}",
            naspeed::platform::canonicalize(&h.store).unwrap().display()
        )),
        "{out}"
    );
}

#[test]
fn shell_rc_wrapper_is_reported_not_edited() {
    let h = Home::new(Some(ORIGINAL));
    let rc = h.home.join(".zshrc");
    std::fs::write(&rc, "export RUSTC_WRAPPER=sccache\n").unwrap();
    let out = h.setup();
    assert!(out.contains(".zshrc:1"), "{out}");
    assert_eq!(
        std::fs::read_to_string(rc).unwrap(),
        "export RUSTC_WRAPPER=sccache\n"
    );
}
