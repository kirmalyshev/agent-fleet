//! What the host agent samples on its machine: the cost of each agent's process tree, and the
//! git worktrees its agents work in.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::event::{MAX_FILES, WorktreeState};

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub ppid: u32,
    /// CPU time so far, reaped children included.
    pub cpu_ms: u64,
    pub rss_kb: u64,
    pub name: String,
    /// `None` when unreadable; a deleted directory keeps its old path and `deleted`.
    pub cwd: Option<String>,
    pub deleted: bool,
    /// The argv as one line, when the platform reports it.
    pub args: Option<String>,
    /// When the process started, when the platform reports it.
    pub started: Option<DateTime<Utc>>,
    /// The process-group id — the group leader's pid — when the platform reports it.
    pub pgid: Option<u32>,
}

/// Every process on this machine at one moment. Empty outside Linux and macOS.
#[derive(Debug, Default)]
pub struct Procs(pub HashMap<u32, Proc>);

impl Procs {
    #[cfg(target_os = "linux")]
    pub fn read() -> Self {
        // SAFETY: sysconf only reads configuration.
        let (tick, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64,
                libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64,
            )
        };
        let boot = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| parse_btime(&s));
        let mut all = HashMap::new();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Self(all);
        };
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            let Some((ppid, ticks, pages, boot_ticks, pgid)) =
                std::fs::read_to_string(entry.path().join("stat"))
                    .ok()
                    .and_then(|s| parse_stat(&s))
            else {
                continue;
            };
            let cwd = std::fs::read_link(entry.path().join("cwd"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let deleted = cwd.as_deref().is_some_and(|c| c.ends_with(" (deleted)"));
            let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            let args = std::fs::read(entry.path().join("cmdline"))
                .ok()
                .map(|b| {
                    b.split(|&c| c == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty());
            let started =
                boot.and_then(|b| DateTime::from_timestamp(b + (boot_ticks / tick) as i64, 0));
            all.insert(
                pid,
                Proc {
                    ppid,
                    cpu_ms: ticks * 1000 / tick,
                    rss_kb: pages * page / 1024,
                    name: name.trim().to_string(),
                    cwd: cwd.map(|c| c.trim_end_matches(" (deleted)").to_string()),
                    deleted,
                    args,
                    started,
                    pgid: Some(pgid),
                },
            );
        }
        Self(all)
    }

    /// One `ps` call for the whole table; `comm` gives no cwd, so `cwd`/`deleted` stay empty.
    #[cfg(target_os = "macos")]
    pub fn read() -> Self {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,pgid=,rss=,time=,lstart=,args="])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        Self(out.map(|s| parse_ps(&s)).unwrap_or_default())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn read() -> Self {
        Self::default()
    }

    /// `root` and everything under it; empty if `root` is gone.
    pub fn tree(&self, root: u32) -> Vec<u32> {
        if !self.0.contains_key(&root) {
            return Vec::new();
        }
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, p) in &self.0 {
            children.entry(p.ppid).or_default().push(pid);
        }
        let (mut out, mut todo) = (Vec::new(), vec![root]);
        while let Some(pid) = todo.pop() {
            out.push(pid);
            todo.extend(children.get(&pid).into_iter().flatten());
        }
        out
    }

    /// `pid` or an ancestor of it is in `pids` — e.g. a program under its own `app-server`.
    pub fn under_any(&self, pid: u32, pids: &HashSet<u32>) -> bool {
        let mut at = Some(pid);
        for _ in 0..64 {
            let Some(cur) = at else { return false };
            if pids.contains(&cur) {
                return true;
            }
            at = self.0.get(&cur).map(|p| p.ppid);
        }
        false
    }

    /// `pid` or an ancestor of it has `name` — e.g. a process inside a herdr pane.
    pub fn under(&self, pid: u32, name: &str) -> bool {
        let mut at = Some(pid);
        for _ in 0..64 {
            let Some(p) = at.and_then(|p| self.0.get(&p)) else {
                return false;
            };
            if p.name == name {
                return true;
            }
            at = Some(p.ppid);
        }
        false
    }

    /// Resident memory (KiB) and CPU so far (ms) of `pids`.
    pub fn sum(&self, pids: &[u32]) -> (u64, u64) {
        pids.iter()
            .filter_map(|p| self.0.get(p))
            .fold((0, 0), |(r, c), p| (r + p.rss_kb, c + p.cpu_ms))
    }

    /// CPU percent of one core that `pids` used between `earlier` and now, `ms` apart. A
    /// process missing from `earlier` started since, so all of its time counts.
    pub fn cpu_pct(&self, earlier: &Procs, pids: &[u32], ms: u64) -> u32 {
        let used: u64 = pids
            .iter()
            .filter_map(|p| {
                Some(
                    self.0
                        .get(p)?
                        .cpu_ms
                        .saturating_sub(earlier.0.get(p).map_or(0, |e| e.cpu_ms)),
                )
            })
            .sum();
        (used * 100 / ms.max(1)) as u32
    }
}

/// Processes running in a worktree, or in a deleted directory beside worktrees (orphans).
#[derive(Debug, Default, PartialEq)]
pub struct Group {
    pub pids: Vec<u32>,
    pub orphaned: bool,
}

/// The terminal multiplexer and the shells of its panes are never a worktree's processes:
/// they are the operator's own, whatever directory they sit in.
pub(crate) fn is_terminal(procs: &Procs, p: &Proc) -> bool {
    p.name == "herdr"
        || procs
            .0
            .get(&p.ppid)
            .is_some_and(|parent| parent.name == "herdr")
}

/// Groups processes not in `exclude` by the worktree their cwd is in (the deepest one).
///
/// An orphan is a process whose cwd was deleted together with its worktree: the cwd is in no
/// worktree that exists, it lies in a directory that holds linked worktrees, and the worktree
/// directory it names is gone from disk (`exists`). Orphan groups never mix with a live
/// worktree's processes. A process in a deleted subdirectory of a live worktree belongs to that
/// worktree; it is not an orphan.
pub fn by_worktree(
    procs: &Procs,
    exclude: &HashSet<u32>,
    worktrees: &[(String, bool)],
    exists: impl Fn(&str) -> bool,
) -> BTreeMap<String, Group> {
    let inside = |cwd: &str, dir: &str| cwd == dir || cwd.starts_with(&format!("{dir}/"));
    let homes: BTreeSet<&str> = worktrees
        .iter()
        .filter(|(_, linked)| *linked)
        .filter_map(|(p, _)| Path::new(p).parent()?.to_str())
        .collect();
    let mut out: BTreeMap<String, Group> = BTreeMap::new();
    for (&pid, p) in &procs.0 {
        let Some(cwd) = p.cwd.as_deref() else {
            continue;
        };
        if exclude.contains(&pid) || is_terminal(procs, p) {
            continue;
        }
        let live = worktrees
            .iter()
            .filter(|(w, _)| inside(cwd, w))
            .max_by_key(|(w, _)| w.len());
        if let Some((wt, _)) = live {
            out.entry(wt.clone()).or_default().pids.push(pid);
            continue;
        }
        if !p.deleted {
            continue;
        }
        let Some(home) = homes
            .iter()
            .filter(|h| inside(cwd, h) && cwd != **h)
            .max_by_key(|h| h.len())
        else {
            continue;
        };
        // The worktree's own directory, one level under its home.
        let rest = &cwd[home.len() + 1..];
        let root = format!("{home}/{}", rest.split('/').next().unwrap_or(rest));
        if exists(&root) || worktrees.iter().any(|(w, _)| inside(w, &root)) {
            continue;
        }
        let g = out.entry(root).or_default();
        g.pids.push(pid);
        g.orphaned = true;
    }
    for g in out.values_mut() {
        g.pids.sort_unstable();
    }
    out
}

/// `ps -axo pid=,ppid=,pgid=,rss=,time=,lstart=,args=` → pid → Proc (macOS; `ps` reports no
/// cwd). `lstart` is five words (`Sat Nov 15 21:31:45 2025`), `args` the rest, and the
/// name is argv[0]'s basename — which also covers a path with a space in it.
#[cfg(any(target_os = "macos", test))]
pub fn parse_ps(s: &str) -> HashMap<u32, Proc> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (pid, ppid, pgid, rss_kb) = (
                f.first()?.parse().ok()?,
                f.get(1)?.parse().ok()?,
                f.get(2)?.parse().ok()?,
                f.get(3)?.parse().ok()?,
            );
            let cpu_ms = parse_ps_time(f.get(4)?)?;
            let started = parse_lstart(&f.get(5..10)?.join(" "));
            let argv0 = f.get(10)?;
            // macOS gives a path, and a login shell is `-zsh`.
            let name = argv0
                .rsplit('/')
                .next()?
                .trim_start_matches('-')
                .to_string();
            let args = f.get(10..).map(|a| a.join(" ")).filter(|a| !a.is_empty());
            Some((
                pid,
                Proc {
                    ppid,
                    cpu_ms,
                    rss_kb,
                    name,
                    cwd: None,
                    deleted: false,
                    args,
                    started,
                    pgid: Some(pgid),
                },
            ))
        })
        .collect()
}

/// `ps` lstart, `Sat Nov 15 21:31:45 2025` in the local zone, → Utc.
#[cfg(any(target_os = "macos", test))]
fn parse_lstart(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%a %b %d %H:%M:%S %Y")
        .ok()?
        .and_local_timezone(chrono::Local)
        .single()
        .map(|t| t.to_utc())
}

/// `ps` TIME, `[[dd-]hh:]mm:ss[.ss]` → milliseconds.
#[cfg(any(target_os = "macos", test))]
fn parse_ps_time(s: &str) -> Option<u64> {
    let (days, s) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let mut f = s.rsplitn(3, ':');
    let sec = f.next()?;
    let min: u64 = f.next()?.parse().ok()?;
    let hour: u64 = f.next().map_or(Ok(0), |h| h.parse()).ok()?;
    let (sec, frac) = sec.split_once('.').unwrap_or((sec, "0"));
    let ms = frac.parse::<u64>().ok()? * 1000 / 10u64.pow(frac.len().min(19) as u32);
    Some(((days * 24 + hour) * 3600 + min * 60 + sec.parse::<u64>().ok()?) * 1000 + ms)
}

/// `/proc/<pid>/stat` → (ppid, utime + stime + cutime + cstime ticks, rss pages,
/// starttime ticks since boot, pgrp).
pub fn parse_stat(s: &str) -> Option<(u32, u64, u64, u64, u32)> {
    // The command name may hold spaces and parens; the fields after the last `)` do not.
    let f: Vec<&str> = s[s.rfind(')')? + 1..].split_whitespace().collect();
    let n = |i: usize| f.get(i)?.parse::<u64>().ok();
    Some((
        n(1)? as u32,
        n(11)? + n(12)? + n(13)? + n(14)?,
        n(21)?,
        n(19)?,
        n(2)? as u32,
    ))
}

/// `/proc/stat`'s `btime` line: the epoch second the machine booted.
#[cfg(target_os = "linux")]
pub fn parse_btime(s: &str) -> Option<i64> {
    s.lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
}

async fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Never take the index lock an agent's own git may need.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The main checkout of the repository `dir` is in.
pub async fn repo_of(dir: &str) -> Option<String> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let common = Path::new(common.trim());
    match common.file_name()?.to_str()? {
        ".git" => Some(common.parent()?.to_str()?.to_string()),
        _ => None, // a bare repository has no main checkout to look at
    }
}

/// `git worktree list --porcelain` → (path, head, branch) of each worktree still on disk.
pub fn parse_worktrees(s: &str) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    for block in s.split("\n\n") {
        let (mut path, mut head, mut branch, mut skip) = (None, None, None, false);
        for line in block.lines() {
            match line.split_once(' ').unwrap_or((line, "")) {
                ("worktree", p) => path = Some(p.to_string()),
                ("HEAD", h) => head = Some(h.to_string()),
                ("branch", b) => {
                    branch = Some(b.trim_start_matches("refs/heads/").to_string());
                }
                ("bare", _) | ("prunable", _) => skip = true,
                _ => {}
            }
        }
        if let (Some(path), Some(head), false) = (path, head, skip) {
            out.push((path, head, branch));
        }
    }
    out
}

/// File names in `git status --porcelain` output (the new name of a rename).
pub fn parse_status(s: &str) -> Vec<String> {
    s.lines()
        .filter_map(|l| l.get(3..))
        .map(|f| {
            f.rsplit(" -> ")
                .next()
                .unwrap_or(f)
                .trim_matches('"')
                .to_string()
        })
        .collect()
}

/// herdr's worktree directory: `[worktrees] directory` in its config, else `~/.herdr/worktrees`.
pub fn herdr_worktrees() -> Option<PathBuf> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(|| home.join(".config"), PathBuf::from)
        .join("herdr/config.toml");
    let config = std::fs::read_to_string(config).unwrap_or_default();
    Some(herdr_dir(&config, &home))
}

fn herdr_dir(config: &str, home: &Path) -> PathBuf {
    let dir = config
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| Some(t.get("worktrees")?.get("directory")?.as_str()?.to_string()))
        .unwrap_or_else(|| "~/.herdr/worktrees".into());
    match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(dir),
    }
}

/// Every worktree of `repo`, as `host` sees it now; `herdr` is herdr's worktree directory.
pub async fn worktrees(host: &str, repo: &str, herdr: Option<&Path>) -> Vec<WorktreeState> {
    let Some(list) = git(repo, &["worktree", "list", "--porcelain"]).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (path, head, branch) in parse_worktrees(&list) {
        let Some(last) = git(&path, &["log", "-1", "--format=%cI"]).await else {
            continue;
        };
        let Ok(last_commit) = DateTime::parse_from_rfc3339(last.trim()) else {
            continue;
        };
        let dirty = parse_status(
            &git(&path, &["status", "--porcelain"])
                .await
                .unwrap_or_default(),
        );
        let unpushed = git(
            &path,
            &["rev-list", "--count", "HEAD", "--not", "--remotes"],
        )
        .await
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
        let base = git(&path, &["rev-parse", "--abbrev-ref", "origin/HEAD"])
            .await
            .map_or_else(|| "origin/main".to_string(), |b| b.trim().to_string());
        let changed = git(&path, &["diff", "--name-only", &format!("{base}...HEAD")])
            .await
            .unwrap_or_default();
        // A submodule pointer moves on most branches; it is shared plumbing, not overlapping work.
        let gitmodules = ["config", "--file", ".gitmodules", "--get-regexp", "path"];
        let submodules: Vec<String> = git(&path, &gitmodules)
            .await
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
            .collect();
        // Of what the branch changed, only what still differs from the default branch is its own
        // work: a merged or squashed branch differs in nothing it changed.
        let differs: BTreeSet<String> = git(&path, &["diff", "--name-only", &base, "HEAD"])
            .await
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let own: BTreeSet<String> = changed
            .lines()
            .filter(|f| differs.contains(*f) && !submodules.iter().any(|s| s == f))
            .map(str::to_string)
            .collect();
        let in_main = own.is_empty();
        let files: BTreeSet<String> = own
            .into_iter()
            .chain(dirty.iter().filter(|&f| !submodules.contains(f)).cloned())
            .collect();
        out.push(WorktreeState {
            host: host.to_string(),
            herdr: path != repo && herdr.is_some_and(|d| Path::new(&path).starts_with(d)),
            path,
            repo: repo.to_string(),
            branch,
            head,
            unpushed,
            dirty: dirty.len() as u32,
            dirty_submodules: dirty.iter().filter(|&f| submodules.contains(f)).count() as u32,
            files: files.into_iter().take(MAX_FILES).collect(),
            last_commit: last_commit.with_timezone(&Utc),
            in_main,
        });
    }
    out
}

/// A lock as the host reads it: who holds it and who queues for it, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lock {
    pub holder: Option<Hold>,
    pub waiting: Vec<Hold>,
}

/// One holder or waiter. `cwd` is what the lock says; the host asks the process when it does not.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub pid: u32,
    pub cwd: Option<String>,
    pub scope: Option<String>,
    pub since: DateTime<Utc>,
}

pub fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && (unsafe { libc::kill(pid as i32, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

fn mtime(p: &Path) -> Option<DateTime<Utc>> {
    Some(std::fs::metadata(p).ok()?.modified().ok()?.into())
}

fn line(p: &Path, n: usize) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let l = s.lines().nth(n)?.trim();
    (!l.is_empty()).then(|| l.to_string())
}

/// A lock directory, held while it exists. The holder's pid is in its `owner` file, else in
/// `pid`; a process still alive under either keeps it held. `cwd` and `scope` files describe the
/// holder. Waiters are files in `<dir>.waiting/`, named `<unix seconds>.<pid>`, with the pid,
/// scope and cwd on their first three lines. A waiter whose process is gone does not count.
pub fn read_lock(dir: &Path) -> Lock {
    let file = |name: &str| dir.join(name);
    let pid_in = |name: &str| line(&file(name), 0).and_then(|l| l.parse::<u32>().ok());
    let holder = match (pid_in("owner"), pid_in("pid")) {
        (owner, pid) if owner.is_some_and(alive) || pid.is_some_and(alive) => {
            let (pid, from) = match owner {
                Some(o) => (o, "owner"),
                None => (pid.unwrap_or(0), "pid"),
            };
            Some(Hold {
                pid,
                cwd: line(&file("cwd"), 0),
                scope: line(&file("scope"), 0),
                since: mtime(&file(from)).unwrap_or_else(Utc::now),
            })
        }
        _ => None,
    };
    let mut queue = dir.as_os_str().to_owned();
    queue.push(".waiting");
    let mut waiting: Vec<Hold> = std::fs::read_dir(Path::new(&queue))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let pid: u32 = line(&p, 0)?.parse().ok()?;
            let since = e
                .file_name()
                .to_str()
                .and_then(|n| n.split('.').next()?.parse::<i64>().ok())
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .or_else(|| mtime(&p))?;
            alive(pid).then(|| Hold {
                pid,
                scope: line(&p, 1),
                cwd: line(&p, 2),
                since,
            })
        })
        .collect();
    waiting.sort_by_key(|w| (w.since, w.pid));
    Lock { holder, waiting }
}

/// A reparented device/test tool whose device is gone must outlive its session by this
/// much before that is proof it can never work again.
pub const STRAY_MIN: chrono::Duration = chrono::Duration::hours(2);

/// A reparented device/test tool whose process-group leader is dead needs only a short
/// floor: long enough that a script mid-`exec` — its leader gone for a breath — is not
/// caught in the gap.
pub const STRAY_LEADER_MIN: chrono::Duration = chrono::Duration::minutes(2);

/// `true` when the process is a device or test tool a dead session can leave behind:
/// `xcodebuild`, `maestro`, `simctl`, `idb`, `xctest`, the WDA runner, `log stream`, or a
/// `java` running maestro — the CLI's real name, since it runs as `java -classpath
/// …/.maestro/lib/… maestro.cli.AppKt`.
pub(crate) fn device_tool(p: &Proc) -> bool {
    matches!(
        p.name.as_str(),
        "xcodebuild" | "maestro" | "simctl" | "idb" | "xctest"
    ) || p.name.starts_with("WebDriverAgentRunner")
        || (p.name == "log" && p.args.as_deref().is_some_and(|a| a.contains("log stream")))
        || (p.name == "java"
            && p.args
                .as_deref()
                .is_some_and(|a| a.contains("maestro.cli.AppKt") || a.contains("/.maestro/lib/")))
}

/// A cheap prefilter: reparented to init and tool-shaped — worth collecting device lists for.
/// The real proof is `stray`.
pub fn may_be_stray(p: &Proc) -> bool {
    p.ppid == 1 && device_tool(p)
}

/// `abcdef01-2345-6789-abcd-ef0123456789` — the shape a UDID argument takes.
fn udid_shaped(w: &str) -> bool {
    w.len() == 36
        && w.chars().filter(|&c| c == '-').count() == 4
        && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Every UDID-shaped word in `devicectl`/`simctl` output.
pub fn udids(s: &str) -> HashSet<String> {
    s.split_whitespace()
        .filter(|w| udid_shaped(w))
        .map(str::to_string)
        .collect()
}

/// The UDID the tool's argv targets: `id=` inside `-destination`, `--udid`/`-d`'s word, then
/// the first UDID-shaped bare argument (a `simctl` verb takes its device positionally).
fn device_of(args: &str) -> Option<String> {
    let w: Vec<&str> = args.split_whitespace().collect();
    for (i, t) in w.iter().enumerate() {
        if t.starts_with("-destination")
            && let Some(id) = [*t, w.get(i + 1).copied().unwrap_or("")]
                .iter()
                .flat_map(|s| s.split(','))
                .find_map(|p| p.strip_prefix("id="))
        {
            return Some(id.to_string());
        }
        if (*t == "--udid" || *t == "-d")
            && let Some(u) = w.get(i + 1)
        {
            return Some(u.to_string());
        }
        if let Some(u) = t.strip_prefix("--udid=") {
            return Some(u.to_string());
        }
    }
    w.iter().find(|t| udid_shaped(t)).map(|s| s.to_string())
}

/// A process reparented to init that can do no more work, proved two ways. Its
/// process-group leader's pid is the group id itself: a group id no live process owns is
/// a dead session, so a device/test tool in it past `STRAY_LEADER_MIN` is an orphan
/// whatever its device — the floor only spares a script mid-`exec`. A tool whose leader
/// is alive needs the older proof: `STRAY_MIN` old and aiming at a UDID in `live` — the
/// devices still connected or booted — nowhere. A tool that names no device, one we
/// cannot date, and (for the second proof) a `live` the toolchain could not produce are
/// never proved.
pub fn stray(
    procs: &Procs,
    pid: u32,
    live: Option<&HashSet<String>>,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    let p = procs.0.get(&pid)?;
    if p.ppid != 1 || !device_tool(p) {
        return None;
    }
    let age = now - p.started?;
    if age >= STRAY_LEADER_MIN
        && p.pgid
            .is_some_and(|g| g != pid && !procs.0.contains_key(&g))
    {
        return Some("group leader dead");
    }
    let dev = p.args.as_deref().and_then(device_of)?;
    (age >= STRAY_MIN && live.is_some_and(|l| !l.contains(&dev))).then_some("device gone")
}

/// Every process still parented into a proved orphan joins the proof, inheriting its
/// ancestor's `why`.
pub fn descendants(procs: &Procs, proved: &mut HashMap<u32, String>) {
    loop {
        let new: Vec<(u32, String)> = procs
            .0
            .iter()
            .filter(|(k, p)| !proved.contains_key(*k) && proved.contains_key(&p.ppid))
            .map(|(&k, p)| (k, proved[&p.ppid].clone()))
            .collect();
        if new.is_empty() {
            break;
        }
        proved.extend(new);
    }
}

/// The pids orphaned under a held lock: the recorded `pid` holder alive under init while
/// `owner` names a dead process — nobody's pane could have started it — plus every member
/// of the lock's recorded `pgid` group whose ancestry reaches init or a proved pid, and
/// everything still parented into a proved pid. The tool list does not apply, and neither
/// does an age or device check: an owner that exits normally releases the lock first, so
/// a member of its group under a still-held lock is an orphan by definition. No `pgid`
/// file, or a member still parented by a live process outside the group, proves nothing
/// beyond the holder.
pub fn lock_orphan(dir: &Path, procs: &Procs) -> Vec<u32> {
    let pid_in = |name: &str| line(&dir.join(name), 0).and_then(|l| l.parse::<u32>().ok());
    let mut out = Vec::new();
    let Some(owner) = pid_in("owner") else {
        return out;
    };
    if alive(owner) {
        return out;
    }
    let Some(pid) = pid_in("pid") else {
        return out;
    };
    if procs.0.get(&pid).is_none_or(|p| p.ppid != 1) {
        return out;
    }
    let mut proved: HashSet<u32> = [pid].into_iter().collect();
    let group = pid_in("pgid");
    loop {
        let new: Vec<u32> = procs
            .0
            .iter()
            .filter(|(k, p)| {
                !proved.contains(*k)
                    && (proved.contains(&p.ppid)
                        || (p.ppid == 1 && group.is_some_and(|g| p.pgid == Some(g))))
            })
            .map(|(&k, _)| k)
            .collect();
        if new.is_empty() {
            break;
        }
        proved.extend(new);
    }
    out.extend(proved);
    out.sort_unstable();
    out
}

/// How a lock changed since it was last read: who let go, who took it, who joined the queue.
/// `before: None` is the first read, which owes a full release ahead of what it found.
#[derive(Debug, PartialEq)]
pub enum LockChange<'a> {
    ReleasedAll,
    Released(u32),
    Taken(&'a Hold),
    Waiting(&'a Hold),
}

pub fn lock_changes<'a>(before: Option<&Lock>, now: &'a Lock) -> Vec<LockChange<'a>> {
    let mut out = Vec::new();
    let empty = Lock::default();
    let before = before.unwrap_or_else(|| {
        out.push(LockChange::ReleasedAll);
        &empty
    });
    let holder = |l: &Lock| l.holder.as_ref().map(|h| h.pid);
    let had: BTreeSet<u32> = holder(before)
        .into_iter()
        .chain(before.waiting.iter().map(|w| w.pid))
        .collect();
    let has: BTreeSet<u32> = holder(now)
        .into_iter()
        .chain(now.waiting.iter().map(|w| w.pid))
        .collect();
    out.extend(
        had.iter()
            .filter(|p| !has.contains(p))
            .map(|&p| LockChange::Released(p)),
    );
    if let Some(h) = &now.holder
        && holder(before) != Some(h.pid)
    {
        out.push(LockChange::Taken(h));
    }
    let queued: Vec<u32> = before.waiting.iter().map(|w| w.pid).collect();
    out.extend(
        now.waiting
            .iter()
            .filter(|w| !queued.contains(&w.pid))
            .map(LockChange::Waiting),
    );
    out
}

/// `codex app-server daemon`, `claude daemon` and friends: an agent program's own
/// infrastructure, not an agent session — `true` when `pid`'s argv carries an
/// `app-server` or `daemon` in the subcommand slot. Only the first argument counts: a
/// `daemon` inside a `-p` prompt (which `ps -o args=` shows unquoted) is no daemon.
pub fn daemonish(pid: u32) -> bool {
    is_infra(&argv_of(pid))
}

fn is_infra(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|t| t == "app-server" || t == "daemon")
}

fn argv_of(pid: u32) -> Vec<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                b.split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .unwrap_or_default();
    }
    std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `lsof` by absolute path: a supervisor's PATH (launchd's default) lacks `/usr/sbin`, where
/// macOS keeps it.
pub fn lsof_path() -> &'static str {
    pick_lsof(|p| Path::new(p).exists())
}

fn pick_lsof(exists: impl Fn(&str) -> bool) -> &'static str {
    ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| exists(p))
        .unwrap_or("lsof")
}

/// The working directory of a live process on this machine.
pub fn cwd_of(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
    }
    let out = std::process::Command::new(lsof_path())
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs(list: &[(u32, u32, u32, &str)]) -> Procs {
        Procs(
            list.iter()
                .map(|&(pid, ppid, pgid, name)| {
                    (
                        pid,
                        Proc {
                            ppid,
                            cpu_ms: 0,
                            rss_kb: 0,
                            name: name.into(),
                            cwd: None,
                            deleted: false,
                            args: None,
                            started: None,
                            pgid: Some(pgid),
                        },
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn under_walks_the_parent_chain() {
        // herdr(1) → shell(10) → claude(11); stray claude(20); host(30) → devin(31)
        let t = procs(&[
            (1, 0, 1, "herdr"),
            (10, 1, 1, "zsh"),
            (11, 10, 10, "claude"),
            (20, 0, 20, "claude"),
            (30, 0, 30, "agent-fleet"),
            (31, 30, 30, "devin"),
        ]);
        assert!(t.under(11, "herdr"));
        assert!(!t.under(20, "herdr"));
        assert!(!t.under(11, "agent-fleet"));
        assert!(t.under(31, "agent-fleet"));
        assert!(!t.under(999, "herdr"));
    }

    #[test]
    fn a_lock_reads_its_holder_and_live_waiters_and_diffs_into_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sim.lock");
        assert_eq!(read_lock(&dir), Lock::default());
        let me = std::process::id();
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("pid"), "999999999\n").unwrap();
        assert_eq!(read_lock(&dir).holder, None, "a dead holder holds nothing");
        std::fs::write(dir.join("owner"), format!("{me}\n")).unwrap();
        std::fs::write(dir.join("scope"), "e2e, 3 flows\n").unwrap();
        let q = tmp.path().join("sim.lock.waiting");
        std::fs::create_dir(&q).unwrap();
        std::fs::write(q.join("1700000000.1"), format!("{me}\nperf.sh\n/w/b\n")).unwrap();
        std::fs::write(q.join("1600000000.2"), "999999999\nghost\n").unwrap();
        let held = read_lock(&dir);
        let h = held.holder.as_ref().unwrap();
        assert_eq!(
            (h.pid, h.scope.as_deref(), h.cwd.as_deref()),
            (me, Some("e2e, 3 flows"), None)
        );
        assert_eq!(held.waiting.len(), 1, "the dead waiter is dropped");
        let w = &held.waiting[0];
        assert_eq!(
            (w.pid, w.scope.as_deref(), w.cwd.as_deref()),
            (me, Some("perf.sh"), Some("/w/b"))
        );
        assert_eq!(w.since.timestamp(), 1_700_000_000);

        assert_eq!(
            lock_changes(None, &held),
            vec![
                LockChange::ReleasedAll,
                LockChange::Taken(h),
                LockChange::Waiting(w)
            ]
        );
        assert!(lock_changes(Some(&held), &held).is_empty());
        let next = Lock {
            holder: Some(Hold {
                pid: 7,
                ..w.clone()
            }),
            waiting: vec![],
        };
        assert_eq!(
            lock_changes(Some(&held), &next),
            vec![
                LockChange::Released(me),
                LockChange::Taken(next.holder.as_ref().unwrap())
            ]
        );
        assert_eq!(
            lock_changes(Some(&next), &Lock::default()),
            vec![LockChange::Released(7)]
        );
    }

    #[test]
    fn a_tool_whose_group_leader_is_dead_is_a_stray() {
        // The owner (pgid 45872) died nine minutes ago; its java driver sits under init in
        // the dead group on a still-booted sim — too young and too attached for the
        // device-gone proof, orphaned all the same.
        let now = Utc::now();
        let tool = |_pid, ppid, pgid, name: &str, args: &str, m: i64| Proc {
            ppid,
            cpu_ms: 0,
            rss_kb: 0,
            name: name.into(),
            cwd: None,
            deleted: false,
            args: Some(args.into()),
            started: Some(now - chrono::Duration::minutes(m)),
            pgid: Some(pgid),
        };
        let mut t = procs(&[
            (65813, 1, 45872, "java"),
            (555, 1, 555, "zsh"),
            (666, 1, 555, "maestro"),
        ]);
        {
            let j = t.0.get_mut(&65813).unwrap();
            j.args = Some(
                "java -classpath /u/.maestro/lib/maestro-cli.jar maestro.cli.AppKt test flow.yaml"
                    .into(),
            );
            j.started = Some(now - chrono::Duration::minutes(9));
        }
        t.0.get_mut(&666).unwrap().started = Some(now - chrono::Duration::minutes(30));
        let live: HashSet<String> = HashSet::new();
        // Maestro runs as `java`: its args name it, and the dead leader proves it.
        assert_eq!(
            stray(&t, 65813, Some(&live), now),
            Some("group leader dead")
        );
        // A plain java in the same dead group is nobody's device tool.
        t.0.insert(
            65814,
            tool(65814, 1, 45872, "java", "java -jar something.jar", 9),
        );
        assert_eq!(
            stray(&t, 65814, Some(&live), now),
            None,
            "java is a tool only when maestro's args say so"
        );
        let s = |pid, min, table: &Procs| {
            let p = tool(pid, 1, 45872, "xcodebuild", "xcodebuild test", min);
            let mut t = Procs(table.0.clone());
            t.0.insert(pid, p);
            stray(&t, pid, Some(&live), now)
        };
        assert_eq!(s(70001, 9, &t), Some("group leader dead"));
        // Each condition on its own is no proof.
        assert_eq!(s(70001, 1, &t), None, "younger than the floor");
        assert_eq!(
            stray(&t, 666, Some(&live), now),
            None,
            "leader 666 alive (somebody's own terminal run)"
        );
        assert_eq!(
            stray(&t, 555, Some(&live), now),
            None,
            "zsh is no device tool"
        );
        // A child of a proved member is `descendants`' work, not `stray`'s.
        let mut t2 = Procs(t.0.clone());
        t2.0.insert(
            67397,
            tool(67397, 65813, 45872, "xcodebuild", "xcodebuild test", 9),
        );
        assert_eq!(stray(&t2, 67397, Some(&live), now), None, "ppid ≠ 1");
        // The leader may name itself: a tool that leads its own group has a live leader.
        let t3 = procs(&[(1, 1, 1, "xcodebuild")]);
        let p = tool(7, 1, 7, "xcodebuild", "xcodebuild test", 9);
        let mut t3 = Procs(t3.0.clone());
        t3.0.insert(7, p);
        assert_eq!(stray(&t3, 7, Some(&live), now), None, "it is the leader");
        // No device list needed for this proof.
        let t4 = procs(&[(1, 1, 1, "xcodebuild")]);
        let p = tool(7, 1, 45872, "xcodebuild", "xcodebuild test", 9);
        let mut t4 = Procs(t4.0.clone());
        t4.0.insert(7, p);
        assert_eq!(stray(&t4, 7, None, now), Some("group leader dead"));
    }

    #[test]
    fn a_reparented_tool_whose_device_is_gone_is_a_stray() {
        let now = Utc::now();
        let dev = "ABCDEF01-2345-6789-ABCD-EF0123456789";
        // Each tool leads its own live group, so only the device-gone proof can fire.
        let proc = |pid, ppid, name: &str, args: &str, h: i64| {
            let mut t = procs(&[(pid, 1, pid, name)]);
            let p = t.0.get_mut(&pid).unwrap();
            p.ppid = ppid;
            p.args = Some(args.into());
            p.started = Some(now - chrono::Duration::hours(h));
            t
        };
        let live: HashSet<String> = HashSet::new();
        let x = format!(
            "xcodebuild -destination id={dev} test-without-building -xctestrun /w/.build/wda/x.xctestrun"
        );
        // All conditions met: ppid 1, a known tool, leader alive, older than STRAY_MIN,
        // device gone.
        let t = proc(9, 1, "xcodebuild", &x, 3);
        assert_eq!(stray(&t, 9, Some(&live), now), Some("device gone"));
        // Each one missing is no proof.
        let t = proc(9, 42, "xcodebuild", &x, 3);
        assert_eq!(stray(&t, 9, Some(&live), now), None, "ppid ≠ 1");
        let t = proc(9, 1, "zsh", "zsh", 3);
        assert_eq!(stray(&t, 9, Some(&live), now), None, "not a tool");
        let t = proc(9, 1, "xcodebuild", &x, 1);
        assert_eq!(stray(&t, 9, Some(&live), now), None, "too young");
        let live: HashSet<String> = [dev.to_string()].into_iter().collect();
        let t = proc(9, 1, "xcodebuild", &x, 3);
        assert_eq!(stray(&t, 9, Some(&live), now), None, "device there");
        // The toolchain that cannot answer proves nothing this way.
        let live: HashSet<String> = HashSet::new();
        let t = proc(9, 1, "xcodebuild", &x, 3);
        assert_eq!(stray(&t, 9, None, now), None, "no list, no proof");
        let t = proc(9, 1, "xcodebuild", "xcodebuild build -scheme app", 3);
        assert_eq!(
            stray(&t, 9, Some(&live), now),
            None,
            "no device named is no proof"
        );
        // `simctl` takes its device positionally; `log stream` is a tool only by its args.
        let boot = format!("simctl bootstatus {dev} -b");
        let t = proc(9, 1, "simctl", &boot, 3);
        assert_eq!(stray(&t, 9, Some(&live), now), Some("device gone"));
        let stream = format!("log stream --device {dev}");
        let t = proc(9, 1, "log", &stream, 3);
        assert_eq!(stray(&t, 9, Some(&live), now), Some("device gone"));
        let t = proc(9, 1, "log", "log show", 3);
        assert_eq!(stray(&t, 9, Some(&live), now), None);
        let t = proc(
            9,
            1,
            "WebDriverAgentRunner-Runner",
            "WebDriverAgentRunner-Runner -XCTest",
            3,
        );
        assert_eq!(
            stray(&t, 9, Some(&live), now),
            None,
            "the runner names no UDID to check"
        );
    }

    #[test]
    fn a_dead_lock_owner_with_a_reparented_holder_is_an_orphan() {
        // 15 Nov: the shard (pid 27634, `sh scripts/e2e.sh`) held the sim lock while its
        // recorded owner (23363) was dead and init had adopted it.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sim.lock");
        std::fs::create_dir(&dir).unwrap();
        let me = std::process::id();
        let t = procs(&[
            (27634, 1, 27634, "sh"),
            (2, 400, 2, "sh"),
            (23363, 1, 23363, "sh"),
        ]);
        std::fs::write(dir.join("owner"), "999999999\n").unwrap();
        std::fs::write(dir.join("pid"), "27634\n").unwrap();
        assert_eq!(lock_orphan(&dir, &t), vec![27634]);
        // A live owner holds the lock — nothing to prove.
        std::fs::write(dir.join("owner"), format!("{me}\n")).unwrap();
        assert!(lock_orphan(&dir, &t).is_empty(), "owner alive");
        // A dead holder is no orphan; a holder still under a live parent is not.
        std::fs::write(dir.join("owner"), "999999999\n").unwrap();
        std::fs::write(dir.join("pid"), "888888888\n").unwrap();
        assert!(lock_orphan(&dir, &t).is_empty(), "holder gone too");
        std::fs::write(dir.join("pid"), "2\n").unwrap();
        assert!(lock_orphan(&dir, &t).is_empty(), "holder still parented");
    }

    #[test]
    fn a_dead_lock_owner_orphans_its_whole_group() {
        // The e2e shard's owner died after a maestro failure. Left under a still-held lock:
        // the holder `sh` in its own group, the java driver and its WDA xcodebuild in the
        // dead owner's group — all orphans by definition, no matter their age or device.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sim.lock");
        std::fs::create_dir(&dir).unwrap();
        let t = procs(&[
            (48607, 1, 48607, "sh"),             // the recorded holder, its own group
            (65813, 1, 45872, "java"),           // a member of the dead owner's group
            (67397, 65813, 45872, "xcodebuild"), // a member's child, same group
            (70000, 999, 45872, "maestro"),      // a member still parented outside it
            (999, 1, 999, "zsh"),                // that live parent, a foreign group
        ]);
        std::fs::write(dir.join("owner"), "999999999\n").unwrap();
        std::fs::write(dir.join("pid"), "48607\n").unwrap();
        // No `pgid` file: nothing beyond the holder is proved.
        assert_eq!(lock_orphan(&dir, &t), vec![48607]);
        std::fs::write(dir.join("pgid"), "45872\n").unwrap();
        assert_eq!(lock_orphan(&dir, &t), vec![48607, 65813, 67397]);
    }

    #[test]
    fn orphans_sweep_their_descendants() {
        let t = procs(&[
            (10, 1, 10, "java"),
            (11, 10, 10, "xcodebuild"),
            (12, 11, 12, "runner"), // a grandchild, another group
            (20, 1, 20, "zsh"),
            (21, 20, 20, "vim"),
        ]);
        let mut proved: HashMap<u32, String> = [(10, "w".to_string())].into_iter().collect();
        descendants(&t, &mut proved);
        let mut keys: Vec<u32> = proved.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, vec![10, 11, 12]);
        assert_eq!(proved[&12], "w", "a descendant inherits its root's why");
    }

    #[test]
    fn stat_line_with_a_tricky_name() {
        let s = "42 (a) b (c)) S 7 42 42 0 -1 4194560 100 0 0 0 150 50 10 5 20 0 3 0 1000 9000 512 rest";
        assert_eq!(parse_stat(s), Some((7, 215, 512, 1000, 42)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn ps_output_parses_every_time_shape() {
        let s = "    1      0      1  12345   0:05.50 Sat Nov 15 20:00:01 2025 /sbin/launchd\n\
                 \x20  42      1      1  20480  12:34:56 Sat Nov 15 08:00:01 2025 /opt/bin/herdr\n\
                 \x20  99     42     42   4096 2-03:04:05 Sat Nov 15 18:00:01 2025 /usr/bin/claude\n\
                 \x20 100     42     42    512   7:08.09 Sat Nov 15 19:00:01 2025 -zsh\n\
                 \x20   7      1      7   1024   1:00.00 Sat Nov 15 19:30:01 2025 /opt/tool/agent-fleet -p my proj\n\
                 \x20 bad line\n";
        let t = Procs(parse_ps(s));
        assert_eq!(t.0.len(), 5);
        let p = |pid| &t.0[&pid];
        assert_eq!(p(1).cpu_ms, 5_500);
        assert_eq!(p(42).cpu_ms, (12 * 3600 + 34 * 60 + 56) * 1000);
        assert_eq!(p(99).cpu_ms, ((2 * 24 + 3) * 3600 + 4 * 60 + 5) * 1000);
        assert_eq!(p(100).cpu_ms, (7 * 60 + 8) * 1000 + 90);
        assert_eq!(
            (p(42).ppid, p(42).rss_kb, p(42).name.as_str()),
            (1, 20480, "herdr")
        );
        assert_eq!(p(99).name, "claude", "a path yields its basename");
        assert_eq!(p(7).name, "agent-fleet");
        assert_eq!(
            p(7).args.as_deref(),
            Some("/opt/tool/agent-fleet -p my proj")
        );
        assert!(p(7).started.is_some(), "lstart folds to a start time");
        assert_eq!(p(99).pgid, Some(42));
        assert_eq!(p(100).name, "zsh", "a login shell drops its dash");
        assert!(p(1).cwd.is_none() && !p(1).deleted);
        assert!(t.under(99, "herdr"), "the ancestor walk works off it");
    }

    #[test]
    fn worktree_list_skips_bare_and_prunable() {
        let s = "worktree /r/main\nHEAD aaa\nbranch refs/heads/main\n\n\
                 worktree /w/x\nHEAD bbb\ndetached\n\n\
                 worktree /w/gone\nHEAD ccc\nbranch refs/heads/y\nprunable gitdir file points to non-existent location\n";
        assert_eq!(
            parse_worktrees(s),
            vec![
                ("/r/main".into(), "aaa".into(), Some("main".into())),
                ("/w/x".into(), "bbb".into(), None),
            ]
        );
    }

    #[test]
    fn status_names_take_the_new_side_of_a_rename() {
        assert_eq!(
            parse_status(" M src/a.ts\n?? new file.md\nR  old.ts -> new.ts\n"),
            vec!["src/a.ts", "new file.md", "new.ts"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_tree() {
        let procs = Procs::read();
        let me = procs.tree(std::process::id());
        assert!(!me.is_empty());
        assert!(procs.sum(&me).0 > 0);
        assert!(procs.tree(u32::MAX).is_empty());
    }

    fn proc(ppid: u32, cpu_ms: u64, cwd: &str, deleted: bool) -> Proc {
        Proc {
            ppid,
            cpu_ms,
            rss_kb: 10,
            name: "p".into(),
            cwd: Some(cwd.into()),
            deleted,
            args: None,
            started: None,
            pgid: None,
        }
    }

    #[test]
    fn processes_group_by_worktree_and_orphans_are_flagged() {
        let procs = Procs(HashMap::from([
            (1, proc(0, 0, "/w/a/src", false)),      // the agent
            (2, proc(1, 0, "/w/a", false)),          // under the agent: not a service
            (3, proc(0, 0, "/w/a/web", false)),      // a dev server in a
            (4, proc(0, 0, "/r/main/sub", false)),   // in the main checkout
            (5, proc(0, 0, "/w/gone/x", true)),      // its worktree was deleted
            (7, proc(0, 0, "/w/app/old/y", true)),   // deleted, beside /w/app/live
            (6, proc(0, 0, "/tmp/elsewhere", true)), // deleted, but nowhere near a worktree
        ]));
        let exclude: HashSet<u32> = procs.tree(1).into_iter().collect();
        let wts = vec![
            ("/r/main".to_string(), false),
            ("/w/a".to_string(), true),
            ("/w/app/live".to_string(), true),
        ];
        let g = by_worktree(&procs, &exclude, &wts, |_| false);
        assert_eq!(
            g["/w/a"],
            Group {
                pids: vec![3],
                orphaned: false
            }
        );
        assert_eq!(g["/r/main"].pids, vec![4]);
        assert_eq!(
            g["/w/gone"],
            Group {
                pids: vec![5],
                orphaned: true
            }
        );
        assert_eq!(g["/w/app/old"].pids, vec![7], "the deepest home names it");
        assert_eq!(g.len(), 4, "{g:?}");
    }

    #[test]
    fn a_deleted_subdirectory_of_a_live_checkout_is_not_an_orphan() {
        let mut herdr = proc(0, 0, "/r", false);
        herdr.name = "herdr".into();
        let procs = Procs(HashMap::from([
            (1, herdr),
            (2, proc(1, 0, "/home/p/app", false)), // a pane shell in the main checkout
            (3, proc(0, 0, "/home/p/app", false)), // a server there
            (4, proc(0, 0, "/home/p/app/design/tmp", true)), // its subdirectory was deleted
            (5, proc(0, 0, "/home/p/gone-wt", true)), // a worktree beside it, deleted
            (6, proc(0, 0, "/home/p/still-here/x", true)), // deleted inside a dir that exists
        ]));
        let wts = vec![
            ("/home/p/app".to_string(), false),
            ("/home/p/app-wt".to_string(), true), // makes /home/p a worktree home
        ];
        let g = by_worktree(&procs, &HashSet::new(), &wts, |d| d == "/home/p/still-here");
        assert_eq!(
            g["/home/p/app"],
            Group {
                pids: vec![3, 4],
                orphaned: false
            }
        );
        assert_eq!(
            g["/home/p/gone-wt"],
            Group {
                pids: vec![5],
                orphaned: true
            }
        );
        assert_eq!(
            g.len(),
            2,
            "no pane shell, no herdr, nothing under an existing dir: {g:?}"
        );
    }

    #[test]
    fn cpu_counts_new_processes_whole() {
        let before = Procs(HashMap::from([(1, proc(0, 1000, "/", false))]));
        let after = Procs(HashMap::from([
            (1, proc(0, 1500, "/", false)),
            (2, proc(1, 300, "/", false)),
        ]));
        assert_eq!(after.cpu_pct(&before, &[1, 2], 1000), 80);
    }

    #[tokio::test]
    async fn a_squash_merged_branch_holds_no_work_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let sh = |cmd: &str| {
            let ok = std::process::Command::new("sh")
                .args(["-c", cmd])
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success();
            assert!(ok, "{cmd}");
        };
        sh(
            "git init -q --bare o.git && git init -q -b main r && cd r && git remote add origin ../o.git \
            && git commit -q --allow-empty -m base && git push -q origin main \
            && git worktree add -q ../sq -b sq && cd ../sq && echo v > c.txt && git add c.txt && git commit -q -m feature \
            && cd ../r && git worktree add -q ../wip -b wip && cd ../wip && echo w > d.txt && git add d.txt && git commit -q -m wip \
            && cd ../r && echo v > c.txt && git add c.txt && git commit -q -m 'feature (#1)' && git push -q origin main",
        );
        let r = dir.path().join("r");
        let wts = worktrees("h", r.to_str().unwrap(), None).await;
        let by = |b: &str| wts.iter().find(|w| w.branch.as_deref() == Some(b)).unwrap();
        assert!(
            by("sq").in_main && by("sq").files.is_empty(),
            "{:?}",
            by("sq")
        );
        assert!(
            !by("wip").in_main && by("wip").files == vec!["d.txt".to_string()],
            "{:?}",
            by("wip")
        );
    }

    #[tokio::test]
    async fn a_worktree_with_unpushed_and_dirty_work() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("r");
        let sh = |cmd: &str| {
            let ok = std::process::Command::new("sh")
                .args(["-c", cmd])
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
                .success();
            assert!(ok, "{cmd}");
        };
        sh(
            "git init -q -b main r && cd r && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m one \
            && echo x > a.txt && git add a.txt && git -c user.name=t -c user.email=t@t commit -q -m two \
            && git worktree add -q ../w -b feat && echo y > ../w/b.txt \
            && printf '[submodule \"lib\"]\\n\\tpath = lib\\n\\turl = x\\n' > ../w/.gitmodules && touch ../w/lib",
        );
        // git resolves the repo path; on macOS the temp dir is reached through a symlink.
        let repo = repo.canonicalize().unwrap();
        let r = repo.to_str().unwrap();
        assert_eq!(repo_of(&format!("{}/../w", r)).await.as_deref(), Some(r));
        let mut wts = worktrees("h", r, repo.parent()).await;
        wts.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(wts.len(), 2, "{wts:#?}");
        let w = wts
            .iter()
            .find(|w| w.branch.as_deref() == Some("feat"))
            .unwrap();
        assert_eq!(
            (w.unpushed, w.dirty, w.dirty_submodules),
            (2, 3, 1),
            "no remote has either commit; three files are dirty, one a submodule"
        );
        assert!(w.herdr, "a linked worktree under the herdr directory");
        let main = wts.iter().find(|w| w.path == r).unwrap();
        assert!(!main.herdr, "a main checkout is never herdr's");
        let none = worktrees("h", r, None).await;
        assert!(none.iter().all(|w| !w.herdr), "no herdr directory known");
        assert!(w.files.contains(&"b.txt".to_string()), "{:?}", w.files);
        assert!(
            !w.files.contains(&"lib".to_string()),
            "a submodule path is not work: {:?}",
            w.files
        );
        assert!(w.files.contains(&".gitmodules".to_string()));
    }

    #[test]
    fn herdrs_worktree_directory_comes_from_its_config() {
        let home = Path::new("/u");
        assert_eq!(herdr_dir("", home), Path::new("/u/.herdr/worktrees"));
        assert_eq!(
            herdr_dir("[ui]\nx = 1", home),
            Path::new("/u/.herdr/worktrees")
        );
        assert_eq!(
            herdr_dir("[worktrees]\ndirectory = \"~/wt\"", home),
            Path::new("/u/wt")
        );
        assert_eq!(
            herdr_dir("[worktrees]\ndirectory = \"/srv/wt\"", home),
            Path::new("/srv/wt")
        );
    }

    #[test]
    fn a_programs_own_daemon_is_infrastructure_not_a_session() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_infra(&argv(&["/x/bin/codex", "app-server", "daemon"])));
        assert!(is_infra(&argv(&["/x/bin/claude", "daemon"])));
        assert!(!is_infra(&argv(&["/x/bin/codex"])));
        assert!(!is_infra(&argv(&[
            "/x/bin/claude",
            "--dangerously-skip-permissions"
        ])));
        // a "daemon" inside a -p prompt is no daemon — ps splits the quoted argument
        assert!(!is_infra(&argv(&[
            "/x/bin/claude",
            "-p",
            "restart",
            "the",
            "daemon"
        ])));
        assert!(!is_infra(&argv(&["/x/bin/claude", "-p", "daemon"])));
    }

    #[test]
    fn a_daemons_children_are_infrastructure_too() {
        let t = procs(&[(1, 0, 1, "codex"), (2, 1, 1, "codex"), (3, 2, 2, "codex")]);
        let infra: HashSet<u32> = [1].into_iter().collect();
        assert!(t.under_any(3, &infra));
        assert!(!t.under_any(3, &HashSet::new()));
    }

    #[test]
    fn lsof_is_found_by_absolute_path_when_the_supervisor_path_lacks_it() {
        assert_eq!(pick_lsof(|p| p == "/usr/sbin/lsof"), "/usr/sbin/lsof");
        assert_eq!(pick_lsof(|p| p == "/usr/bin/lsof"), "/usr/bin/lsof");
        assert_eq!(
            pick_lsof(|_| true),
            "/usr/sbin/lsof",
            "macOS keeps it in sbin"
        );
        assert_eq!(pick_lsof(|_| false), "lsof", "else whatever PATH finds");
    }
}
