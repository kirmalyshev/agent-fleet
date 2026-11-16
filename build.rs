//! Embeds the commit the binary is built from, so `--version` and self-update can name it.
use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let mut commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    if git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty()) {
        commit.push_str("-dirty");
    }
    let short: String = commit.chars().take(7).collect();
    let short = if commit.ends_with("-dirty") {
        format!("{short}-dirty")
    } else {
        short
    };
    println!("cargo:rustc-env=AGENT_FLEET_COMMIT={commit}");
    println!("cargo:rustc-env=AGENT_FLEET_SHORT={short}");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(r) =
        git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r]))
    {
        println!("cargo:rerun-if-changed={r}");
    }
}
