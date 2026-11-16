//! The machine's own config dir: `agent-fleet init` writes `fleet.toml` and `brief.md` from the
//! templates in `docs/`, and `agent-fleet repo add|ls|rm` edit the `[repos]` table of that
//! `fleet.toml`: which checkout serves each connected repository on this machine.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The agent programs `agent.start` knows, as profile kinds.
pub const KINDS: [&str; 3] = ["claude", "codex", "devin"];

/// The reference config is the template: what init writes is what the docs explain.
const FLEET: &str = include_str!("../docs/fleet.example.toml");
const BRIEF: &str = include_str!("../docs/brief.example.md");
/// The template's default rule; init points it at the first program found.
const DEFAULT_RULE: &str = "profile = \"claude\"";

/// The kinds whose program is an executable on `PATH`.
pub fn kinds_on_path() -> Vec<&'static str> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    KINDS
        .into_iter()
        .filter(|k| {
            std::env::split_paths(&path).any(|d| {
                std::fs::metadata(d.join(k))
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
        })
        .collect()
}

/// The template with a profile for each kind in `found` and the default rule on the first.
/// None found: every profile stays, the default rule on claude.
pub fn fleet_toml(found: &[&str]) -> String {
    let mut out = String::with_capacity(FLEET.len());
    let mut skip = false;
    for line in FLEET.lines() {
        if let Some(name) = line
            .strip_prefix("[profiles.")
            .and_then(|r| r.strip_suffix(']'))
        {
            skip = !found.is_empty() && !found.contains(&name);
        } else if skip && line.is_empty() {
            // A profile block ends at a blank line; the dropped block takes it along.
            skip = false;
            continue;
        }
        if skip {
            continue;
        }
        match found.first() {
            Some(k) if line == DEFAULT_RULE => out.push_str(&format!("profile = \"{k}\"")),
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Writes `fleet.toml` and `brief.md` into `dir`, each only if it does not exist yet.
/// Returns each file and whether it was written.
pub fn init(dir: &Path, found: &[&str]) -> Result<Vec<(PathBuf, bool)>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut out = Vec::new();
    for (name, body) in [
        ("fleet.toml", fleet_toml(found)),
        ("brief.md", BRIEF.to_string()),
    ] {
        let p = dir.join(name);
        let wrote = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(mut f) => {
                f.write_all(body.as_bytes())
                    .with_context(|| format!("write {}", p.display()))?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e).with_context(|| format!("create {}", p.display())),
        };
        out.push((p, wrote));
    }
    Ok(out)
}

#[derive(Deserialize)]
struct Machine {
    #[serde(default)]
    repos: BTreeMap<String, PathBuf>,
}

fn repos_of(src: &str) -> Result<BTreeMap<String, PathBuf>> {
    Ok(toml::from_str::<Machine>(src)?.repos)
}

/// The `[repos]` of the machine's `fleet.toml` in `dir`: `owner/repo` → checkout. No file, none.
pub fn registered(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let p = dir.join("fleet.toml");
    match std::fs::read_to_string(&p) {
        Ok(src) => repos_of(&src).with_context(|| format!("{}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// The one key of a `key = value` line, if it is one.
fn key_of(line: &str) -> Option<String> {
    let t: toml::Table = toml::from_str(line).ok()?;
    let mut keys = t.into_iter().map(|(k, _)| k);
    keys.next().filter(|_| keys.next().is_none())
}

fn is_header(line: &str, name: Option<&str>) -> bool {
    let t = line.split('#').next().unwrap_or_default().trim();
    match name {
        Some(n) => t == format!("[{n}]"),
        None => t.starts_with('['),
    }
}

/// `src` with `slug` set to `dir` in its `[repos]` table, or removed when `dir` is `None`.
/// Every other line, comments included, is kept as it is; the result is re-read and must say
/// exactly that, or nothing is changed.
pub fn set_repo(src: &str, slug: &str, dir: Option<&Path>) -> Result<String> {
    let mut want = repos_of(src)?;
    let entry = match dir {
        Some(d) => {
            let s = d
                .to_str()
                .with_context(|| format!("{} is not valid UTF-8", d.display()))?;
            want.insert(slug.to_string(), d.to_path_buf());
            Some(format!(
                "{} = {}",
                toml::Value::String(slug.into()),
                toml::Value::String(s.into())
            ))
        }
        None => {
            want.remove(slug);
            None
        }
    };
    let mut lines: Vec<String> = src.lines().map(String::from).collect();
    match lines.iter().position(|l| is_header(l, Some("repos"))) {
        Some(h) => {
            let end = (h + 1..lines.len())
                .find(|&i| is_header(&lines[i], None))
                .unwrap_or(lines.len());
            let keys: Vec<(usize, String)> = (h + 1..end)
                .filter_map(|i| key_of(&lines[i]).map(|k| (i, k)))
                .collect();
            match (keys.iter().find(|(_, k)| k == slug), entry) {
                (Some(&(i, _)), Some(e)) => lines[i] = e,
                (Some(&(i, _)), None) => {
                    lines.remove(i);
                }
                // A new entry goes after the table's last key, before any comment that leads
                // into the next table.
                (None, Some(e)) => lines.insert(keys.last().map_or(h, |(i, _)| *i) + 1, e),
                (None, None) => {}
            }
        }
        None => {
            if let Some(e) = entry {
                lines.extend(["".into(), "[repos]".into(), e]);
            }
        }
    }
    let mut out = lines.join("\n");
    if src.ends_with('\n') || src.is_empty() {
        out.push('\n');
    }
    if repos_of(&out).ok().as_ref() != Some(&want) {
        bail!("cannot edit [repos] in place; edit it by hand");
    }
    Ok(out)
}

/// `owner/repo`: two non-empty parts.
fn check_slug(slug: &str) -> Result<()> {
    match slug.split_once('/') {
        Some((o, r)) if !o.is_empty() && !r.is_empty() && !r.contains('/') => Ok(()),
        _ => bail!("{slug:?} is not owner/repo"),
    }
}

/// The toplevel of the git checkout holding `dir`.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn read_machine(cfg_dir: &Path) -> Result<(PathBuf, String)> {
    let p = cfg_dir.join("fleet.toml");
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok((p, s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no {}: run `agent-fleet init` first", p.display())
        }
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// Write through a temp file and a rename: a reader never sees half a file.
fn replace(p: &Path, body: &str) -> Result<()> {
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, p).with_context(|| format!("replace {}", p.display()))
}

/// Records `dir`'s checkout as `slug`'s. Refuses a directory outside a git checkout, and a
/// checkout whose `origin` is not `slug`: the host would make that repository's worktrees and
/// branches for `slug`'s issues. Returns the checkout and whether the file changed.
pub fn add(cfg_dir: &Path, slug: &str, dir: &Path) -> Result<(PathBuf, bool)> {
    check_slug(slug)?;
    let Some(top) = toplevel(dir) else {
        bail!(
            "{} is not a git checkout: clone {slug} first, then `agent-fleet repo add {slug} <checkout>`",
            dir.display()
        );
    };
    match crate::spawn::slug_of(&top) {
        Some(o) if o.eq_ignore_ascii_case(slug) => {}
        Some(o) => bail!(
            "{}'s origin is {o}, not {slug}: the host would make {o}'s worktrees and branches for {slug}'s issues",
            top.display()
        ),
        None => bail!(
            "{} has no GitHub origin remote; spawn needs one to fetch and push",
            top.display()
        ),
    }
    let (p, src) = read_machine(cfg_dir)?;
    let new = set_repo(&src, slug, Some(&top)).with_context(|| format!("{}", p.display()))?;
    if new == src {
        return Ok((top, false));
    }
    replace(&p, &new)?;
    Ok((top, true))
}

/// Drops `slug` from `[repos]`; the checkout itself is left alone.
pub fn rm(cfg_dir: &Path, slug: &str) -> Result<()> {
    let (p, src) = read_machine(cfg_dir)?;
    if !repos_of(&src)
        .with_context(|| format!("{}", p.display()))?
        .contains_key(slug)
    {
        bail!("{slug} is not connected (agent-fleet repo ls)");
    }
    let new = set_repo(&src, slug, None).with_context(|| format!("{}", p.display()))?;
    replace(&p, &new)
}

/// Words that mark a skill as one that drives terminals and other agents' panes, as herdr does.
const TERMINAL_WORDS: [&str; 3] = ["herdr", "tmux", "cmux"];

/// Each agent program's user-level skills dir: `$CLAUDE_CONFIG_DIR` or `~/.claude`, and
/// `$CODEX_HOME` or `~/.codex`, each with `/skills`.
pub fn user_skill_dirs() -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = |var: &str, dot: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(dot)))
    };
    [
        ("claude", dir("CLAUDE_CONFIG_DIR", ".claude")),
        ("codex", dir("CODEX_HOME", ".codex")),
    ]
    .into_iter()
    .filter_map(|(k, d)| Some((k, d?.join("skills"))))
    .collect()
}

/// A checkout's project-level skills dirs.
pub fn project_skill_dirs(checkout: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("claude", checkout.join(".claude/skills")),
        ("codex", checkout.join(".codex/skills")),
    ]
}

/// The `<dir>/<skill>/SKILL.md` files that name a terminal multiplexer: (program, file, word).
/// A dir that cannot be read has none.
pub fn terminal_skills(
    dirs: &[(&'static str, PathBuf)],
) -> Vec<(&'static str, PathBuf, &'static str)> {
    let mut out = Vec::new();
    for (kind, dir) in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path().join("SKILL.md"))
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            if let Some(w) = TERMINAL_WORDS
                .into_iter()
                .find(|w| crate::spawn::has_word(&text, w))
            {
                out.push((*kind, f, w));
            }
        }
    }
    out
}

/// What `init` and `repo add` print about such skills: nothing when there are none.
pub fn skills_report(found: &[(&'static str, PathBuf, &'static str)]) -> String {
    if found.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nSkills that drive terminals and other agents' panes:\n");
    for (kind, f, w) in found {
        out += &format!("  {kind:<7}{} ({w})\n", f.display());
    }
    out += "In a fleet, agents message each other with `agent-fleet tell` and start agents with \
            `agent-fleet spawn`. Check that these skills do not tell an agent to type into another \
            agent's pane or start agents by hand instead.\n";
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A checkout whose origin is `url` (none when empty).
    fn checkout(url: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        if !url.is_empty() {
            git(d.path(), &["remote", "add", "origin", url]);
        }
        d
    }

    fn top(d: &tempfile::TempDir) -> PathBuf {
        toplevel(d.path()).unwrap()
    }

    #[test]
    fn the_template_has_one_default_rule_and_a_profile_per_kind() {
        assert_eq!(FLEET.lines().filter(|l| *l == DEFAULT_RULE).count(), 1);
        for k in KINDS {
            assert!(
                FLEET.contains(&format!("[profiles.{k}]\nkind = \"{k}\"\n")),
                "{k}"
            );
        }
        // The template as shipped (no program found) is a valid fleet.toml.
        crate::spawn::FleetConfig::parse(&fleet_toml(&[])).unwrap();
    }

    #[test]
    fn init_writes_a_profile_per_found_kind_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("agent-fleet");
        let wrote = init(&dir, &["codex", "devin"]).unwrap();
        assert!(wrote.iter().all(|(_, w)| *w), "{wrote:?}");
        let src = std::fs::read_to_string(dir.join("fleet.toml")).unwrap();
        let cfg = crate::spawn::FleetConfig::parse(&src).unwrap();
        assert_eq!(cfg.profiles.keys().collect::<Vec<_>>(), ["codex", "devin"]);
        assert_eq!(cfg.rules.last().unwrap().profile, "codex");
        assert!(cfg.repos.is_empty());
        assert!(
            src.contains("#   handoff_min "),
            "the comments explain every key"
        );
        let brief = std::fs::read_to_string(dir.join("brief.md")).unwrap();
        assert!(brief.contains("@@TASK@@"));

        std::fs::write(dir.join("fleet.toml"), "mine").unwrap();
        std::fs::remove_file(dir.join("brief.md")).unwrap();
        let again = init(&dir, &["claude"]).unwrap();
        assert_eq!(
            again.iter().map(|(_, w)| *w).collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("fleet.toml")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn repo_add_records_a_checkout_keeps_comments_and_is_idempotent() {
        let cfg = tempfile::tempdir().unwrap();
        init(cfg.path(), &["claude"]).unwrap();
        let co = checkout("git@github.com:o/r.git");
        std::fs::create_dir(co.path().join("sub")).unwrap();
        let before = std::fs::read_to_string(cfg.path().join("fleet.toml")).unwrap();

        // A subdirectory records its checkout's toplevel.
        let (got, changed) = add(cfg.path(), "o/r", &co.path().join("sub")).unwrap();
        assert!(changed);
        assert_eq!(got, top(&co));
        let after = std::fs::read_to_string(cfg.path().join("fleet.toml")).unwrap();
        assert!(after.starts_with(&before), "only a line is added:\n{after}");
        assert_eq!(
            registered(cfg.path()).unwrap(),
            BTreeMap::from([("o/r".into(), top(&co))])
        );
        crate::spawn::FleetConfig::parse(&after).unwrap();

        assert!(
            !add(cfg.path(), "o/r", co.path()).unwrap().1,
            "the same again: no change"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.path().join("fleet.toml")).unwrap(),
            after
        );

        let two = checkout("https://github.com/o/s");
        add(cfg.path(), "o/s", two.path()).unwrap();
        rm(cfg.path(), "o/r").unwrap();
        assert_eq!(
            registered(cfg.path()).unwrap(),
            BTreeMap::from([("o/s".into(), top(&two))])
        );
        assert!(rm(cfg.path(), "o/r").is_err(), "not connected");
        rm(cfg.path(), "o/s").unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.path().join("fleet.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn repo_add_refuses_a_non_checkout_a_wrong_origin_and_a_missing_init() {
        let cfg = tempfile::tempdir().unwrap();
        let co = checkout("https://github.com/o/r.git");
        let e = add(cfg.path(), "o/r", co.path()).unwrap_err().to_string();
        assert!(e.contains("agent-fleet init"), "{e}");
        init(cfg.path(), &[]).unwrap();

        let plain = tempfile::tempdir().unwrap();
        let e = add(cfg.path(), "o/r", plain.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a git checkout"), "{e}");
        let e = add(cfg.path(), "o/other", co.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("origin is o/r"), "{e}");
        let e = add(cfg.path(), "o/r", checkout("").path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no GitHub origin"), "{e}");
        assert!(add(cfg.path(), "nope", co.path()).is_err());
        assert!(registered(cfg.path()).unwrap().is_empty());
    }

    #[test]
    fn set_repo_edits_only_the_repos_table() {
        let src = "a = 1\n[repos] # mine\n\"o/r\" = \"/x\" # keep\n\n# next\n[other]\nb = 2\n";
        let out = set_repo(src, "o/s", Some(Path::new("/y"))).unwrap();
        assert_eq!(
            out,
            "a = 1\n[repos] # mine\n\"o/r\" = \"/x\" # keep\n\"o/s\" = \"/y\"\n\n# next\n[other]\nb = 2\n"
        );
        let out = set_repo(&out, "o/r", Some(Path::new("/z"))).unwrap();
        assert!(
            out.contains("\"o/r\" = \"/z\"") && !out.contains("/x"),
            "{out}"
        );
        assert_eq!(set_repo("a = 1\n", "o/r", None).unwrap(), "a = 1\n");
        assert_eq!(
            set_repo("a = 1\n", "o/r", Some(Path::new("/x"))).unwrap(),
            "a = 1\n\n[repos]\n\"o/r\" = \"/x\"\n"
        );
        // An inline table cannot be edited by lines: refused, never mangled.
        assert!(
            set_repo(
                "repos = { \"o/r\" = \"/x\" }\n",
                "o/s",
                Some(Path::new("/y"))
            )
            .is_err()
        );
    }

    fn host_cfg(config_dir: &Path, flags: &[(&str, &str)]) -> crate::host::Config {
        crate::host::Config {
            host: "h".into(),
            session: "s".into(),
            herdr_socket: "/nonexistent".into(),
            replica: config_dir.join("replica.db"),
            data_dir: config_dir.to_path_buf(),
            repos: Vec::new(),
            checkouts: flags
                .iter()
                .map(|(r, d)| (r.to_string(), PathBuf::from(d)))
                .collect(),
            config_dir: Some(config_dir.to_path_buf()),
            locks: Vec::new(),
            stall_min: 15,
            stuck_act: false,
        }
    }

    #[test]
    fn the_host_picks_up_a_repo_connected_after_it_started_and_its_flags_win() {
        let cfg = tempfile::tempdir().unwrap();
        let host = host_cfg(cfg.path(), &[("o/flag", "/f"), ("o/both", "/from-flag")]);
        // No machine config yet: the flags alone.
        assert_eq!(host.checkouts_now().unwrap().len(), 2);
        init(cfg.path(), &["claude"]).unwrap();
        let p = cfg.path().join("fleet.toml");
        let src = std::fs::read_to_string(&p).unwrap();
        let src = set_repo(&src, "o/late", Some(Path::new("/late"))).unwrap();
        let src = set_repo(&src, "o/both", Some(Path::new("/from-config"))).unwrap();
        std::fs::write(&p, src).unwrap();
        assert_eq!(
            host.checkouts_now().unwrap(),
            BTreeMap::from([
                ("o/both".into(), PathBuf::from("/from-flag")),
                ("o/flag".into(), PathBuf::from("/f")),
                ("o/late".into(), PathBuf::from("/late")),
            ])
        );
    }

    #[test]
    fn a_repos_own_fleet_toml_replaces_the_machines_rules() {
        let machine = tempfile::tempdir().unwrap();
        init(machine.path(), &["codex"]).unwrap();
        let co = tempfile::tempdir().unwrap();
        let (cfg, dir) = crate::spawn::load_in(co.path(), Some(machine.path())).unwrap();
        assert_eq!(dir, machine.path());
        assert!(cfg.profiles.contains_key("codex"));

        let own = co.path().join(".agent-fleet");
        std::fs::create_dir(&own).unwrap();
        std::fs::write(
            own.join("fleet.toml"),
            "brief = \"b.md\"\n[profiles.p]\nkind = \"claude\"\n[[rules]]\nprofile = \"p\"\n",
        )
        .unwrap();
        let (cfg, dir) = crate::spawn::load_in(co.path(), Some(machine.path())).unwrap();
        assert_eq!(dir, own);
        assert_eq!(cfg.profiles.keys().collect::<Vec<_>>(), ["p"]);
        // An error names the file that was read.
        std::fs::write(own.join("fleet.toml"), "nope = 1").unwrap();
        let e = format!(
            "{:#}",
            crate::spawn::load_in(co.path(), Some(machine.path())).unwrap_err()
        );
        assert!(
            e.contains(&own.join("fleet.toml").display().to_string()),
            "{e}"
        );
    }

    #[test]
    fn orphan_prs_default_to_the_connected_repos() {
        let machine = tempfile::tempdir().unwrap();
        init(machine.path(), &["claude"]).unwrap();
        let co = checkout("https://github.com/o/r");
        add(machine.path(), "o/r", co.path()).unwrap();
        let c = crate::orphanpr::config_in(co.path(), Some(machine.path()))
            .unwrap()
            .unwrap();
        assert_eq!(c.repos(), ["o/r"]);

        // An explicit list still wins, and `[]` turns the check off.
        let p = machine.path().join("fleet.toml");
        let src = std::fs::read_to_string(&p).unwrap();
        for (list, want) in [("[\"o/x\"]", vec!["o/x"]), ("[]", vec![])] {
            std::fs::write(&p, format!("{src}\n[orphan_pr]\nrepos = {list}\n")).unwrap();
            let c = crate::orphanpr::config_in(co.path(), Some(machine.path()))
                .unwrap()
                .unwrap();
            assert_eq!(c.repos(), want);
        }
    }

    #[test]
    fn terminal_skills_are_found_per_program_and_reported() {
        let d = tempfile::tempdir().unwrap();
        let skill = |root: &str, name: &str, body: &str| {
            let s = d.path().join(root).join(name);
            std::fs::create_dir_all(&s).unwrap();
            std::fs::write(s.join("SKILL.md"), body).unwrap();
        };
        skill(
            "claude",
            "herdr",
            "---\nname: herdr\n---\nControl Herdr panes.",
        );
        skill(
            "claude",
            "other",
            "Write docs. Not herdrish, no tmuxy words.",
        );
        skill("codex", "mux", "Drives tmux send-keys.");
        std::fs::write(d.path().join("claude/README"), "herdr").unwrap();
        let dirs = vec![
            ("claude", d.path().join("claude")),
            ("codex", d.path().join("codex")),
            ("codex", d.path().join("missing")),
        ];
        let found = terminal_skills(&dirs);
        assert_eq!(
            found,
            [
                ("claude", d.path().join("claude/herdr/SKILL.md"), "herdr"),
                ("codex", d.path().join("codex/mux/SKILL.md"), "tmux"),
            ]
        );
        let r = skills_report(&found);
        assert!(
            r.contains("herdr/SKILL.md (herdr)") && r.contains("agent-fleet tell"),
            "{r}"
        );
        assert_eq!(skills_report(&[]), "");
        assert_eq!(
            project_skill_dirs(Path::new("/co"))[0].1,
            Path::new("/co/.claude/skills")
        );
    }
}
