//! Embeds the commit this binary is built from as `AGENT_FLEET_COMMIT`; outside a git checkout
//! it is left unset and the binary reports no commit. `AGENT_FLEET_SHORT` is always set, for
//! `--version`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let sha = git(&["rev-parse", "HEAD"]);
    // `--version` always names the commit (or says it has none): a binary that cannot say which
    // build it is cannot be told from an older one.
    let short = sha.as_deref().and_then(|s| s.get(..7)).unwrap_or("unknown");
    println!("cargo:rustc-env=AGENT_FLEET_SHORT={short}");
    let Some(sha) = sha else {
        return;
    };
    println!("cargo:rustc-env=AGENT_FLEET_COMMIT={sha}");
    // HEAD moves on checkout; logs/HEAD on every commit, pull and reset.
    for p in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
