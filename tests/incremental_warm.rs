//! A workspace crate restored from the store gets its incremental state
//! rebuilt in the background, so the first edit in a fresh worktree is an
//! incremental compile rather than a full one.

use std::path::Path;
use std::time::{Duration, Instant};

mod common;

use common::Sandbox;

/// Finalized incremental sessions (`s-…`, not `-working`) under a profile.
fn finalized_sessions(profile: &Path) -> usize {
    let mut n = 0;
    for krate in std::fs::read_dir(profile.join("incremental"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for s in std::fs::read_dir(krate.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = s.file_name().to_string_lossy().into_owned();
            if name.starts_with("s-") && !name.ends_with("-working") && s.path().is_dir() {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn restored_workspace_crates_get_incremental_state() {
    let sb = Sandbox::new("warm");
    sb.run("t1", &["build"]);
    let cold = finalized_sessions(&sb.root.join("t1/debug"));
    assert!(
        cold > 0,
        "cargo keeps incremental sessions for workspace crates"
    );

    let out = sb
        .cargo("t2", &["build"])
        .env("NASPEED_WARM_INCREMENTAL", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (hits, _) = sb.usage();
    assert!(!hits.is_empty());

    // The warm-up runs detached at low priority; wait for its sessions.
    let deadline = Instant::now() + Duration::from_secs(120);
    let profile = sb.root.join("t2/debug");
    while finalized_sessions(&profile) < cold && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        finalized_sessions(&profile) >= cold,
        "{} of {cold} sessions rebuilt",
        finalized_sessions(&profile)
    );
    // Its throwaway outputs never replaced the restored ones.
    let rebuilt = sb.cargo("t2", &["build", "-v"]).output().unwrap();
    let text = String::from_utf8_lossy(&rebuilt.stderr);
    assert!(!text.contains("Compiling"), "nothing to rebuild: {text}");
}
