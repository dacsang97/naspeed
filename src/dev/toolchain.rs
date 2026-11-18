//! Toolchain selection and the shared ownership boundary for `ns dev`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::go_dev::{GoDev, GoGraph, executable_name};
use crate::project::ns_config::DevToolchainName;
use crate::project::{NsConfig, Project, WorkspaceGraph};

pub struct CargoDev {
    pub project: Project,
}

pub enum DevToolchain {
    Cargo(CargoDev),
    Go(GoDev),
}

impl DevToolchain {
    pub fn load_cwd() -> Result<DevToolchain> {
        Self::load(&std::env::current_dir()?)
    }

    pub fn load(dir: &Path) -> Result<DevToolchain> {
        let marker_root = nearest_marker_root(dir).with_context(|| {
            format!(
                "no Cargo.toml, go.work, or go.mod found from {}",
                dir.display()
            )
        })?;
        let cargo = marker_root.join("Cargo.toml").is_file();
        let go = marker_root.join("go.work").is_file() || marker_root.join("go.mod").is_file();
        let configured = NsConfig::load(&marker_root)?.dev.toolchain;
        let selected = match (cargo, go, configured) {
            (true, true, None) => bail!(
                "both {} and a Go module file exist; set [dev].toolchain = \"cargo\" or \"go\" in {}",
                marker_root.join("Cargo.toml").display(),
                marker_root.join("ns.toml").display()
            ),
            (true, _, Some(DevToolchainName::Cargo)) | (true, false, None) => {
                DevToolchainName::Cargo
            }
            (_, true, Some(DevToolchainName::Go)) | (false, true, None) => DevToolchainName::Go,
            (false, true, Some(DevToolchainName::Cargo)) => {
                bail!("[dev].toolchain = \"cargo\" but no Cargo.toml was found")
            }
            (true, false, Some(DevToolchainName::Go)) => {
                bail!("[dev].toolchain = \"go\" but no go.work or go.mod was found")
            }
            (false, false, _) => unreachable!("marker root always has a marker"),
        };
        match selected {
            DevToolchainName::Cargo => Ok(DevToolchain::Cargo(CargoDev {
                project: Project::load(dir)?,
            })),
            DevToolchainName::Go => Ok(DevToolchain::Go(GoDev::load(dir)?)),
        }
    }

    pub fn cargo_graph(&self) -> Option<&WorkspaceGraph> {
        match self {
            DevToolchain::Cargo(cargo) => Some(&cargo.project.graph),
            DevToolchain::Go(_) => None,
        }
    }

    pub fn go(&self) -> Option<&GoDev> {
        match self {
            DevToolchain::Go(go) => Some(go),
            DevToolchain::Cargo(_) => None,
        }
    }

    pub fn go_mut(&mut self) -> Option<&mut GoDev> {
        match self {
            DevToolchain::Go(go) => Some(go),
            DevToolchain::Cargo(_) => None,
        }
    }

    pub fn go_graph(&self) -> Option<&GoGraph> {
        self.go().map(|go| &go.graph)
    }

    pub fn target_dir(&self) -> Option<PathBuf> {
        self.cargo_graph().map(|graph| graph.target_dir.clone())
    }

    pub fn is_go(&self) -> bool {
        matches!(self, DevToolchain::Go(_))
    }

    pub fn final_binary_path(&self, bin: &str, current: &Path) -> PathBuf {
        self.go()
            .map(|go| go.output_dir().join("bin").join(executable_name(bin)))
            .unwrap_or_else(|| current.to_path_buf())
    }

    pub fn validate_staged_binary(&self, bin: &str, staged: &Path) -> Result<()> {
        let final_path = self.final_binary_path(bin, staged);
        if staged != final_path && !staged.is_file() {
            bail!(
                "built binary {} is missing; previous process keeps running",
                staged.display()
            );
        }
        Ok(())
    }

    pub fn finalize_binary(&self, bin: &str, staged: &Path) -> Result<PathBuf> {
        if self.go().is_none() {
            return Ok(staged.to_path_buf());
        }
        let final_path = self.final_binary_path(bin, staged);
        if staged == final_path {
            return Ok(final_path);
        }
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let backup = final_path.with_extension("ns-dev-old");
        let _ = std::fs::remove_file(&backup);
        if final_path.exists() {
            std::fs::rename(&final_path, &backup)
                .with_context(|| format!("prepare to replace {}", final_path.display()))?;
        }
        if let Err(error) = std::fs::rename(staged, &final_path) {
            let install_error = anyhow::Error::new(error).context(format!(
                "install {} as {}",
                staged.display(),
                final_path.display()
            ));
            if backup.exists() {
                std::fs::rename(&backup, &final_path).with_context(|| {
                    format!(
                        "restore {} after failed install: {install_error:#}",
                        final_path.display()
                    )
                })?;
            }
            return Err(install_error);
        }
        let _ = std::fs::remove_file(backup);
        Ok(final_path)
    }
}

fn nearest_marker_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| {
            ancestor.join("Cargo.toml").is_file()
                || ancestor.join("go.work").is_file()
                || ancestor.join("go.mod").is_file()
        })
        .map(Path::to_path_buf)
}

pub fn is_go_only_dir(dir: &Path) -> bool {
    let Some(root) = nearest_marker_root(dir) else {
        return false;
    };
    !root.join("Cargo.toml").is_file()
        && (root.join("go.work").is_file() || root.join("go.mod").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_toolchains_require_an_explicit_choice() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            temp.path().join("go.mod"),
            "module example.test/m\n\ngo 1.25\n",
        )
        .unwrap();
        let error = match DevToolchain::load(temp.path()) {
            Ok(_) => panic!("ambiguous project unexpectedly loaded"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("[dev].toolchain"), "{error}");
    }

    #[test]
    fn identifies_go_only_roots_for_cargo_only_command_errors() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("go.mod"),
            "module example.test/m\n\ngo 1.25\n",
        )
        .unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        assert!(is_go_only_dir(&temp.path().join("nested")));
    }
}
