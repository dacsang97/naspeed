//! Find `RUSTC_WRAPPER` set in the environment or in shell rc files. An env
//! `RUSTC_WRAPPER` overrides `build.rustc-wrapper`, so naspeed would be
//! bypassed. We only report; rc files are the owner's to edit.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: PathBuf,
    pub line_no: usize,
    pub line: String,
}

pub fn rc_files(home: &Path) -> Vec<PathBuf> {
    [
        ".zshrc",
        ".zprofile",
        ".zshenv",
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".config/fish/config.fish",
        ".config/environment.d/rust.conf",
    ]
    .iter()
    .map(|f| home.join(f))
    .collect()
}

pub fn scan(files: &[PathBuf]) -> Vec<Finding> {
    let mut out = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.contains("RUSTC_WRAPPER") {
                out.push(Finding {
                    file: file.clone(),
                    line_no: i + 1,
                    line: line.to_owned(),
                });
            }
        }
    }
    out
}

/// Human-readable warnings; empty when nothing overrides the config.
pub fn report(home: &Path, naspeed_bin: &Path) -> Vec<String> {
    let mut msgs = Vec::new();
    if let Some(value) = std::env::var_os("RUSTC_WRAPPER")
        && Path::new(&value) != naspeed_bin
        && !value.is_empty()
    {
        msgs.push(format!(
            "RUSTC_WRAPPER={} is set in this shell and overrides build.rustc-wrapper; naspeed will be bypassed until it is unset.",
            value.to_string_lossy()
        ));
    }
    for f in scan(&rc_files(home)) {
        msgs.push(format!(
            "{}:{}: `{}` — remove this line (then open a new shell) so cargo uses naspeed.",
            f.file.display(),
            f.line_no,
            f.line.trim()
        ));
    }
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_uncommented_exports_only() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        std::fs::write(
            &rc,
            "export PATH=x\n# export RUSTC_WRAPPER=old\nexport RUSTC_WRAPPER=sccache\n",
        )
        .unwrap();
        let found = scan(std::slice::from_ref(&rc));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line_no, 3);
    }
}
