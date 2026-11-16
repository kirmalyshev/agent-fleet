//! Embeds the commit this binary is built from as `AGENT_FLEET_COMMIT`; outside a git checkout
//! it is left unset and the binary reports no commit. `AGENT_FLEET_VERSION` is what `--version`
//! prints: the crate version, then the commit when there is one.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let Some(sha) = git(&["rev-parse", "HEAD"]) else {
        println!("cargo:rustc-env=AGENT_FLEET_VERSION={version}");
        return;
    };
    println!("cargo:rustc-env=AGENT_FLEET_COMMIT={sha}");
    println!("cargo:rustc-env=AGENT_FLEET_VERSION={version} {sha}");
    // HEAD moves on checkout; logs/HEAD on every commit, pull and reset.
    for p in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
