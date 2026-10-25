//! Sets `RFSCAN_VERSION`: the version from `Cargo.toml`, and for a development version (`-dev`)
//! also the short git commit hash, with `-dirty` when tracked files have uncommitted changes
//! (e.g. `0.5.0-dev+fd2fca4-dirty`). Without git the version from `Cargo.toml` alone.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    // A new commit, staging or a changed source file can change the hash or the dirty flag.
    for path in [
        ".git/HEAD",
        ".git/index",
        ".git/packed-refs",
        "src",
        "Cargo.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/{head}");
    }

    let base = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut version = base.clone();
    if base.contains("-dev")
        && let Some(hash) = git(&["rev-parse", "--short", "HEAD"])
    {
        version = format!("{base}+{hash}");
        let dirty =
            git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        if dirty {
            version.push_str("-dirty");
        }
    }
    println!("cargo:rustc-env=RFSCAN_VERSION={version}");
}
