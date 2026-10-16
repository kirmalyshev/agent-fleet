//! Text views. Each is a pure function of the state and the current time.

use std::fmt::Write;

use chrono::{DateTime, Utc};

use crate::state::{Hold, State};
use crate::stats::{Dist, LogStats};

pub fn ago(now: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let s = (now - ts).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

/// `pid 42 "e2e, 3 flows" in wt-a, 12m`: the cwd's last component, not the whole path.
pub fn hold(now: DateTime<Utc>, h: &Hold) -> String {
    let mut out = format!("pid {}", h.pid);
    if let Some(sc) = &h.scope {
        let _ = write!(out, " \"{sc}\"");
    }
    if let Some(c) = &h.cwd {
        let _ = write!(out, " in {}", c.rsplit('/').next().unwrap_or(c));
    }
    let _ = write!(out, ", {}", ago(now, h.since));
    out
}

/// Each held or queued-for resource, then the capacity rules.
pub fn resources(s: &State, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    for (key, r) in &s.resources {
        let holder = r
            .holder
            .as_ref()
            .map_or("free".to_string(), |h| hold(now, h));
        let _ = writeln!(out, "{key:<24} {holder}");
        for (i, w) in r.waiting.iter().enumerate() {
            let _ = writeln!(out, "{:<24}   {}. {}", "", i + 1, hold(now, w));
        }
    }
    for (key, l) in &s.limits {
        let until = l.until.map_or(String::new(), |u| {
            format!(" until {}", u.with_timezone(&chrono::Local).format("%H:%M"))
        });
        let _ = writeln!(
            out,
            "{key:<24} rule: at most {} of {}{until}",
            l.max,
            l.resources.join(" / ")
        );
    }
    out
}

fn state_name<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn ps(s: &State, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "{:<44} {:<8} {:>7}  {:<8} TASK\n",
        "AGENT", "STATE", "FOR", "KIND"
    );
    for (id, a) in &s.agents {
        let task = s
            .tasks
            .iter()
            .filter(|(t, _)| s.owner_of(t) == Some(id))
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            out,
            "{id:<44} {:<8} {:>7}  {:<8} {}",
            state_name(a.state),
            ago(now, a.since),
            a.kind.as_deref().unwrap_or("-"),
            if task.is_empty() { "-" } else { &task }
        );
    }
    if s.agents.is_empty() {
        out.push_str("(no agents up)\n");
    }
    let res = resources(s, now);
    if !res.is_empty() {
        let _ = write!(out, "\n{:<24} HOLDER / QUEUE\n{res}", "RESOURCE");
    }
    out
}

pub fn tasks(s: &State, all: bool, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "{:<28} {:<15} {:>7}  {:<36} TITLE\n",
        "TASK", "STATE", "FOR", "OWNER"
    );
    for (id, t) in s
        .tasks
        .iter()
        .filter(|(_, t)| all || !t.state.is_terminal())
    {
        let blocked = if t.blocked_by.is_empty() {
            String::new()
        } else {
            format!(
                "  [blocked by {}]",
                t.blocked_by.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        let _ = writeln!(
            out,
            "{id:<28} {:<15} {:>7}  {:<36} {}{blocked}",
            state_name(t.state),
            ago(now, t.since),
            t.owner.as_deref().unwrap_or("-"),
            t.title
        );
    }
    out
}

pub fn task(s: &State, id: &str, now: DateTime<Utc>) -> Option<String> {
    let t = s.tasks.get(id)?;
    let mut out = format!("{id}  {}\n", t.title);
    let _ = writeln!(out, "  project   {}", t.project);
    let _ = writeln!(
        out,
        "  state     {} for {}",
        state_name(t.state),
        ago(now, t.since)
    );
    let _ = writeln!(out, "  owner     {}", t.owner.as_deref().unwrap_or("-"));
    if let Some(l) = &t.lead {
        let _ = writeln!(out, "  lead      {l}");
    }
    if let Some(u) = &t.url {
        let _ = writeln!(out, "  spec      {u}");
    }
    for d in &t.blocked_by {
        let _ = writeln!(out, "  blocked by {d}");
    }
    let blocks: Vec<_> = s
        .tasks
        .iter()
        .filter(|(_, o)| o.blocked_by.contains(id))
        .map(|(k, _)| k.as_str())
        .collect();
    if !blocks.is_empty() {
        let _ = writeln!(out, "  blocks    {}", blocks.join(", "));
    }
    for (bid, b) in s
        .blockers
        .iter()
        .filter(|(_, b)| b.task.as_deref() == Some(id))
    {
        let _ = writeln!(
            out,
            "  BLOCKER {bid}: {} ({} ago, by {})",
            b.what,
            ago(now, b.since),
            b.by
        );
    }
    for (aid, a) in s.asks.iter().filter(|(_, a)| a.task.as_deref() == Some(id)) {
        let _ = writeln!(
            out,
            "  ASK {aid}: {} [{}]",
            a.question,
            a.options.join(" | ")
        );
    }
    for h in &t.handoffs {
        let _ = writeln!(out, "  handoff   {h}");
    }
    Some(out)
}

pub fn board(s: &State, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "== agents ({})", s.agents.len());
    out.push_str(&ps(s, now));
    let _ = writeln!(
        out,
        "\n== tasks ({} open)",
        s.tasks.values().filter(|t| !t.state.is_terminal()).count()
    );
    out.push_str(&tasks(s, false, now));
    let _ = writeln!(out, "\n== blockers ({})", s.blockers.len());
    for (id, b) in &s.blockers {
        let _ = writeln!(
            out,
            "{id}  {:>7}  {}  {}  owner={}  unblocks when: {}",
            ago(now, b.since),
            b.task.as_deref().unwrap_or("-"),
            b.what,
            b.owner.as_deref().unwrap_or("-"),
            b.unblocks_when.as_deref().unwrap_or("-")
        );
    }
    let _ = writeln!(out, "\n== asks ({})", s.asks.len());
    for (id, a) in &s.asks {
        let rec = a
            .recommendation
            .as_deref()
            .map(|r| format!(" (recommends {r})"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{id}  {:>7}  {}  {}{rec}",
            ago(now, a.since),
            a.task.as_deref().unwrap_or("-"),
            a.question
        );
    }
    let _ = writeln!(out, "\n== undelivered doorbells ({})", s.msgs.len());
    for (id, m) in &s.msgs {
        let _ = writeln!(
            out,
            "{id}  {:>7}  {} → {}  {} {}",
            ago(now, m.since),
            m.from,
            m.to,
            m.kind,
            m.reference
        );
    }
    out.push('\n');
    out.push_str(&doctor(&crate::doctor::findings(s, now)));
    let _ = writeln!(out, "\n== hosts ({})", s.hosts.len());
    for (h, x) in &s.hosts {
        let gb = |b: u64| b as f64 / 1e9;
        let _ = writeln!(
            out,
            "{h}  disk {:.1}/{:.1} GB free  agents {}  ({} ago)",
            gb(x.disk_free),
            gb(x.disk_total),
            x.agents,
            ago(now, x.ts)
        );
    }
    out
}

pub fn doctor(f: &[crate::doctor::Finding]) -> String {
    let mut out = format!("== findings ({})\n", f.len());
    for x in f {
        let _ = writeln!(out, "{:<10} {}  {}", x.kind, x.subject, x.detail);
    }
    out
}

/// `ms` as `850ms`, `1.2s`, `3m10s`.
pub fn dur(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        60_000..3_600_000 => format!("{}m{:02}s", ms / 60_000, ms / 1000 % 60),
        _ => format!("{}h{:02}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

fn dist(name: &str, d: &Dist, f: fn(u64) -> String) -> String {
    if d.n == 0 {
        return format!("{name:<8} -\n");
    }
    format!(
        "{name:<8} p50 {:<8} p90 {:<8} p99 {:<8} max {:<8} n={}\n",
        f(d.p50),
        f(d.p90),
        f(d.p99),
        f(d.max),
        d.n
    )
}

fn mb(kb: u64) -> String {
    if kb < 1024 {
        return format!("{kb} KB");
    }
    format!("{:.1} MB", kb as f64 / 1024.0)
}

/// `GET /stats` as text.
pub fn stats(v: &serde_json::Value) -> anyhow::Result<String> {
    let log: LogStats = serde_json::from_value(v["log"].clone())?;
    let hub = &v["hub"];
    let u = |k: &str| hub[k].as_u64().unwrap_or(0);
    let mut o = String::new();
    let d = &log.doorbells;
    let _ = writeln!(o, "== doorbells (last {})", dur(log.window_s as u64 * 1000));
    let _ = writeln!(
        o,
        "sent {}  delivered {}  pending {}  lost {} (>{}m)  retried {}",
        d.sent,
        d.delivered,
        d.pending,
        d.lost,
        crate::stats::LOST_AFTER_S / 60,
        d.retried
    );
    o += &dist("total", &d.total_ms, dur);
    o += &dist("wait", &d.wait_ms, dur);
    o += &dist("react", &d.react_ms, dur);
    let p = &log.presence;
    let _ = writeln!(
        o,
        "\n== presence\nagents {}  up {}  down {}  state changes {}  flaps (<5s) {}  unknown {}",
        p.agents, p.ups, p.downs, p.state_changes, p.flaps, p.unknown_states
    );
    let _ = writeln!(o, "\n== events\n{} in window", log.events);
    o += &dist("lag", &log.ingest_lag_ms, dur);
    let types: Vec<String> = log
        .by_type
        .iter()
        .map(|(t, n)| format!("{t} {n}"))
        .collect();
    let _ = writeln!(o, "{}", types.join(", "));
    let _ = writeln!(o, "\n== hosts");
    for (name, h) in &log.hosts {
        let _ = write!(
            o,
            "{name}  reports {}/{}  max gap {}  restarts {}",
            h.reports,
            h.expected,
            dur(h.max_gap_s as u64 * 1000),
            h.restarts
        );
        if let Some(s) = &h.last {
            let _ = write!(
                o,
                "  up {}  rss {}  cpu {}  rings {} (failed {})  herdr errors {}  write failures {}  spooled {}",
                dur(s.uptime_s * 1000),
                mb(s.max_rss_kb),
                dur(s.cpu_ms),
                s.rings,
                s.ring_failures,
                s.herdr_errors,
                s.write_failures,
                s.spooled
            );
        }
        o.push('\n');
    }
    let _ = writeln!(
        o,
        "\n== hub\nup {}  rss {}  cpu {}  events {}  db {}  followers {}",
        dur(u("uptime_s") * 1000),
        mb(u("max_rss_kb")),
        dur(u("cpu_ms")),
        u("events"),
        mb(u("db_bytes") / 1024),
        u("followers")
    );
    let post: Dist = serde_json::from_value(hub["post_us"].clone())?;
    o += &dist("post", &post, |us| {
        if us < 1000 {
            format!("{us}µs")
        } else {
            dur(us / 1000)
        }
    });
    let outcomes: Vec<String> = hub["outcomes"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, n)| format!("{k} {n}"))
        .collect();
    let _ = writeln!(o, "outcomes: {}", outcomes.join(", "));
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_buckets() {
        let now = Utc::now();
        let s = |n: i64| ago(now, now - chrono::Duration::seconds(n));
        assert_eq!(
            [s(5), s(125), s(3_725), s(200_000)],
            ["5s", "2m", "1h02m", "2d"]
        );
    }
}
