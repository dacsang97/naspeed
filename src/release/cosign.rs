//! Keyless cosign signature of the checksum file (`<name>.sig` + `<name>.pem`).
//!
//! Signing runs only when `cosign` is on PATH and an OIDC identity is
//! available (GitHub Actions id-token or `SIGSTORE_ID_TOKEN`), or when
//! `--sign` forces it (cosign then opens its interactive browser flow).
//! Otherwise the release continues unsigned with "signing skipped: <reason>".

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::project::ns_config::SignMode;

/// Env vars that give cosign a non-interactive OIDC identity.
const OIDC_ENV: &[&[&str]] = &[
    &[
        "ACTIONS_ID_TOKEN_REQUEST_URL",
        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    ],
    &["SIGSTORE_ID_TOKEN"],
];

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Sign(PathBuf),
    Skip(String),
}

/// Pure decision so the rules are testable without cosign installed.
pub fn decide(
    mode: SignMode,
    forced: bool,
    cosign: Option<PathBuf>,
    env: &dyn Fn(&str) -> bool,
) -> Result<Decision> {
    if mode == SignMode::None && !forced {
        return Ok(Decision::Skip("sign = \"none\" in ns.toml".into()));
    }
    let Some(cosign) = cosign else {
        if forced {
            bail!("--sign given but cosign is not on PATH");
        }
        return Ok(Decision::Skip("cosign not found on PATH".into()));
    };
    let oidc = OIDC_ENV.iter().any(|vars| vars.iter().all(|v| env(v)));
    if !oidc && !forced {
        return Ok(Decision::Skip(
            "no CI OIDC identity (pass --sign to sign interactively)".into(),
        ));
    }
    Ok(Decision::Sign(cosign))
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// `cosign sign-blob --yes` writing `<file>.sig` and `<file>.pem`.
pub fn sign_blob(cosign: &Path, file: &Path) -> Result<(PathBuf, PathBuf)> {
    let sig = PathBuf::from(format!("{}.sig", file.display()));
    let pem = PathBuf::from(format!("{}.pem", file.display()));
    let status = Command::new(cosign)
        .arg("sign-blob")
        .arg("--yes")
        .arg("--output-signature")
        .arg(&sig)
        .arg("--output-certificate")
        .arg(&pem)
        .arg(file)
        .status()
        .context("run cosign sign-blob")?;
    if !status.success() {
        bail!("cosign sign-blob failed ({status})");
    }
    for p in [&sig, &pem] {
        if !p.is_file() {
            bail!("cosign did not write {}", p.display());
        }
    }
    Ok((sig, pem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_rules() {
        let none = |_: &str| false;
        let ci = |v: &str| v.starts_with("ACTIONS_ID_TOKEN");
        let cosign = || Some(PathBuf::from("/bin/cosign"));
        assert!(matches!(
            decide(SignMode::None, false, cosign(), &ci).unwrap(),
            Decision::Skip(_)
        ));
        assert!(
            matches!(decide(SignMode::Cosign, false, None, &ci).unwrap(), Decision::Skip(r) if r.contains("not found"))
        );
        assert!(
            matches!(decide(SignMode::Cosign, false, cosign(), &none).unwrap(), Decision::Skip(r) if r.contains("OIDC"))
        );
        assert_eq!(
            decide(SignMode::Cosign, false, cosign(), &ci).unwrap(),
            Decision::Sign("/bin/cosign".into())
        );
        assert_eq!(
            decide(SignMode::None, true, cosign(), &none).unwrap(),
            Decision::Sign("/bin/cosign".into())
        );
        assert!(decide(SignMode::Cosign, true, None, &none).is_err());
    }
}
