//! Runs the pseudo-terminal end-to-end suite (tests/pty/test_e2e.py) against
//! the release binary when python3 is available. Skipped otherwise.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn pty_end_to_end() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if Command::new("python3").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("python3 not available, skipping pty suite");
        return;
    }
    // Build the release binary the suite drives.
    let status = Command::new(env!("CARGO")).args(["build", "--release", "--quiet"]).current_dir(&root).status().expect("cargo");
    assert!(status.success(), "release build failed");
    let bin = root.join("target/release/bash-tools");
    let out = Command::new("python3")
        .arg(root.join("tests/pty/test_e2e.py"))
        .env("BASH_TOOLS_BIN", &bin)
        .current_dir(&root)
        .output()
        .expect("run python suite");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "pty suite failed:\n{stderr}");
}
