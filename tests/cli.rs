mod common;

use assert_cmd::Command;
use common::Fixture;
use serde_json::json;

/// `midden prune | head` must behave like any Unix filter when the reader
/// closes early: die of SIGPIPE, not panic with a broken-pipe error (exit 101).
#[cfg(unix)]
#[test]
fn early_closed_stdout_pipe_kills_with_sigpipe_instead_of_panicking() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    // SIGPIPE is 13 on every platform this crate supports.
    const SIGPIPE: i32 = 13;

    let fx = Fixture::new();
    // Enough orphaned entries that the report cannot fit any kernel pipe
    // buffer, so the child is still writing after the read end is closed.
    let mut projects = serde_json::Map::new();
    for i in 0..5000 {
        projects.insert(format!("/no/such/dir/entry-{i:04}"), json!({}));
    }
    fx.write_config(json!(projects), json!({}));

    let mut child = Command::new(assert_cmd::cargo::cargo_bin("midden"))
        .arg("--color")
        .arg("never")
        .arg("--config")
        .arg(&fx.config)
        .arg("--claude-home")
        .arg(&fx.claude_home)
        .arg("--codex-home")
        .arg(&fx.codex_home)
        .arg("prune")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn midden");
    drop(child.stdout.take());
    let status = child.wait().expect("wait for midden");

    assert_ne!(
        status.code(),
        Some(101),
        "a closed stdout pipe must not panic"
    );
    assert_eq!(
        status.signal(),
        Some(SIGPIPE),
        "expected death by SIGPIPE, got {status:?}"
    );
}

#[test]
fn bash_completions_are_generated() {
    Command::cargo_bin("midden")
        .unwrap()
        .arg("completions")
        .arg("bash")
        .assert()
        .success()
        .stdout(predicates::str::contains("_midden()"));
}
