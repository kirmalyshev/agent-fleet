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
    let mut commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| {
        println!("cargo:warning=no git commit: the build is `unknown` and self-update replaces it");
        "unknown".into()
    });
    // A status that cannot be read counts as dirty: never claim a clean commit unverified.
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_none_or(|s| !s.is_empty());
    if dirty {
        commit.push_str("-dirty");
    }
    let short: String = commit.chars().take(7).collect();
    let short = if commit.ends_with("-dirty") {
        format!("{short}-dirty")
    } else {
        short
    };
    for p in ["src", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={p}");
    }
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
