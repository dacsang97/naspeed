//! `ns.toml`: optional per-workspace settings for `ns dev`, `ns release` and
//! `ns image`. Every field has a default derived from `cargo metadata`, and
//! unknown keys are errors (with line and column) rather than silently ignored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const FILE_NAME: &str = "ns.toml";

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NsConfig {
    pub dev: DevConfig,
    pub release: ReleaseConfig,
    pub image: ImageConfig,
    pub test: TestConfig,
}

/// `[test]`: the `ns test` result cache.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestConfig {
    /// Replay unchanged passing test binaries instead of running them.
    pub cache: bool,
    /// Environment variables the tests read; their values join the key.
    pub env: Vec<String>,
    /// Extra files or directories (relative to the workspace root) the
    /// tests read at run time, beyond the package and its workspace deps.
    pub inputs: Vec<PathBuf>,
    /// Record what each test binary reads at run time (env, files, dirs,
    /// executables) and replay a pass only while all of it is unchanged.
    pub trace: bool,
    /// Globs (relative to the workspace root) left out of the workspace
    /// files hashed when a run could not be traced, e.g. `docs/**`.
    pub exclude: Vec<String>,
}

impl Default for TestConfig {
    fn default() -> Self {
        TestConfig {
            cache: true,
            env: Vec::new(),
            inputs: Vec::new(),
            trace: true,
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DevConfig {
    /// Binary to run; default: the only binary of the workspace.
    pub bin: Option<String>,
    pub args: Vec<String>,
    /// Loaded into the child's environment; changes only restart the child.
    pub env_file: Option<PathBuf>,
    /// Paths (relative to the workspace root) to watch; default: every member.
    pub watch: Vec<PathBuf>,
    /// Glob patterns ignored on top of `.gitignore`, `target*` and `.git`.
    pub ignore: Vec<String>,
    /// Ports that must be free before the new process starts.
    pub ports: Vec<u16>,
    /// After each clean build, run tests of changed crates and their dependents.
    pub test: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReleaseConfig {
    /// Binaries to ship (one plain build each); default: every workspace
    /// binary. Ignored when `builds` is set.
    pub bins: Vec<String>,
    /// Build variants: the same binary with other
    /// features or another output name (e.g. `api` and `api-noui`).
    pub builds: Vec<BuildConfig>,
    pub targets: Vec<String>,
    pub profile: String,
    /// Shell commands run at the workspace root before any build, e.g.
    /// building an embedded web UI.
    pub before: Vec<String>,
    pub archive: ArchiveConfig,
    /// Name of the checksum file.
    pub checksum: String,
    pub sign: SignMode,
    /// Create the GitHub Release as a draft.
    pub draft: bool,
    /// `"auto"` marks tags with a pre-release part (`v1.2.0-rc.1`) as
    /// pre-releases; `true`/`false` force it.
    pub prerelease: Prerelease,
    pub changelog: ChangelogConfig,
    /// SBOM written next to the archives (and attached to images).
    pub sbom: SbomMode,
}

/// One build variant. Every build gets `NS_VERSION`, `NS_TAG`, `NS_COMMIT`
/// and `SOURCE_DATE_EPOCH` in its environment (read them with
/// `option_env!`), the Rust counterpart of `-ldflags -X main.version=…`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    /// Identifier referenced by `[image].build`; default: `binary`.
    pub id: Option<String>,
    /// Cargo binary target (`--bin`).
    pub bin: String,
    /// File name shipped in archives and images; default: `bin`.
    pub binary: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    /// Extra environment for this build only.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Override `release.targets` for this build.
    #[serde(default)]
    pub targets: Vec<String>,
}

impl BuildConfig {
    pub fn binary_name(&self) -> &str {
        self.binary.as_deref().unwrap_or(&self.bin)
    }

    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or_else(|| self.binary_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Prerelease {
    Fixed(bool),
    Auto(AutoKeyword),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoKeyword {
    Auto,
}

impl Prerelease {
    /// Whether a release for `tag` is a pre-release.
    pub fn resolve(self, tag: &str) -> bool {
        match self {
            Prerelease::Fixed(b) => b,
            // semver pre-release part: anything after a `-` in the version.
            Prerelease::Auto(_) => tag.trim_start_matches('v').contains('-'),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogConfig {
    pub sort: ChangelogSort,
    /// Regexes; commits whose subject matches any are left out (e.g. `^docs:`).
    pub exclude: Vec<String>,
}

impl Default for ChangelogConfig {
    fn default() -> Self {
        ChangelogConfig {
            sort: ChangelogSort::Asc,
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangelogSort {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SbomMode {
    /// SPDX 2.3 JSON from `cargo metadata`.
    Spdx,
    None,
}

impl Default for ReleaseConfig {
    fn default() -> Self {
        ReleaseConfig {
            bins: Vec::new(),
            builds: Vec::new(),
            targets: DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect(),
            profile: "release".into(),
            before: Vec::new(),
            archive: ArchiveConfig::default(),
            checksum: "SHA256SUMS".into(),
            sign: SignMode::Cosign,
            draft: false,
            prerelease: Prerelease::Auto(AutoKeyword::Auto),
            changelog: ChangelogConfig::default(),
            sbom: SbomMode::Spdx,
        }
    }
}

pub const DEFAULT_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "universal2-apple-darwin",
    "x86_64-pc-windows-msvc",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    pub format: ArchiveFormat,
    /// Format for Windows targets.
    pub windows: ArchiveFormat,
    /// Extra files (relative to the workspace root) packed next to the binary.
    pub files: Vec<PathBuf>,
    /// Archive base name (without extension). Placeholders: `{project}`,
    /// `{id}`, `{binary}`, `{version}`, `{tag}`, `{os}`, `{arch}` (OCI-style
    /// `linux`/`darwin`/`windows`, `amd64`/`arm64`/`all`) and `{target}`.
    pub name: String,
}

pub const DEFAULT_ARCHIVE_NAME: &str = "{binary}-{version}-{target}";

impl Default for ArchiveConfig {
    fn default() -> Self {
        ArchiveConfig {
            format: ArchiveFormat::TarGz,
            windows: ArchiveFormat::Zip,
            files: Vec::new(),
            name: DEFAULT_ARCHIVE_NAME.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ArchiveFormat {
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "zip")]
    Zip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignMode {
    Cosign,
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageConfig {
    /// Repository without tag, e.g. `ghcr.io/owner/api`.
    pub name: Option<String>,
    /// Base image; default: distroless `static-debian12:nonroot` for musl
    /// targets, `cc-debian12:nonroot` for gnu.
    pub base: Option<String>,
    pub platforms: Vec<String>,
    /// Default: `[path]`.
    pub entrypoint: Vec<String>,
    /// Default arguments (Dockerfile `CMD`), e.g. `["serve", "-config", "/data/config.yaml"]`.
    pub cmd: Vec<String>,
    /// Binary placed in the image (a release binary name); default: the only one.
    pub bin: Option<String>,
    /// Release build id to package; takes precedence over `bin`.
    pub build: Option<String>,
    /// Where the binary goes; `{binary}` is replaced. On PATH so `docker exec … <binary>` works.
    pub path: String,
    /// Image config `User`; default nonroot like distroless `:nonroot`.
    pub user: String,
    pub workdir: Option<String>,
    /// `KEY=value` entries appended to the base image's `Env`.
    pub env: Vec<String>,
    /// `ExposedPorts`, e.g. `["8317"]` or `["53/udp"]`.
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
    /// Tags pushed; placeholders `{version}`, `{tag}`, `{commit}`, `{short_commit}`.
    /// `latest` is skipped for snapshots and pre-releases.
    pub tags: Vec<String>,
    /// Extra labels on top of `org.opencontainers.image.{title,version,revision,source,created}`.
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Default for ImageConfig {
    fn default() -> Self {
        ImageConfig {
            name: None,
            base: None,
            platforms: vec!["linux/amd64".into(), "linux/arm64".into()],
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            bin: None,
            build: None,
            path: "/usr/bin/{binary}".into(),
            user: "65532:65532".into(),
            workdir: None,
            env: Vec::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            tags: vec!["{version}".into(), "latest".into()],
            labels: Default::default(),
        }
    }
}

impl NsConfig {
    pub fn parse(text: &str) -> Result<NsConfig> {
        // toml's error already names the line, column and offending key.
        toml::from_str(text).map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// `<root>/ns.toml`, or all defaults when the file does not exist.
    pub fn load(workspace_root: &Path) -> Result<NsConfig> {
        let path = workspace_root.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                NsConfig::parse(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(NsConfig::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[dev]
bin = "api"
args = ["--port", "8080"]
env_file = ".env"
watch = ["crates", "apps"]
ignore = ["**/*.md"]
ports = [8080]
test = true

[release]
bins = ["api"]
targets = ["x86_64-unknown-linux-musl", "universal2-apple-darwin"]
profile = "release"
archive = { format = "tar.gz", windows = "zip", files = ["README.md", "LICENSE"] }
sign = "cosign"

[image]
name = "ghcr.io/owner/api"
base = "gcr.io/distroless/cc-debian12"
platforms = ["linux/amd64", "linux/arm64"]
entrypoint = ["/api"]
"#;

    #[test]
    fn parses_full_file() {
        let cfg = NsConfig::parse(FULL).unwrap();
        assert_eq!(cfg.dev.bin.as_deref(), Some("api"));
        assert_eq!(cfg.dev.ports, vec![8080]);
        assert!(cfg.dev.test);
        assert_eq!(cfg.release.targets.len(), 2);
        assert_eq!(cfg.release.archive.files.len(), 2);
        assert_eq!(cfg.release.sign, SignMode::Cosign);
        assert_eq!(cfg.image.entrypoint, vec!["/api".to_string()]);
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let cfg = NsConfig::parse("").unwrap();
        assert_eq!(cfg, NsConfig::default());
        assert_eq!(cfg.release.targets.len(), DEFAULT_TARGETS.len());
        assert_eq!(cfg.release.profile, "release");
        assert_eq!(cfg.image.platforms.len(), 2);
    }

    #[test]
    fn unknown_key_is_an_error_with_location() {
        let err = NsConfig::parse("[dev]\nbin = \"api\"\nport = 8080\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("port"), "{err}");
        assert!(err.contains("line 3"), "{err}");
        let err = NsConfig::parse("[deploy]\nx = 1\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("deploy"), "{err}");
    }

    #[test]
    fn multi_build_release_and_image() {
        let cfg = NsConfig::parse(
            r#"
[release]
before = ["cd web/ui && bun run build"]
checksum = "checksums.txt"
draft = true
prerelease = "auto"
archive = { name = "{binary}_{version}_{os}_{arch}" }
changelog = { sort = "asc", exclude = ["^docs:", "^test:"] }

[[release.builds]]
bin = "myapp"
features = ["dashboard-ui"]

[[release.builds]]
id = "noui"
bin = "myapp"
binary = "myapp-noui"
no_default_features = true
env = { RUSTFLAGS = "-C target-cpu=x86-64-v2" }

[image]
name = "ghcr.io/owner/myapp"
build = "myapp"
cmd = ["serve", "-config", "/data/config.yaml"]
workdir = "/data"
volumes = ["/data"]
ports = ["8317"]
tags = ["{tag}", "latest"]
"#,
        )
        .unwrap();
        let r = &cfg.release;
        assert_eq!(r.builds.len(), 2);
        assert_eq!(r.builds[0].id(), "myapp");
        assert_eq!(r.builds[1].id(), "noui");
        assert_eq!(r.builds[1].binary_name(), "myapp-noui");
        assert_eq!(r.checksum, "checksums.txt");
        assert!(r.draft);
        assert!(r.prerelease.resolve("v1.2.0-rc.1"));
        assert!(!r.prerelease.resolve("v1.2.0"));
        assert_eq!(r.changelog.exclude.len(), 2);
        assert_eq!(cfg.image.user, "65532:65532");
        assert_eq!(cfg.image.path, "/usr/bin/{binary}");
        assert_eq!(cfg.image.workdir.as_deref(), Some("/data"));
        let forced = NsConfig::parse("[release]\nprerelease = true\n").unwrap();
        assert!(forced.release.prerelease.resolve("v1.0.0"));
        assert!(NsConfig::parse("[release]\nprerelease = \"sometimes\"\n").is_err());
        assert!(NsConfig::parse("[[release.builds]]\nbin = \"a\"\nfeature = []\n").is_err());
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(NsConfig::load(dir.path()).unwrap(), NsConfig::default());
        std::fs::write(dir.path().join(FILE_NAME), "[release]\nsign = \"gpg\"\n").unwrap();
        assert!(NsConfig::load(dir.path()).is_err());
    }
}
