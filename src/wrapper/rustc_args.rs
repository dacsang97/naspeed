// Portions adapted from sccache `src/compiler/rust.rs` (`parse_arguments`),
// Copyright 2016 Mozilla Foundation, licensed under the Apache License 2.0.
// Rewritten as synchronous code with a narrower, conservative flag table:
// any flag we do not recognise makes the invocation uncacheable.

//! Parse a rustc command line into ordered `(flag, value)` tokens plus the
//! fields that decide cacheability and the cache key.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One argument as rustc understands it. `flag` is canonical (`-C`, `--cfg`,
/// …); positional inputs use the empty flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub flag: String,
    pub value: Option<OsString>,
}

#[derive(Debug, Default, Clone)]
pub struct RustcInvocation {
    pub tokens: Vec<Token>,
    pub crate_name: Option<String>,
    pub crate_types: Vec<String>,
    pub inputs: Vec<OsString>,
    pub out_dir: Option<PathBuf>,
    /// Each `--emit` occurrence, split on commas (`link`, `metadata=path`, …).
    pub emit: Vec<Vec<String>>,
    pub extra_filename: Option<String>,
    pub externs: Vec<(String, Option<PathBuf>)>,
    /// `-L [kind=]path`, kind defaults to `all`.
    pub link_paths: Vec<(String, PathBuf)>,
    /// `-l [kind[:modifiers]=]name`, kind defaults to `dylib`.
    pub link_libs: Vec<(String, String)>,
    pub incremental: bool,
    pub test: bool,
    pub unstable: bool,
    pub print: bool,
    pub output_file: bool,
    pub target: Option<String>,
    /// Flags we do not know: they may take a value we would misparse.
    pub unknown_flags: Vec<String>,
}

/// Long flags whose value is the next argument unless written `--flag=value`.
const LONG_WITH_VALUE: &[&str] = &[
    "--allow",
    "--cap-lints",
    "--cfg",
    "--check-cfg",
    "--codegen",
    "--color",
    "--crate-name",
    "--crate-type",
    "--deny",
    "--diagnostic-width",
    "--edition",
    "--emit",
    "--env-set",
    "--error-format",
    "--explain",
    "--extern",
    "--forbid",
    "--force-warn",
    "--json",
    "--out-dir",
    "--print",
    "--remap-path-prefix",
    "--sysroot",
    "--target",
    "--warn",
];

const LONG_BOOL: &[&str] = &["--test", "--verbose", "--version", "--help"];

/// Short flags that take a value, attached (`-Copt=v`) or separate (`-C opt=v`).
const SHORT_WITH_VALUE: &[&str] = &["-C", "-L", "-l", "-Z", "-o", "-A", "-W", "-D", "-F"];

const SHORT_BOOL: &[&str] = &["-g", "-O", "-v", "-V", "-h"];

fn canonical_long(flag: &str) -> &str {
    match flag {
        "--codegen" => "-C",
        "--warn" => "-W",
        "--allow" => "-A",
        "--deny" => "-D",
        "--forbid" => "-F",
        other => other,
    }
}

/// Expand `@file` response files (one argument per line), relative to `cwd`.
pub fn expand_response_files(args: &[OsString], cwd: &Path) -> Result<Vec<OsString>> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg.to_str().and_then(|s| s.strip_prefix('@')) {
            Some(path) if !path.is_empty() => {
                let path = cwd.join(path);
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read response file {}", path.display()))?;
                out.extend(text.lines().map(OsString::from));
            }
            _ => out.push(arg.clone()),
        }
    }
    Ok(out)
}

/// Parse rustc arguments (without the program name).
pub fn parse(args: &[OsString]) -> RustcInvocation {
    let mut inv = RustcInvocation::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let Some(s) = arg.to_str() else {
            // Non-UTF-8 arguments only appear as paths; treat as an input.
            inv.inputs.push(arg.clone());
            inv.tokens.push(Token {
                flag: String::new(),
                value: Some(arg.clone()),
            });
            continue;
        };

        let (flag, value): (String, Option<OsString>) = if let Some(long) = s.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, v)) => (format!("--{name}"), Some(OsString::from(v))),
                None => (format!("--{long}"), None),
            };
            if LONG_WITH_VALUE.contains(&name.as_str()) {
                let value = match inline {
                    Some(v) => Some(v),
                    None => {
                        let v = args.get(i).cloned();
                        i += 1;
                        v
                    }
                };
                (canonical_long(&name).to_owned(), value)
            } else if LONG_BOOL.contains(&name.as_str()) && inline.is_none() {
                (name, None)
            } else {
                inv.unknown_flags.push(name.clone());
                (name, inline)
            }
        } else if s.starts_with('-') && s.len() > 1 {
            let short = &s[..2];
            if SHORT_WITH_VALUE.contains(&short) {
                let value = if s.len() > 2 {
                    Some(OsString::from(&s[2..]))
                } else {
                    let v = args.get(i).cloned();
                    i += 1;
                    v
                };
                (short.to_owned(), value)
            } else if SHORT_BOOL.contains(&s) {
                (s.to_owned(), None)
            } else {
                inv.unknown_flags.push(s.to_owned());
                (s.to_owned(), None)
            }
        } else {
            // Positional input (`-` is stdin).
            inv.inputs.push(arg.clone());
            (String::new(), Some(arg.clone()))
        };

        record(&mut inv, &flag, value.as_deref());
        inv.tokens.push(Token { flag, value });
    }
    inv
}

fn record(inv: &mut RustcInvocation, flag: &str, value: Option<&OsStr>) {
    let text = value.and_then(OsStr::to_str);
    match (flag, text) {
        ("--crate-name", Some(v)) => inv.crate_name = Some(v.to_owned()),
        ("--crate-type", Some(v)) => {
            inv.crate_types
                .extend(v.split(',').map(str::trim).map(str::to_owned));
        }
        ("--out-dir", _) => inv.out_dir = value.map(PathBuf::from),
        ("--emit", Some(v)) => inv.emit.push(v.split(',').map(str::to_owned).collect()),
        ("--extern", Some(v)) => {
            let (name, path) = match v.split_once('=') {
                Some((name, path)) => (name, Some(PathBuf::from(path))),
                None => (v, None),
            };
            // `--extern priv:name=path` / `noprelude:name`: keep the crate name only.
            let name = name.rsplit(':').next().unwrap_or(name);
            inv.externs.push((name.to_owned(), path));
        }
        ("-L", Some(v)) => {
            let (kind, path) = match v.split_once('=') {
                Some((kind, path))
                    if matches!(
                        kind,
                        "dependency" | "crate" | "native" | "framework" | "all"
                    ) =>
                {
                    (kind, path)
                }
                _ => ("all", v),
            };
            inv.link_paths.push((kind.to_owned(), PathBuf::from(path)));
        }
        ("-l", Some(v)) => {
            let (kind, name) = match v.split_once('=') {
                Some((kind, name)) => (kind.split(':').next().unwrap_or(kind), name),
                None => ("dylib", v),
            };
            // `name:rename` keeps the link name before the colon.
            let name = name.split(':').next().unwrap_or(name);
            inv.link_libs.push((kind.to_owned(), name.to_owned()));
        }
        ("-C", Some(v)) => {
            let (opt, val) = match v.split_once('=') {
                Some((opt, val)) => (opt, Some(val)),
                None => (v, None),
            };
            match opt {
                "incremental" => inv.incremental = true,
                "extra-filename" => inv.extra_filename = val.map(str::to_owned),
                _ => {}
            }
        }
        ("-Z", _) => inv.unstable = true,
        ("--test", _) => inv.test = true,
        ("--print", _) => inv.print = true,
        ("-o", _) => inv.output_file = true,
        ("--target", Some(v)) => inv.target = Some(v.to_owned()),
        _ => {}
    }
}

impl RustcInvocation {
    /// `<crate_name><extra_filename>`, the stem shared by every output file.
    pub fn output_stem(&self) -> Option<String> {
        let name = self.crate_name.as_deref()?;
        Some(format!(
            "{name}{}",
            self.extra_filename.as_deref().unwrap_or("")
        ))
    }

    pub fn emit_kinds(&self) -> impl Iterator<Item = &str> {
        self.emit.iter().flatten().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_cargo_registry_invocation() {
        let line = include_str!("../../tests/fixtures/rustc_args/itoa_registry.txt");
        let args: Vec<OsString> = line
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(OsString::from)
            .collect();
        let inv = parse(&args);
        assert_eq!(inv.crate_name.as_deref(), Some("itoa"));
        assert_eq!(inv.crate_types, vec!["lib"]);
        assert_eq!(inv.inputs.len(), 1);
        assert_eq!(inv.extra_filename.as_deref(), Some("-b44586fc39d2b7cf"));
        assert_eq!(inv.output_stem().as_deref(), Some("itoa-b44586fc39d2b7cf"));
        assert!(inv.out_dir.as_ref().unwrap().ends_with("debug/deps"));
        assert_eq!(
            inv.emit_kinds().collect::<Vec<_>>(),
            vec!["dep-info", "metadata", "link"]
        );
        assert!(inv.unknown_flags.is_empty(), "{:?}", inv.unknown_flags);
        assert!(!inv.incremental && !inv.test && !inv.unstable);
    }

    #[test]
    fn separate_and_inline_values() {
        let inv = parse(&os(&[
            "--crate-name",
            "foo",
            "--edition=2021",
            "src/lib.rs",
            "-Cincremental=/tmp/inc",
            "-L",
            "native=/x/out",
            "-Ldependency=/x/deps",
            "-l",
            "static:+bundle=z",
            "--extern",
            "noprelude:bar=/x/libbar.rmeta",
            "--extern",
            "proc_macro",
            "--test",
        ]));
        assert!(inv.incremental && inv.test);
        assert_eq!(inv.inputs, vec![OsString::from("src/lib.rs")]);
        assert_eq!(
            inv.link_paths,
            vec![
                ("native".to_owned(), PathBuf::from("/x/out")),
                ("dependency".to_owned(), PathBuf::from("/x/deps"))
            ]
        );
        assert_eq!(inv.link_libs, vec![("static".to_owned(), "z".to_owned())]);
        assert_eq!(
            inv.externs,
            vec![
                ("bar".to_owned(), Some(PathBuf::from("/x/libbar.rmeta"))),
                ("proc_macro".to_owned(), None)
            ]
        );
    }

    #[test]
    fn stdin_and_unknown_flags() {
        let inv = parse(&os(&[
            "-",
            "--crate-name",
            "___",
            "--print=file-names",
            "--frobnicate",
        ]));
        assert_eq!(inv.inputs, vec![OsString::from("-")]);
        assert!(inv.print);
        assert_eq!(inv.unknown_flags, vec!["--frobnicate".to_owned()]);
    }

    #[test]
    fn response_file_expands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("args.txt"),
            "--crate-name\nfoo\n--cfg\nfeature=\"x\"\n",
        )
        .unwrap();
        let expanded =
            expand_response_files(&os(&["@args.txt", "src/lib.rs"]), dir.path()).unwrap();
        assert_eq!(
            expanded,
            os(&[
                "--crate-name",
                "foo",
                "--cfg",
                "feature=\"x\"",
                "src/lib.rs"
            ])
        );
    }
}
