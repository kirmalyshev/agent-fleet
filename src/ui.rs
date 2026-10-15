//! Text views. Each is a pure function of the state and the current time.

use std::fmt::Write;

use chrono::{DateTime, Utc};

use crate::state::State;

pub fn ago(now: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let s = (now - ts).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
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
