//! What the host agent samples on its machine: the cost of each agent's process tree, and the
//! git worktrees its agents work in.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

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
}

/// Every process on this machine at one moment. Empty off Linux.
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
        let mut all = HashMap::new();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Self(all);
        };
        for entry in dir.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            let Some((ppid, ticks, pages)) = std::fs::read_to_string(entry.path().join("stat"))
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
            all.insert(
                pid,
                Proc {
                    ppid,
                    cpu_ms: ticks * 1000 / tick,
                    rss_kb: pages * page / 1024,
                    name: name.trim().to_string(),
                    cwd: cwd.map(|c| c.trim_end_matches(" (deleted)").to_string()),
                    deleted,
                },
            );
        }
        Self(all)
    }

    #[cfg(not(target_os = "linux"))]
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
fn is_terminal(procs: &Procs, p: &Proc) -> bool {
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

/// `/proc/<pid>/stat` → (ppid, utime + stime + cutime + cstime ticks, rss pages).
pub fn parse_stat(s: &str) -> Option<(u32, u64, u64)> {
    // The command name may hold spaces and parens; the fields after the last `)` do not.
    let f: Vec<&str> = s[s.rfind(')')? + 1..].split_whitespace().collect();
    let n = |i: usize| f.get(i)?.parse::<u64>().ok();
    Some((n(1)? as u32, n(11)? + n(12)? + n(13)? + n(14)?, n(21)?))
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

/// Every worktree of `repo`, as `host` sees it now.
pub async fn worktrees(host: &str, repo: &str) -> Vec<WorktreeState> {
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
            path,
            repo: repo.to_string(),
            branch,
            head,
            unpushed,
            dirty: dirty.len() as u32,
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

/// The working directory of a live process on this machine.
pub fn cwd_of(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
    }
    let out = std::process::Command::new("lsof")
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
    fn stat_line_with_a_tricky_name() {
        let s = "42 (a) b (c)) S 7 42 42 0 -1 4194560 100 0 0 0 150 50 10 5 20 0 3 0 1000 9000 512 rest";
        assert_eq!(parse_stat(s), Some((7, 215, 512)));
        assert_eq!(parse_stat("garbage"), None);
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
        let wts = worktrees("h", r.to_str().unwrap()).await;
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
        let r = repo.to_str().unwrap();
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
        assert_eq!(repo_of(&format!("{}/../w", r)).await.as_deref(), Some(r));
        let mut wts = worktrees("h", r).await;
        wts.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(wts.len(), 2, "{wts:#?}");
        let w = wts
            .iter()
            .find(|w| w.branch.as_deref() == Some("feat"))
            .unwrap();
        assert_eq!(
            (w.unpushed, w.dirty),
            (2, 3),
            "no remote has either commit; three files are dirty"
        );
        assert!(w.files.contains(&"b.txt".to_string()), "{:?}", w.files);
        assert!(
            !w.files.contains(&"lib".to_string()),
            "a submodule path is not work: {:?}",
            w.files
        );
        assert!(w.files.contains(&".gitmodules".to_string()));
    }
}
