//! `archive.name` templates and the `{os}`/`{arch}` names (as in OCI
//! platforms: `linux`/`amd64`) derived from Rust target triples.

use anyhow::{Result, bail};

use super::target_matrix::{BuildKind, UNIVERSAL2, classify};

/// Values a template may reference.
pub struct NameVars<'a> {
    pub project: &'a str,
    pub id: &'a str,
    pub binary: &'a str,
    pub version: &'a str,
    /// Empty for snapshots.
    pub tag: &'a str,
    pub target: &'a str,
}

/// Expand `{placeholder}`s; an unknown or unclosed placeholder is an error.
pub fn render(template: &str, vars: &NameVars) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('}') else {
            bail!("archive.name {template:?}: unclosed `{{`");
        };
        let key = &rest[start + 1..start + len];
        let value = match key {
            "project" => vars.project.to_string(),
            "id" => vars.id.to_string(),
            "binary" => vars.binary.to_string(),
            "version" => vars.version.to_string(),
            "tag" => vars.tag.to_string(),
            "target" => vars.target.to_string(),
            "os" => go_os(vars.target)?.to_string(),
            "arch" => go_arch(vars.target)?.to_string(),
            other => bail!(
                "archive.name {template:?}: unknown placeholder {{{other}}} (known: project, id, binary, version, tag, os, arch, target)"
            ),
        };
        out.push_str(&value);
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    if out.is_empty() || out.contains('/') || out.contains('\\') || out.starts_with('.') {
        bail!("archive.name {template:?} gives an invalid file name {out:?}");
    }
    Ok(out)
}

/// The triple a release target builds for (`.2.17` glibc pins stripped).
fn triple(target: &str) -> String {
    match classify(target) {
        BuildKind::PinnedGlibc { triple, .. } => triple,
        BuildKind::Cargo | BuildKind::Universal2 => target.to_string(),
    }
}

/// OS name for a target: `linux`, `darwin`, `windows`, ...
pub fn go_os(target: &str) -> Result<&'static str> {
    let t = triple(target);
    let os = if t.contains("-linux-") {
        if t.contains("-android") {
            "android"
        } else {
            "linux"
        }
    } else if t.contains("-apple-darwin") {
        "darwin"
    } else if t.contains("-windows-") {
        "windows"
    } else if t.contains("-freebsd") {
        "freebsd"
    } else if t.contains("-netbsd") {
        "netbsd"
    } else if t.contains("-openbsd") {
        "openbsd"
    } else if t.contains("-illumos") {
        "illumos"
    } else {
        bail!("no os name for target {target}");
    };
    Ok(os)
}

/// Arch name for a target: `amd64`, `arm64`, `386`; `all` for universal2.
pub fn go_arch(target: &str) -> Result<&'static str> {
    if target == UNIVERSAL2 {
        return Ok("all");
    }
    let t = triple(target);
    let cpu = t.split('-').next().unwrap_or_default();
    let arch = match cpu {
        "x86_64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        "i386" | "i586" | "i686" => "386",
        "riscv64gc" | "riscv64" => "riscv64",
        "powerpc64le" => "ppc64le",
        "powerpc64" => "ppc64",
        "s390x" => "s390x",
        "loongarch64" => "loong64",
        c if c.starts_with("arm") || c.starts_with("thumbv7") => "arm",
        _ => bail!("no arch name for target {target}"),
    };
    Ok(arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(target: &str) -> NameVars<'_> {
        NameVars {
            project: "proj",
            id: "noui",
            binary: "api-noui",
            version: "1.2.0",
            tag: "v1.2.0",
            target,
        }
    }

    #[test]
    fn renders_os_arch_names() {
        let v = vars("x86_64-unknown-linux-musl");
        assert_eq!(
            render("{binary}_{version}_{os}_{arch}", &v).unwrap(),
            "api-noui_1.2.0_linux_amd64"
        );
        assert_eq!(
            render("{project}-{id}-{tag}-{target}", &v).unwrap(),
            "proj-noui-v1.2.0-x86_64-unknown-linux-musl"
        );
        assert_eq!(
            render("{binary}_{os}_{arch}", &vars(UNIVERSAL2)).unwrap(),
            "api-noui_darwin_all"
        );
        assert_eq!(
            render("{os}_{arch}", &vars("aarch64-unknown-linux-gnu.2.17")).unwrap(),
            "linux_arm64"
        );
        assert_eq!(
            render("{os}_{arch}", &vars("i686-pc-windows-msvc")).unwrap(),
            "windows_386"
        );
    }

    #[test]
    fn rejects_bad_templates() {
        let v = vars("x86_64-unknown-linux-musl");
        let err = render("{binary}_{goos}", &v).unwrap_err().to_string();
        assert!(err.contains("{goos}"), "{err}");
        assert!(render("{binary", &v).is_err());
        assert!(render("a/{binary}", &v).is_err());
        assert!(render("{os}", &vars("wasm32-unknown-unknown")).is_err());
        // Without {os}, an exotic target is fine.
        assert_eq!(
            render("{binary}", &vars("wasm32-unknown-unknown")).unwrap(),
            "api-noui"
        );
    }
}
