//! Go workspace discovery and `go list` JSON decoding.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GoContext {
    pub root: PathBuf,
    pub pattern: String,
    pub workspace: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ListedModule {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ListedPackage {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub import_path: String,
    pub dir: PathBuf,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub test_imports: Vec<String>,
    #[serde(default)]
    pub x_test_imports: Vec<String>,
    #[serde(default)]
    pub go_files: Vec<PathBuf>,
    #[serde(default)]
    pub cgo_files: Vec<PathBuf>,
    #[serde(default)]
    pub c_files: Vec<PathBuf>,
    #[serde(default)]
    pub h_files: Vec<PathBuf>,
    #[serde(default)]
    pub s_files: Vec<PathBuf>,
    #[serde(default)]
    #[serde(rename = "CXXFiles")]
    pub cxx_files: Vec<PathBuf>,
    #[serde(default)]
    pub m_files: Vec<PathBuf>,
    #[serde(default)]
    pub f_files: Vec<PathBuf>,
    #[serde(default)]
    pub syso_files: Vec<PathBuf>,
    #[serde(default)]
    pub embed_files: Vec<PathBuf>,
    #[serde(default)]
    pub embed_patterns: Vec<String>,
    #[serde(default)]
    pub test_go_files: Vec<PathBuf>,
    #[serde(default)]
    pub x_test_go_files: Vec<PathBuf>,
    pub module: Option<ListedModule>,
    #[serde(default)]
    pub standard: bool,
    #[serde(rename = "Error")]
    pub _error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GoEnvironment {
    #[serde(rename = "GOWORK")]
    work: String,
    #[serde(rename = "GOMOD")]
    module: String,
    #[serde(rename = "GOVERSION")]
    version: String,
}

pub fn detect(dir: &Path) -> Result<GoContext> {
    let output = Command::new("go")
        .args(["env", "-json", "GOWORK", "GOMOD", "GOVERSION"])
        .current_dir(dir)
        .output()
        .context("run go env")?;
    if !output.status.success() {
        bail!(
            "go env failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let env: GoEnvironment = serde_json::from_slice(&output.stdout).context("parse go env JSON")?;
    if !env.work.is_empty() && env.work != "off" {
        let (major, minor) = parse_version(&env.version)?;
        if (major, minor) < (1, 25) {
            bail!(
                "go.work development requires Go 1.25 or newer; found {}",
                env.version
            );
        }
        let work = PathBuf::from(env.work);
        return Ok(GoContext {
            root: work.parent().unwrap_or(Path::new(".")).to_path_buf(),
            pattern: "work".into(),
            workspace: true,
        });
    }
    if !env.module.is_empty() && env.module != "/dev/null" && env.module != "NUL" {
        let module = PathBuf::from(env.module);
        return Ok(GoContext {
            root: module.parent().unwrap_or(Path::new(".")).to_path_buf(),
            pattern: "./...".into(),
            workspace: false,
        });
    }
    bail!("not a Go module (no go.work or go.mod found)")
}

pub fn load(context: &GoContext) -> Result<Vec<ListedPackage>> {
    const FIELDS: &str = "Name,ImportPath,Dir,Deps,TestImports,XTestImports,GoFiles,CgoFiles,CFiles,HFiles,SFiles,CXXFiles,MFiles,FFiles,SysoFiles,EmbedFiles,EmbedPatterns,TestGoFiles,XTestGoFiles,Module,Standard,Error";
    let output = Command::new("go")
        .args(["list", "-e", &format!("-json={FIELDS}"), &context.pattern])
        .current_dir(&context.root)
        .output()
        .with_context(|| format!("run go list in {}", context.root.display()))?;
    let stream = serde_json::Deserializer::from_slice(&output.stdout).into_iter::<ListedPackage>();
    let packages = stream
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse go list JSON stream")?;
    if packages.is_empty() && !output.status.success() {
        bail!(
            "go list failed in {}: {}",
            context.root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(packages)
}

fn parse_version(version: &str) -> Result<(u32, u32)> {
    let version = version
        .strip_prefix("go")
        .with_context(|| format!("unrecognized Go version {version:?}"))?;
    let mut parts = version.split(['.', 'r', 'c']);
    let major = parts.next().unwrap_or_default().parse()?;
    let minor = parts.next().unwrap_or_default().parse()?;
    Ok((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_prerelease_versions() {
        assert_eq!(parse_version("go1.25.3").unwrap(), (1, 25));
        assert_eq!(parse_version("go1.27rc1").unwrap(), (1, 27));
    }
}
