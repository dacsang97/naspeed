//! Tags pushed for the image index: `--tag` as given, else `[image].tags`
//! rendered with `{version}`, `{tag}`, `{commit}` and `{short_commit}`
//! (`[image].tags`). A template that renders empty (`{tag}` on a
//! snapshot) is skipped, and `latest` is never moved by a snapshot or a
//! pre-release.

use anyhow::{Result, bail};

use super::reference::{is_valid_tag, tag_from_version};

/// Values for the placeholders, and whether `latest` must be skipped.
pub struct TagContext<'a> {
    pub version: &'a str,
    pub tag: Option<&'a str>,
    pub commit: Option<&'a str>,
    /// Snapshot or pre-release.
    pub skip_latest: bool,
}

pub fn resolve_tags(
    explicit: &[String],
    templates: &[String],
    ctx: &TagContext,
) -> Result<Vec<String>> {
    if !explicit.is_empty() {
        if let Some(bad) = explicit.iter().find(|t| !is_valid_tag(t)) {
            bail!("invalid tag {bad:?}");
        }
        return Ok(dedup(explicit.to_vec()));
    }
    let mut tags = Vec::new();
    for template in templates {
        let rendered = render(template, ctx)?;
        if rendered.is_empty() {
            eprintln!("ns image: tag {template:?} is empty for this release; skipping it");
            continue;
        }
        let tag = rendered;
        if !is_valid_tag(&tag) {
            bail!("[image].tags entry {template:?} renders to invalid tag {tag:?}");
        }
        if tag == "latest" && ctx.skip_latest {
            eprintln!("ns image: not tagging latest (snapshot or pre-release)");
            continue;
        }
        tags.push(tag);
    }
    Ok(dedup(tags))
}

fn dedup(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for tag in tags {
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

fn render(template: &str, ctx: &TagContext) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            bail!("[image].tags entry {template:?}: unclosed `{{`");
        };
        let name = &rest[open + 1..open + close];
        let commit = ctx.commit.unwrap_or_default();
        let value = match name {
            "version" => ctx.version,
            "tag" => ctx.tag.unwrap_or_default(),
            "commit" => commit,
            "short_commit" => commit.get(..7).unwrap_or(commit),
            other => bail!(
                "[image].tags entry {template:?}: unknown placeholder {{{other}}} (use version, tag, commit or short_commit)"
            ),
        };
        // Versions may hold `+build` metadata, which tags cannot.
        if !value.is_empty() {
            out.push_str(&tag_from_version(value));
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    const RELEASE: TagContext<'static> = TagContext {
        version: "1.2.0",
        tag: Some("v1.2.0"),
        commit: Some("0123456789abcdef0123456789abcdef01234567"),
        skip_latest: false,
    };

    #[test]
    fn templates_render_every_placeholder() {
        let tags = resolve_tags(
            &[],
            &strings(&[
                "{version}",
                "{tag}",
                "sha-{short_commit}",
                "{commit}",
                "latest",
                "{version}",
            ]),
            &RELEASE,
        )
        .unwrap();
        assert_eq!(
            tags,
            strings(&[
                "1.2.0",
                "v1.2.0",
                "sha-0123456",
                "0123456789abcdef0123456789abcdef01234567",
                "latest"
            ])
        );
        let build_meta = TagContext {
            version: "1.2.0+build.5",
            ..RELEASE
        };
        assert_eq!(
            resolve_tags(&[], &strings(&["{version}"]), &build_meta).unwrap(),
            strings(&["1.2.0-build.5"])
        );
    }

    #[test]
    fn snapshots_and_prereleases_skip_latest_and_empty_tags() {
        let snapshot = TagContext {
            version: "1.2.1-SNAPSHOT-0123456",
            tag: None,
            skip_latest: true,
            ..RELEASE
        };
        assert_eq!(
            resolve_tags(&[], &strings(&["{tag}", "{version}", "latest"]), &snapshot).unwrap(),
            strings(&["1.2.1-SNAPSHOT-0123456"])
        );
        assert!(
            resolve_tags(&[], &strings(&["{tag}", "latest"]), &snapshot)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn explicit_tags_override_templates() {
        let snapshot = TagContext {
            skip_latest: true,
            ..RELEASE
        };
        assert_eq!(
            resolve_tags(
                &strings(&["edge", "latest"]),
                &strings(&["{version}"]),
                &snapshot
            )
            .unwrap(),
            strings(&["edge", "latest"])
        );
        assert!(resolve_tags(&strings(&["bad tag"]), &[], &RELEASE).is_err());
    }

    #[test]
    fn bad_templates_are_errors() {
        for bad in ["{Version}", "{version", "v{ver}", "{}"] {
            assert!(
                resolve_tags(&[], &strings(&[bad]), &RELEASE).is_err(),
                "{bad}"
            );
        }
        assert!(resolve_tags(&[], &strings(&["a/b"]), &RELEASE).is_err());
    }
}
