//! Where the host reads each agent program's usage quota: codex's newest session log, the file
//! `agent-fleet hook statusline` keeps current for claude, and the devin CLI's cached
//! `devin auth status`. The I/O is thin; the parsing is pure and tested on fixtures.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::client::Paths;
use crate::event::{AgentQuota, QuotaWindow};

/// `$CODEX_HOME/sessions`, default `~/.codex/sessions`.
pub fn codex_sessions() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
        .map(|d| d.join("sessions"))
}

/// The platform cache dir's `devin/cli` (`~/.cache/devin/cli` on Linux).
pub fn devin_cache() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.cache_dir().join("devin").join("cli"))
}

/// The machine-wide file `hook statusline` writes, in the platform data dir root.
pub fn claude_file() -> Option<PathBuf> {
    Paths::base()
        .ok()
        .map(|d| d.join("claude-rate-limits.json"))
}

fn home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// A program's quota is published when two reads differ in anything but `read_at`.
pub fn changed(a: &AgentQuota, b: &AgentQuota) -> bool {
    a.host != b.host
        || a.kind != b.kind
        || a.windows != b.windows
        || a.plan != b.plan
        || a.exhausted != b.exhausted
}

/// The newest file named `<prefix>*.<ext>` under `dir`, by mtime, searching recursively.
fn newest(dir: &Path, prefix: &str, ext: &str) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                todo.push(p);
            } else if name.starts_with(prefix) && name.ends_with(&format!(".{ext}")) {
                let m = e.metadata().ok()?.modified().ok()?;
                if best.as_ref().is_none_or(|(_, t)| m > *t) {
                    best = Some((p, m));
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

/// The last `max` bytes of `file` as text, starting at a line boundary.
fn tail(file: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(file).ok()?;
    let skip = f.metadata().ok()?.len().saturating_sub(max);
    f.seek(SeekFrom::Start(skip)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if skip > 0 {
        text = text.split_once('\n').map(|(_, r)| r.to_string())?;
    }
    Some(text)
}

/// A timestamp: unix seconds (or millis, over 1e12), or an RFC 3339 string.
fn ts(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(n) = v.as_i64() {
        let (s, n) = if n > 1_000_000_000_000 {
            (n / 1000, (n % 1000) * 1_000_000)
        } else {
            (n, 0)
        };
        DateTime::from_timestamp(s, n as u32)
    } else {
        DateTime::parse_from_rfc3339(v.as_str()?)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

// ---------- codex ----------

/// The quota codex's newest session log under `sessions` reports.
pub fn codex(host: &str, sessions: &Path) -> Option<AgentQuota> {
    let file = newest(sessions, "", "jsonl")?;
    codex_tail(host, &tail(&file, 256 * 1024)?)
}

fn window_name(minutes: u64) -> String {
    match minutes {
        300 => "5h".into(),
        1440 => "day".into(),
        10080 => "week".into(),
        n => format!("{n}m"),
    }
}

fn codex_tail(host: &str, tail: &str) -> Option<AgentQuota> {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let rl = &v["payload"]["rate_limits"];
        if v["type"] != "event_msg" || v["payload"]["type"] != "token_count" || !rl.is_object() {
            continue;
        }
        let read_at = ts(&v["timestamp"])?;
        let windows: Vec<QuotaWindow> = ["primary", "secondary"]
            .iter()
            .filter_map(|k| {
                let w = &rl[k];
                Some(QuotaWindow {
                    name: window_name(w["window_minutes"].as_u64()?),
                    used_pct: w["used_percent"].as_f64()?,
                    resets_at: ts(&w["resets_at"]),
                })
            })
            .collect();
        if !windows.is_empty() {
            return Some(AgentQuota {
                host: host.into(),
                kind: "codex".into(),
                windows,
                plan: rl["plan_type"].as_str().map(str::to_string),
                exhausted: !rl["rate_limit_reached_type"].is_null(),
                read_at,
            });
        }
    }
    None
}

// ---------- claude ----------

/// claude's quota, from the file `agent-fleet hook statusline` keeps current.
pub fn claude(host: &str, file: &Path) -> Option<AgentQuota> {
    claude_quota(host, &std::fs::read_to_string(file).ok()?)
}

fn claude_quota(host: &str, stored: &str) -> Option<AgentQuota> {
    let v: Value = serde_json::from_str(stored).ok()?;
    let read_at = ts(&v["read_at"])?;
    let windows: Vec<QuotaWindow> = [("five_hour", "5h"), ("seven_day", "week")]
        .iter()
        .filter_map(|(k, name)| {
            let w = &v["rate_limits"][k];
            Some(QuotaWindow {
                name: name.to_string(),
                used_pct: w["used_percentage"].as_f64()?,
                resets_at: ts(&w["resets_at"]),
            })
        })
        .collect();
    if windows.is_empty() {
        return None;
    }
    let exhausted = windows.iter().any(|w| w.used_pct >= 100.0);
    Some(AgentQuota {
        host: host.into(),
        kind: "claude".into(),
        windows,
        plan: None,
        exhausted,
        read_at,
    })
}

// ---------- devin ----------

/// devin's quota, from the newest `user_status.<identity>.bin` the CLI cached in `dir`.
pub fn devin(host: &str, dir: &Path) -> Option<AgentQuota> {
    let file = newest(dir, "user_status.", "bin")?;
    devin_quota(host, &std::fs::read_to_string(file).ok()?)
}

fn devin_quota(host: &str, file: &str) -> Option<AgentQuota> {
    let v: Value = serde_json::from_str(file).ok()?;
    let read_at = DateTime::from_timestamp(v["fetched_at_secs"].as_i64()?, 0)?;
    let raw = b64(v["payload"].as_str()?)?;
    let plan = bytes_at(&raw, 13)?;
    // The plan name ("Pro") is a string under field 1's sub-message, field 2.
    let plan_name = bytes_at(plan, 1)
        .and_then(|b| bytes_at(b, 2))
        .and_then(|b| std::str::from_utf8(b).ok())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == ' '))
        .map(str::to_string);
    let remaining = |n| varint_at(plan, n).unwrap_or(0); // proto3 omits zeros
    let (day, week) = (remaining(14), remaining(15));
    let used = |r: u64| (100.0 - r as f64).clamp(0.0, 100.0);
    let resets = |n| varint_at(plan, n).and_then(|s| DateTime::from_timestamp(s as i64, 0));
    Some(AgentQuota {
        host: host.into(),
        kind: "devin".into(),
        windows: vec![
            QuotaWindow {
                name: "day".into(),
                used_pct: used(day),
                resets_at: resets(17),
            },
            QuotaWindow {
                name: "week".into(),
                used_pct: used(week),
                resets_at: resets(18),
            },
        ],
        plan: plan_name,
        exhausted: day == 0 || week == 0,
        read_at,
    })
}

/// A decoded protobuf field value.
enum Pb<'a> {
    V(u64),
    B(&'a [u8]),
}

fn varint_at(b: &[u8], want: u64) -> Option<u64> {
    match find(b, want)? {
        Pb::V(v) => Some(v),
        _ => None,
    }
}

fn bytes_at(b: &[u8], want: u64) -> Option<&[u8]> {
    match find(b, want)? {
        Pb::B(v) => Some(v),
        _ => None,
    }
}

/// Field `want`'s value in the protobuf message `b`.
fn find<'a>(b: &'a [u8], want: u64) -> Option<Pb<'a>> {
    let mut at = 0;
    while at < b.len() {
        let tag = varint(b, &mut at)?;
        let (num, wire) = (tag >> 3, tag & 7);
        match wire {
            0 => {
                let v = varint(b, &mut at)?;
                if num == want {
                    return Some(Pb::V(v));
                }
            }
            2 => {
                let n = varint(b, &mut at)? as usize;
                let end = at.checked_add(n)?;
                let s = b.get(at..end)?;
                if num == want {
                    return Some(Pb::B(s));
                }
                at = end;
            }
            1 | 5 => {
                at = at.checked_add(if wire == 1 { 8 } else { 4 })?;
                if at > b.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

fn varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0;
    for i in 0..10 {
        let byte = *b.get(*at)?;
        *at += 1;
        v |= ((byte & 0x7f) as u64) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Standard base64, padding optional; no crate for a few lines.
fn b64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let n = chunk
            .iter()
            .try_fold(0u32, |n, &c| Some((n << 6) | val(c)? as u32))?
            << (6 * (4 - chunk.len()));
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

// ---------- the statusline hook ----------

/// `agent-fleet hook statusline [-- <cmd>…]`: Claude Code's status line, wrapped. The rate
/// limits it is fed are stored for the host to publish; a wrapped command gets the same stdin
/// and its stdout passes through. A status line must never break: every error is swallowed.
/// When `context_window.used_percentage` is at the context-full mark and the process sits in
/// a herdr pane, it also writes an `agent.signal` `context_full`, once per session.
pub async fn statusline(
    cmd: &[String],
    paths: &Paths,
    hub: Option<&str>,
    token_file: Option<&Path>,
    project: &str,
    host: &str,
) {
    let mut input = Vec::new();
    let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut input);
    if let Some(file) = claude_file() {
        store_limits(&input, &file);
    }
    if let Ok(v) = serde_json::from_slice::<Value>(&input) {
        crate::signal::context_full(paths, hub, token_file, project, host, &v).await;
    }
    let Some((prog, args)) = cmd.split_first() else {
        return;
    };
    let Ok(mut child) = std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = std::io::Write::write_all(&mut stdin, &input);
    }
    let _ = child.wait();
}

/// What a status line fed `input` stores in `file`: its `rate_limits` with `read_at` stamped.
fn store_limits(input: &[u8], file: &Path) {
    let Ok(v) = serde_json::from_slice::<Value>(input) else {
        return;
    };
    let Some(limits) = v.get("rate_limits").filter(|l| l.is_object()) else {
        return;
    };
    let body = json!({ "read_at": Utc::now(), "rate_limits": limits }).to_string();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn b64e(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in bytes.chunks(3) {
            let n = c.iter().fold(0u32, |n, &b| (n << 8) | b as u32) << (8 * (3 - c.len()));
            for i in 0..=c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
            s.extend(std::iter::repeat_n('=', 3 - c.len()));
        }
        s
    }

    #[test]
    fn codex_reads_the_last_token_count() {
        let line = |payload: Value| {
            json!({ "timestamp": "2026-10-17T10:00:00Z", "type": "event_msg", "payload": payload })
                .to_string()
        };
        let limits = json!({
            "primary": { "used_percent": 45.0, "window_minutes": 300, "resets_at": 1_800_000_000 },
            "secondary": { "used_percent": 8.0, "window_minutes": 10080, "resets_at": 1_800_500_000 },
            "plan_type": "Pro",
            "rate_limit_reached_type": null,
        });
        let tail = [
            line(json!({ "type": "other" })),
            line(json!({ "type": "token_count", "rate_limits": limits })),
            line(json!({ "type": "turn_context" })),
            "not json".into(),
        ]
        .join("\n");
        let q = codex_tail("h", &tail).unwrap();
        assert_eq!(q.kind, "codex");
        assert_eq!(q.plan.as_deref(), Some("Pro"));
        assert!(!q.exhausted);
        assert_eq!(q.read_at.to_rfc3339(), "2026-10-17T10:00:00+00:00");
        assert_eq!(
            q.windows
                .iter()
                .map(|w| (w.name.as_str(), w.used_pct as u32, w.resets_at.is_some()))
                .collect::<Vec<_>>(),
            [("5h", 45, true), ("week", 8, true)]
        );
    }

    #[test]
    fn codex_exhausted_and_window_names() {
        let line = |mins: u64, reached: Value| {
            json!({ "timestamp": "2026-10-17T10:00:00Z", "type": "event_msg", "payload": {
                "type": "token_count", "rate_limits": {
                    "primary": { "used_percent": 100, "window_minutes": mins },
                    "plan_type": "plus", "rate_limit_reached_type": reached } } })
            .to_string()
        };
        let q = codex_tail("h", &line(1440, json!("primary"))).unwrap();
        assert_eq!(q.windows[0].name, "day");
        assert!(q.exhausted);
        let q = codex_tail("h", &line(90, Value::Null)).unwrap();
        assert_eq!(q.windows[0].name, "90m");
        assert!(!q.exhausted);
        assert!(codex_tail("h", "{\"type\":\"x\"}\n").is_none());
    }

    #[test]
    fn codex_skips_a_big_file_to_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let f = sessions.join("2026").join("10").join("s.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        let mut big = "x".repeat(300 * 1024) + "\n";
        big += &json!({ "timestamp": "2026-10-17T10:00:00Z", "type": "event_msg", "payload": {
            "type": "token_count", "rate_limits": {
                "primary": { "used_percent": 3, "window_minutes": 300 },
                "plan_type": "free", "rate_limit_reached_type": null } } })
        .to_string();
        std::fs::write(&f, big).unwrap();
        let q = codex("h", &sessions).unwrap();
        assert_eq!(q.plan.as_deref(), Some("free"));
        assert_eq!(q.windows[0].used_pct, 3.0);
    }

    #[test]
    fn claude_reads_the_statusline_file() {
        let stored = json!({ "read_at": "2026-10-17T10:00:00Z", "rate_limits": {
            "five_hour": { "used_percentage": 45.0, "resets_at": "2026-10-17T22:10:00Z" },
            "seven_day": { "used_percentage": 8, "resets_at": 1_800_500_000 } } })
        .to_string();
        let q = claude_quota("h", &stored).unwrap();
        assert_eq!(q.kind, "claude");
        assert!(!q.exhausted);
        assert_eq!(
            q.windows
                .iter()
                .map(|w| (w.name.as_str(), w.used_pct as u32))
                .collect::<Vec<_>>(),
            [("5h", 45), ("week", 8)]
        );
        assert_eq!(
            q.windows[0].resets_at.unwrap().to_rfc3339(),
            "2026-10-17T22:10:00+00:00"
        );
        let full = stored.replace("45.0", "100");
        assert!(claude_quota("h", &full).unwrap().exhausted);
        // No rate_limits key: an API-key user has no quota to report.
        assert!(claude_quota("h", "{\"read_at\":1}").is_none());
    }

    #[test]
    fn devin_walks_the_protobuf() {
        let mut inner = Vec::new();
        // field 1: {2: "Pro"} — the plan name
        let mut sub = Vec::new();
        varint(2 << 3 | 2, &mut sub);
        varint(3, &mut sub);
        sub.extend(b"Pro");
        varint(1 << 3 | 2, &mut inner);
        varint(sub.len() as u64, &mut inner);
        inner.extend(&sub);
        for (n, v) in [
            (14, 30),             // daily remaining %
            (15, 80),             // weekly remaining %
            (16, (-5i64) as u64), // overage micros: a negative int64, 10-byte varint
            (17, 1_800_000_000),
            (18, 1_800_500_000),
        ] {
            varint(n << 3, &mut inner);
            varint(v, &mut inner);
        }
        let mut msg = Vec::new();
        varint(13 << 3 | 2, &mut msg);
        varint(inner.len() as u64, &mut msg);
        msg.extend(&inner);
        let file = json!({ "version": 1, "identity_digest": "x",
            "fetched_at_secs": 1_799_000_000, "payload": b64e(&msg) })
        .to_string();
        let q = devin_quota("h", &file).unwrap();
        assert_eq!(q.kind, "devin");
        assert!(!q.exhausted);
        assert_eq!(q.plan.as_deref(), Some("Pro"));
        assert_eq!(
            q.read_at,
            DateTime::from_timestamp(1_799_000_000, 0).unwrap()
        );
        assert_eq!(
            q.windows
                .iter()
                .map(|w| (w.name.as_str(), w.used_pct as u32, w.resets_at.is_some()))
                .collect::<Vec<_>>(),
            [("day", 70, true), ("week", 20, true)]
        );
    }

    #[test]
    fn devin_an_absent_remaining_percent_is_exhausted() {
        let mut inner = Vec::new();
        varint(15 << 3, &mut inner);
        varint(50, &mut inner); // only the weekly field is present
        let mut msg = Vec::new();
        varint(13 << 3 | 2, &mut msg);
        varint(inner.len() as u64, &mut msg);
        msg.extend(&inner);
        let file = json!({ "fetched_at_secs": 1, "payload": b64e(&msg) }).to_string();
        let q = devin_quota("h", &file).unwrap();
        assert!(q.exhausted, "no daily field = 0 remaining");
        assert_eq!(q.windows[0].used_pct, 100.0);
        assert_eq!(q.windows[1].used_pct, 50.0);
        assert_eq!(q.plan, None);
    }

    #[test]
    fn b64_decodes_what_it_encoded() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(b64(&b64e(&bytes)).unwrap(), bytes);
        assert_eq!(b64("aGk").unwrap(), b"hi");
        assert_eq!(b64("aGk=").unwrap(), b"hi");
        assert!(b64("!bad").is_none());
        assert!(b64("a").is_none());
    }

    #[test]
    fn changed_ignores_read_at() {
        let now = Utc::now();
        let q = AgentQuota {
            host: "h".into(),
            kind: "claude".into(),
            windows: vec![],
            plan: None,
            exhausted: false,
            read_at: now,
        };
        let mut later = q.clone();
        later.read_at = now + chrono::Duration::minutes(1);
        assert!(!changed(&q, &later));
        later.exhausted = true;
        assert!(changed(&q, &later));
    }

    #[test]
    fn the_statusline_store_is_silent_when_there_is_nothing_to_store() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("claude-rate-limits.json");
        store_limits(b"not json", &file);
        store_limits(br#"{"model":"x"}"#, &file);
        assert!(!file.exists());
        store_limits(
            br#"{"rate_limits":{"five_hour":{"used_percentage":7}}}"#,
            &file,
        );
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(stored["rate_limits"]["five_hour"]["used_percentage"], 7);
        assert!(stored["read_at"].is_string());
    }
}
