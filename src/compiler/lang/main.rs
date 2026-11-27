//! nasc: naspeed's own Rust-subset compiler for live patch bodies.
//!
//! `nasc batch <sig-dir> <source-root>`
//! For every `nasc-sig-<i>.txt` in <sig-dir>, parse the Rust source tree once,
//! compile the function named by the sig's `want` record to LLVM IR text and
//! write `impl-<i>.ll` next to the sig. A function outside the supported
//! subset produces no impl file; the caller falls back to cloning rustc's
//! bitcode. Exit 0 always unless the batch itself was unusable.

mod ir;
mod lex;
mod parse;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "map" {
        // `nasc map <source-root> <selector-file>`: print the items map for
        // the functions of the source file named on line 1 of
        // <selector-file>'s directory. Used by `ns patch` to diff bodies
        // against the persisted build-time ledger.
        let mut files = Vec::new();
        collect(Path::new(&args[2]), &mut files);
        let source_file = std::fs::read_to_string(Path::new(&args[3]).join("nasc-source.txt"))
            .ok()
            .map(|s| PathBuf::from(s.trim()));
        files.sort_by_key(|p| {
            if source_file.as_ref().is_some_and(|s| *p == *s) { 0 }
            else { match p.file_name().map(|n| n.to_string_lossy().into_owned()).as_deref() {
                Some("main.rs") | Some("lib.rs") => 1, _ => 2 } }
        });
        for file in files {
            let Ok(src) = std::fs::read_to_string(&file) else { continue };
            let Ok(items) = parse::parse_file(&src) else { continue };
            for f in items { println!("{}", item_line(&f)); }
        }
        return;
    }
    if args.len() != 4 || args[1] != "batch" {
        eprintln!("usage: {} batch <sig-dir> <source-root>", args[0]);
        std::process::exit(1);
    }
    let (sig_dir, root) = (&args[2], &args[3]);

    // Parse every .rs once, index functions by leaf name. Crate entry
    // points rank first: `src/bin/*.rs` and sibling targets may define a
    // same-named function, but the binary being patched resolves through
    // its own main.rs/lib.rs.
    let mut files = Vec::new();
    collect(Path::new(root), &mut files);
    // The target's own source file wins: rustc's input path reaches us
    // as nasc-source.txt, so a same-named fn in a sibling bin or lib
    // target can never shadow the file the patch is built from.
    let source_file = std::fs::read_to_string(Path::new(sig_dir).join("nasc-source.txt"))
        .ok()
        .map(|s| PathBuf::from(s.trim()));
    files.sort_by_key(|p| {
        if source_file.as_ref().is_some_and(|s| *p == *s) {
            0
        } else {
            match p.file_name().map(|n| n.to_string_lossy().into_owned()).as_deref() {
                Some("main.rs") | Some("lib.rs") => 1,
                _ => 2,
            }
        }
    });
    let mut fns: BTreeMap<String, Vec<parse::FnItem>> = BTreeMap::new();
    for file in files {
        let Ok(src) = std::fs::read_to_string(&file) else { continue };
        let items = match parse::parse_file(&src) {
            Ok(i) => i,
            Err(_) => continue,
        };
        for f in items {
            let leaf = f.name.rsplit("::").next().unwrap_or(&f.name).to_string();
            fns.entry(leaf).or_default().push(f);
        }
    }

    // The item map is the fast path's freshness ledger: `ns patch` diffs
    // selector body hashes and refuses on any non-selector drift.
    let mut map = String::new();
    for cands in fns.values() {
        for f in cands {
            map.push_str(&item_line(f));
            map.push('\n');
        }
    }
    let _ = std::fs::write(Path::new(sig_dir).join("items.map"), &map);
    if std::env::var_os("NS_ITEMS_MAP").is_some() {
        print!("{map}");
    }

    let Ok(rd) = std::fs::read_dir(sig_dir) else { std::process::exit(1) };
    let mut sigs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("nasc-sig-")))
        .collect();
    sigs.sort();
    for sig_path in sigs {
        let Some(stem) = sig_path.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        let index = stem
            .trim_start_matches("nasc-sig-")
            .trim_end_matches(".txt");
        let out_path = sig_path.with_file_name(format!("impl-{index}.ll"));
        let Ok(sig_text) = std::fs::read_to_string(&sig_path) else { continue };
        let Ok(sig) = ir::parse_sig(&sig_text) else { continue };
        let Some(want) = sig.want.clone() else { continue };
        let leaf = want.rsplit("::").next().unwrap_or(&want);
        let Some(cands) = fns.get(leaf) else {
            eprintln!("nasc: {want}: not found");
            continue;
        };
        // Prefer the exact qualified path when the selector has one.
        let chosen: Option<&parse::FnItem> = if want.contains("::") {
            cands.iter().find(|f| f.name == want).or(cands.first())
        } else {
            cands.first()
        };
        let Some(f) = chosen else { continue };
        match ir::emit(f, &sig) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&out_path, text) {
                    eprintln!("nasc: write {}: {e}", out_path.display());
                }
            }
            Err(e) => eprintln!("nasc: {want}: {e:?}"),
        }
    }
}

/// Body hash for the items map: the parsed expression tree's debug text.
/// `Debug` is derived, deterministic, and covers every meaningful token —
/// comments and formatting never reach the AST, so they hash identically.
fn body_hash(f: &parse::FnItem) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}|{:?}|{:?}", f.params, f.ret, f.body).hash(&mut h);
    h.finish()
}

fn item_line(f: &parse::FnItem) -> String {
    let params: Vec<String> = f.params.iter().map(|(n, t)| format!("{n}:{t:?}")).collect();
    format!("{}	{}	{:?}	{:016x}", f.name, params.join(","), f.ret, body_hash(f))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}
