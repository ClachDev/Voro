//! Regression test for the `VORO_TASK_ID` leak (task #184): the `voro` binary
//! must read no environment when resolving `propose`'s discovered-from source.
//! A dispatched session runs with `VORO_TASK_ID` exported, and before this fix a
//! bare `propose` picked it up as the `--from` default — so `cargo test` run
//! inside such a session failed when that id did not exist in the scratch db.
//!
//! The test drives the real binary with `VORO_TASK_ID=999` set on the child
//! (safe: it is the child's environment, not this process's) and asserts a bare
//! `propose` links nothing, while an explicit `--from` still links — proving the
//! ambient id is ignored but the flag works even when the env var is present.

use std::path::Path;
use std::process::Command;

/// Run the built `voro` with `VORO_TASK_ID=999` exported, against `db`, and
/// return its stdout. Panics if the command does not exit successfully.
fn voro(db: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_voro"))
        .arg("--db")
        .arg(db)
        .args(args)
        .env("VORO_TASK_ID", "999")
        .output()
        .expect("spawn voro");
    assert!(
        output.status.success(),
        "voro {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 stdout")
}

#[test]
fn propose_ignores_ambient_voro_task_id() {
    let root = std::env::temp_dir().join(format!(
        "voro-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("voro.db");

    voro(&db, &["project", "add", "demo", root.to_str().unwrap()]);
    voro(&db, &["add", "demo", "Source", "--state", "ready"]);

    // A bare propose under a set VORO_TASK_ID=999 must not link discovered-from
    // #999 (which does not exist and would error) — it links nothing.
    let orphan = voro(&db, &["propose", "demo", "Orphan"]);
    assert!(orphan.contains("proposed"), "{orphan}");
    assert!(!orphan.contains("discovered from"), "{orphan}");

    // The explicit flag still links, even with the env var set — the source is
    // the flag, never the environment.
    let follow = voro(&db, &["propose", "demo", "Follow-up", "--from", "1"]);
    assert!(follow.contains("discovered from #1"), "{follow}");

    std::fs::remove_dir_all(&root).ok();
}
