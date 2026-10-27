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

/// `ts` in local time: "21:00" today, "17 Oct 21:00" any other day.
pub fn at(now: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let t = ts.with_timezone(&chrono::Local);
    if t.date_naive() == now.with_timezone(&chrono::Local).date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%d %b %H:%M").to_string()
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
            if now >= u {
                " expired".to_string()
            } else {
                format!(" until {}", at(now, u))
            }
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

/// `<host>:<kind>  plan  5h 45% (resets 22:10) · week 8%  read 3m ago  EXHAUSTED`.
pub fn quotas(s: &State, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    for (key, q) in &s.quotas {
        let windows = q
            .windows
            .iter()
            .map(|w| match w.resets_at {
                Some(r) => format!("{} {:.0}% (resets {})", w.name, w.used_pct, at(now, r)),
                None => format!("{} {:.0}%", w.name, w.used_pct),
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let _ = writeln!(
            out,
            "{key:<20} {:<8}{windows}  read {} ago{}",
            q.plan.as_deref().unwrap_or("-"),
            ago(now, q.read_at),
            if q.exhausted_at(now) {
                "  EXHAUSTED"
            } else {
                ""
            },
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

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// `kind` for views: `kind·profile` when the fleet spawned the agent.
pub fn kind_profile(s: &State, id: &str, kind: Option<&str>) -> String {
    match (kind, s.profile_of(id)) {
        (Some(k), Some(p)) => format!("{k}·{p}"),
        (Some(k), None) => k.to_string(),
        (None, Some(p)) => format!("?·{p}"),
        (None, None) => "-".into(),
    }
}

pub fn ps(s: &State, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "{:<44} {:<8} {:<13} {:>7}  {:<14} {:<9} {:<12} TASK\n",
        "AGENT", "STATE", "STUCK", "FOR", "KIND", "ROLE", "LEAD"
    );
    for (id, a) in &s.agents {
        let task = s
            .tasks
            .iter()
            .filter(|(t, _)| s.owner_of(t) == Some(id))
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let role = s.roles.get(id);
        let _ = writeln!(
            out,
            "{id:<44} {:<8} {:<13} {:>7}  {:<14} {:<9} {:<12} {}",
            state_name(a.state),
            s.stuck.get(id).map(|st| st.cause.name()).unwrap_or(""),
            ago(now, a.since),
            kind_profile(s, id, a.kind.as_deref()),
            role.map(|r| r.level.short()).unwrap_or(""),
            role.and_then(|r| r.lead.as_deref())
                .map(short)
                .unwrap_or(""),
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
    let q = quotas(s, now);
    if !q.is_empty() {
        let _ = write!(out, "\n{:<20} PLAN    WINDOWS\n{q}", "QUOTA");
    }
    out
}

/// `agent-fleet role <agent>` and the TUI's agent detail: the level and domain, the chain of
/// leads up to the root, and the direct reports. Empty when no role is declared.
pub fn role_lines(s: &State, id: &str, now: DateTime<Utc>) -> Vec<String> {
    let Some(r) = s.roles.get(id) else {
        return Vec::new();
    };
    let domain = r
        .domain
        .as_ref()
        .map(|d| format!(" · {d}"))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "{id}  {}{domain}  ({} ago, by {})",
        r.level,
        ago(now, r.since),
        r.by
    )];
    let chain: Vec<String> = s
        .chain(id)
        .iter()
        .map(|l| {
            s.roles.get(*l).map_or_else(
                || short(l).to_string(),
                |r| format!("{} ({})", short(l), r.level.short()),
            )
        })
        .collect();
    if !chain.is_empty() {
        lines.push(format!("  reports to {}", chain.join(" → ")));
    }
    let reports: Vec<String> = s
        .reports(id)
        .iter()
        .map(|k| format!("{} ({})", short(k), s.roles[*k].level.short()))
        .collect();
    if !reports.is_empty() {
        lines.push(format!("  reports: {}", reports.join(", ")));
    }
    lines
}

/// `agent-fleet role <agent>` as text.
pub fn role(s: &State, id: &str, now: DateTime<Utc>) -> String {
    let lines = role_lines(s, id, now);
    if lines.is_empty() {
        return format!("{id}: no role declared\n");
    }
    lines.join("\n") + "\n"
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
    let waits = crate::signal::questions(s);
    let _ = writeln!(
        out,
        "\n== waiting for the principal ({})",
        waits.len() + s.asks.len()
    );
    for (id, since, text) in waits {
        let _ = writeln!(out, "{id}  {:>7}  {text}", ago(now, since));
    }
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
    fn ps_shows_role_and_lead() {
        use crate::event::{AgentState, Level};
        use crate::state::{Agent, Role};
        let now = Utc::now();
        let mut s = State::default();
        for (id, state) in [
            ("agent:h/s/root", AgentState::Working),
            ("agent:h/s/lead-a", AgentState::Idle),
            ("agent:h/s/impl-1", AgentState::Working),
            ("agent:h/s/spare", AgentState::Idle),
        ] {
            s.agents.insert(
                id.into(),
                Agent {
                    host: "h".into(),
                    kind: Some("claude".into()),
                    pane: None,
                    state,
                    since: now,
                    worked: None,
                    usage: None,
                },
            );
        }
        let role = |level, lead: Option<&str>| Role {
            level,
            lead: lead.map(str::to_string),
            domain: None,
            by: "agent:h/s/root".into(),
            since: now,
        };
        s.roles
            .insert("agent:h/s/root".into(), role(Level::RootCoordinator, None));
        s.roles.insert(
            "agent:h/s/lead-a".into(),
            role(Level::DomainLead, Some("agent:h/s/root")),
        );
        s.roles.insert(
            "agent:h/s/impl-1".into(),
            role(Level::Implementer, Some("agent:h/s/lead-a")),
        );
        let out = ps(&s, now);
        assert!(out.lines().next().unwrap().contains("ROLE"));
        let row = |id: &str| out.lines().find(|l| l.starts_with(id)).unwrap_or("?");
        assert!(row("agent:h/s/root").contains("root      "));
        assert!(row("agent:h/s/lead-a").contains("domain    root"));
        assert!(row("agent:h/s/impl-1").contains("impl      lead-a"));
        assert!(
            !row("agent:h/s/spare").contains("impl"),
            "no role, empty cells"
        );
    }

    #[test]
    fn role_view_shows_the_chain_and_reports() {
        use crate::event::{AgentState, Level};
        use crate::state::{Agent, Role};
        let now = Utc::now();
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/impl-1".into(),
            Agent {
                host: "h".into(),
                kind: None,
                pane: None,
                state: AgentState::Working,
                since: now,
                worked: None,
                usage: None,
            },
        );
        let mk = |level, lead: Option<&str>, domain: Option<&str>| Role {
            level,
            lead: lead.map(str::to_string),
            domain: domain.map(str::to_string),
            by: "agent:h/s/root".into(),
            since: now,
        };
        s.roles.insert(
            "agent:h/s/root".into(),
            mk(Level::RootCoordinator, None, None),
        );
        s.roles.insert(
            "agent:h/s/lead-a".into(),
            mk(Level::DomainLead, Some("agent:h/s/root"), Some("payments")),
        );
        s.roles.insert(
            "agent:h/s/impl-1".into(),
            mk(Level::Implementer, Some("agent:h/s/lead-a"), None),
        );
        s.roles.insert(
            "agent:h/s/impl-2".into(),
            mk(Level::Implementer, Some("agent:h/s/lead-a"), None),
        );
        let out = role(&s, "agent:h/s/lead-a", now);
        assert!(out.contains("agent:h/s/lead-a  domain-lead · payments"));
        assert!(out.contains("root (root)"));
        assert!(out.contains("impl-1 (impl), impl-2 (impl)"), "{out}");
        assert!(role(&s, "agent:h/s/nobody", now).contains("no role declared"));
    }

    #[test]
    fn ps_shows_quotas() {
        use crate::event::*;
        let now = Utc::now();
        let quota = |kind: &str, exhausted: bool| {
            Envelope::new(
                Actor {
                    kind: ActorKind::Host,
                    id: "host:h".into(),
                },
                "s",
                Known::AgentQuota(AgentQuota {
                    host: "h".into(),
                    kind: kind.into(),
                    windows: vec![
                        QuotaWindow {
                            name: "5h".into(),
                            used_pct: 45.0,
                            resets_at: Some(now + chrono::Duration::hours(3)),
                        },
                        QuotaWindow {
                            name: "week".into(),
                            used_pct: 8.0,
                            resets_at: Some(now + chrono::Duration::days(2)),
                        },
                    ],
                    plan: Some("Pro".into()),
                    exhausted,
                    read_at: now - chrono::Duration::minutes(3),
                }),
            )
            .into_event()
        };
        let s = State::fold(&[quota("claude", false), quota("devin", true)]);
        assert_eq!(s.quotas.len(), 2);
        let out = ps(&s, now);
        assert!(out.contains("QUOTA"), "{out}");
        let row = |k: &str| out.lines().find(|l| l.contains(k)).unwrap_or("?");
        assert!(row("h:claude").contains("Pro"));
        assert!(row("h:claude").contains("5h 45% (resets "));
        assert!(row("h:claude").contains("week 8%"));
        assert!(row("h:claude").contains("read 3m ago"));
        assert!(row("h:devin").ends_with("  EXHAUSTED"));
    }

    #[test]
    fn the_board_lists_what_waits_for_the_principal() {
        use crate::event::*;
        use crate::state::{Agent, Ask, Signal};
        let now = Utc::now();
        let mut s = State::default();
        s.agents.insert(
            "agent:h/s/a".into(),
            Agent {
                host: "h".into(),
                kind: Some("claude".into()),
                pane: None,
                state: AgentState::Idle,
                since: now - chrono::Duration::minutes(10),
                worked: None,
                usage: None,
            },
        );
        s.signals.insert(
            "agent:h/s/a".into(),
            Signal {
                signal: "question".into(),
                detail: Some("which branch?".into()),
                program: "claude".into(),
                since: now - chrono::Duration::minutes(5),
            },
        );
        s.asks.insert(
            "ask1".into(),
            Ask {
                project: "p".into(),
                task: None,
                by: "agent:h/s/a".into(),
                question: "ship it?".into(),
                options: vec![],
                recommendation: None,
                context: None,
                since: now - chrono::Duration::minutes(2),
            },
        );
        let out = board(&s, now);
        assert!(out.contains("waiting for the principal (2)"), "{out}");
        assert!(
            out.contains("agent:h/s/a") && out.contains("which branch?"),
            "{out}"
        );
        assert!(out.contains("ship it?"), "{out}");
        // a signal for a down agent drops off the list
        s.signals.insert(
            "agent:h/s/gone".into(),
            Signal {
                signal: "question".into(),
                detail: Some("gone".into()),
                program: "claude".into(),
                since: now,
            },
        );
        let out = board(&s, now);
        assert!(out.contains("waiting for the principal (2)"), "{out}");
    }

    #[test]
    fn ago_buckets() {
        let now = Utc::now();
        let s = |n: i64| ago(now, now - chrono::Duration::seconds(n));
        assert_eq!(
            [s(5), s(125), s(3_725), s(200_000)],
            ["5s", "2m", "1h02m", "2d"]
        );
    }

    #[test]
    fn a_past_rule_is_expired_and_a_far_one_shows_its_date() {
        use crate::event::ResourceLimit;
        let now = Utc::now();
        let near = now + chrono::Duration::hours(2);
        let far = now + chrono::Duration::hours(30);
        let mut s = State::default();
        for (name, until) in [
            ("live", Some(near)),
            ("far", Some(far)),
            ("old", Some(now - chrono::Duration::hours(1))),
        ] {
            s.limits.insert(
                format!("h:{name}"),
                ResourceLimit {
                    host: "h".into(),
                    name: name.into(),
                    resources: vec!["sim".into()],
                    max: 1,
                    until,
                },
            );
        }
        let out = resources(&s, now);
        let local = |t: DateTime<Utc>| t.with_timezone(&chrono::Local);
        assert_eq!(at(far, far), local(far).format("%H:%M").to_string());
        assert_eq!(at(now, far), local(far).format("%d %b %H:%M").to_string());
        for (name, want) in [
            ("h:live", format!(" until {}", at(now, near))),
            (
                "h:far",
                format!(" until {}", local(far).format("%d %b %H:%M")),
            ),
            ("h:old", " expired".to_string()),
        ] {
            let line = out.lines().find(|l| l.contains(name)).unwrap();
            assert!(line.contains(&want), "{name}: {line}");
        }
        let old = out.lines().find(|l| l.contains("h:old")).unwrap();
        assert!(!old.contains("until"), "{old}");
    }
}
