//! Smoke test of the `paid-services-mock` binary. Writes only under `CARGO_TARGET_TMPDIR`,
//! in a new directory, and deletes nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_paid-services-mock");

fn wait_for(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn node_writes_a_config_and_obeys_control_commands() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("mock-cli")
        .join(format!("node-{}", std::process::id()));
    let mut child = Command::new(BIN)
        .args(["node", "--dir", dir.to_str().unwrap(), "--serve-secs", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let cfg = dir.join("sidecar.json");
    wait_for(&cfg);
    let cfg = cfg.to_str().unwrap();

    let out = Command::new(BIN)
        .args(["control", "--config", cfg, "mine", "2"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"tip\":4500002"));

    // Refuses to reuse a directory: it never overwrites the cookie or the config.
    let again = Command::new(BIN)
        .args(["node", "--dir", dir.to_str().unwrap(), "--serve-secs", "1"])
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(1));

    child.kill().unwrap();
    child.wait().unwrap();
}
