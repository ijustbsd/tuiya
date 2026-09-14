use std::process::Command;

/// Stamps the binary with a version.
///
/// Releases are cut from git tags, not from the `version` field in
/// `Cargo.toml`, so the number comes from `TUIYA_VERSION` when CI sets it and
/// from `git describe` otherwise. A build between two tags says so —
/// `0.2.0-3-gaecf094` — which beats reporting a number that shipped days ago.
fn main() {
    println!("cargo::rerun-if-env-changed=TUIYA_VERSION");
    println!("cargo::rerun-if-changed=.git/HEAD");

    let version = std::env::var("TUIYA_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo::rustc-env=TUIYA_VERSION={version}");
}

/// Falls back to `None` outside a git checkout — building from a source
/// tarball must not fail just because there is no history to read.
fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let described = String::from_utf8(output.stdout).ok()?;
    let described = described.trim().trim_start_matches('v');
    (!described.is_empty()).then(|| described.to_string())
}
