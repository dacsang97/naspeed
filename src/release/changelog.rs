//! Release notes: `## Changelog` followed by one
//! `* <short sha> <subject>` line per commit, filtered by `changelog.exclude`
//! regexes and sorted oldest first (`asc`) or newest first (`desc`).

use anyhow::{Context, Result};
use regex::Regex;

use crate::project::ns_config::{ChangelogConfig, ChangelogSort};

/// Compiled `changelog.exclude`; checked before any build so a typo fails fast.
pub fn compile(cfg: &ChangelogConfig) -> Result<Vec<Regex>> {
    cfg.exclude
        .iter()
        .map(|r| Regex::new(r).with_context(|| format!("invalid changelog.exclude regex {r:?}")))
        .collect()
}

/// `commits` are `(short sha, subject)`, newest first (git log order).
pub fn render(commits: &[(String, String)], sort: ChangelogSort, exclude: &[Regex]) -> String {
    let mut kept: Vec<&(String, String)> = commits
        .iter()
        .filter(|(_, subject)| !exclude.iter().any(|r| r.is_match(subject)))
        .collect();
    if sort == ChangelogSort::Asc {
        kept.reverse();
    }
    let mut body = String::from("## Changelog\n");
    for (sha, subject) in kept {
        body.push_str(&format!("* {sha} {subject}\n"));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_sorts() {
        let commits: Vec<(String, String)> = [
            ("ccc", "feat: third"),
            ("bbb", "docs: second"),
            ("aaa", "fix: first"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let cfg = ChangelogConfig {
            sort: ChangelogSort::Asc,
            exclude: vec!["^docs:".into()],
        };
        let ex = compile(&cfg).unwrap();
        assert_eq!(
            render(&commits, ChangelogSort::Asc, &ex),
            "## Changelog\n* aaa fix: first\n* ccc feat: third\n"
        );
        assert_eq!(
            render(&commits, ChangelogSort::Desc, &[]),
            "## Changelog\n* ccc feat: third\n* bbb docs: second\n* aaa fix: first\n"
        );
        let bad = ChangelogConfig {
            sort: ChangelogSort::Asc,
            exclude: vec!["(".into()],
        };
        assert!(compile(&bad).is_err());
    }
}
