//! Numbers about the fleet itself, read off the log: how fast doorbells land, how many are lost,
//! how far events lag, how steady and how heavy the host agents are. Pure, like the reducer.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::{AgentState, Body, DiskEntry, Event, HostStats, Known, MachineStats};

/// A doorbell still undelivered after this long counts as lost.
pub const LOST_AFTER_S: i64 = 600;
/// How often a host agent reports `host.health`.
pub const HEALTH_EVERY_S: i64 = 60;

/// Percentiles of a sample, in its unit.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Dist {
    pub n: usize,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

impl Dist {
    pub fn of(mut v: Vec<u64>) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_unstable();
        let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        Self {
            n: v.len(),
            p50: at(0.5),
            p90: at(0.9),
            p99: at(0.99),
            max: v[v.len() - 1],
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LogStats {
    pub window_s: i64,
    pub events: usize,
    pub by_type: BTreeMap<String, usize>,
    /// Hub arrival minus the writer's `ts`, in ms. Spooled events show up as the long tail.
    pub ingest_lag_ms: Dist,
    pub doorbells: Doorbells,
    pub presence: Presence,
    pub hosts: BTreeMap<String, HostReport>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Doorbells {
    pub sent: usize,
    pub delivered: usize,
    /// Undelivered, younger than `LOST_AFTER_S`.
    pub pending: usize,
    /// Undelivered for longer than `LOST_AFTER_S`.
    pub lost: usize,
    /// Delivered, but not on the first ring.
    pub retried: usize,
    /// `msg.sent` to `msg.delivered`, ms.
    pub total_ms: Dist,
    /// `msg.sent` to the ring that worked: time spent waiting for the agent to be free, ms.
    pub wait_ms: Dist,
    /// That ring to the agent turning `working`, ms.
    pub react_ms: Dist,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Presence {
    pub agents: usize,
    pub ups: usize,
    pub downs: usize,
    pub state_changes: usize,
    /// Changes into `unknown`: herdr could not tell what the agent is doing.
    pub unknown_states: usize,
    /// States that lasted under 5 s before the next change: flapping.
    pub flaps: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HostReport {
    pub reports: usize,
    /// Reports expected over the part of the window since the host's first report.
    pub expected: usize,
    /// Longest silence between two reports (or since the last one), s.
    pub max_gap_s: i64,
    /// Times `uptime_s` went down between two reports.
    pub restarts: usize,
    pub last: Option<HostStats>,
}

/// One host's `host.health` reports: when, and the vitals if it sent them.
type Reports<'a> = Vec<(DateTime<Utc>, Option<&'a HostStats>)>;

fn ms(d: Duration) -> u64 {
    d.num_milliseconds().max(0) as u64
}

/// Stats over the events whose `ts` falls in the last `window` before `now`. `rows` are the log
/// in arrival order, each with its hub arrival time (unix ms).
pub fn compute(rows: &[(Event, Option<i64>)], now: DateTime<Utc>, window: Duration) -> LogStats {
    let from = now - window;
    let mut s = LogStats {
        window_s: window.num_seconds(),
        ..Default::default()
    };
    let mut lag = Vec::new();
    let mut sent: BTreeMap<&str, DateTime<Utc>> = BTreeMap::new();
    let (mut total, mut wait, mut react) = (Vec::new(), Vec::new(), Vec::new());
    let mut last_change: BTreeMap<&str, DateTime<Utc>> = BTreeMap::new();
    let mut agents = std::collections::BTreeSet::new();
    let mut health: BTreeMap<&str, Reports> = BTreeMap::new();
    for (e, at) in rows {
        if e.ts < from {
            continue;
        }
        s.events += 1;
        if let Some(at) = at {
            lag.push((*at - e.ts.timestamp_millis()).max(0) as u64);
        }
        let Body::Known(k) = &e.body else {
            *s.by_type.entry("unknown".into()).or_default() += 1;
            continue;
        };
        *s.by_type.entry(k.type_name().into()).or_default() += 1;
        match k {
            Known::MsgSent(_) => {
                s.doorbells.sent += 1;
                sent.insert(&e.id, e.ts);
            }
            Known::MsgDelivered(d) if d.attempts == Some(0) => {
                // Closed unrung (a stale status report): no ring happened to time.
                if let Some(re) = e.re.as_deref() {
                    sent.remove(re);
                }
            }
            Known::MsgDelivered(d) => {
                let Some(at_sent) = e.re.as_deref().and_then(|re| sent.remove(re)) else {
                    continue;
                };
                s.doorbells.delivered += 1;
                total.push(ms(e.ts - at_sent));
                if let Some(rung) = d.rung {
                    wait.push(ms(rung - at_sent));
                    react.push(ms(e.ts - rung));
                }
                s.doorbells.retried += usize::from(d.attempts.is_some_and(|a| a > 1));
            }
            Known::AgentUp(a) => {
                s.presence.ups += 1;
                agents.insert(a.agent.as_str());
                last_change.insert(&a.agent, e.ts);
            }
            Known::AgentDown(_) => s.presence.downs += 1,
            Known::AgentState(a) => {
                s.presence.state_changes += 1;
                agents.insert(a.agent.as_str());
                s.presence.unknown_states += usize::from(a.state == AgentState::Unknown);
                if let Some(prev) = last_change.insert(&a.agent, e.ts) {
                    s.presence.flaps += usize::from(e.ts - prev < Duration::seconds(5));
                }
            }
            Known::HostHealth(h) => health
                .entry(h.host.as_str())
                .or_default()
                .push((e.ts, h.stats.as_ref())),
            _ => {}
        }
    }
    for at_sent in sent.values() {
        if now - *at_sent > Duration::seconds(LOST_AFTER_S) {
            s.doorbells.lost += 1;
        } else {
            s.doorbells.pending += 1;
        }
    }
    s.ingest_lag_ms = Dist::of(lag);
    s.doorbells.total_ms = Dist::of(total);
    s.doorbells.wait_ms = Dist::of(wait);
    s.doorbells.react_ms = Dist::of(react);
    s.presence.agents = agents.len();
    for (host, reports) in health {
        let first = reports[0].0;
        let mut r = HostReport {
            reports: reports.len(),
            expected: ((now - first).num_seconds() / HEALTH_EVERY_S) as usize + 1,
            max_gap_s: (now - reports[reports.len() - 1].0).num_seconds(),
            last: reports[reports.len() - 1].1.cloned(),
            ..Default::default()
        };
        for pair in reports.windows(2) {
            r.max_gap_s = r.max_gap_s.max((pair[1].0 - pair[0].0).num_seconds());
            if let (Some(a), Some(b)) = (pair[0].1, pair[1].1) {
                r.restarts += usize::from(b.uptime_s < a.uptime_s);
            }
        }
        s.hosts.insert(host.into(), r);
    }
    s
}

/// The machine's load average, online core count and memory use. Linux reads `/proc/meminfo`;
/// macOS uses `sysctl` for total memory and `vm_stat` for the working set. A counter the
/// platform cannot read is zero rather than a failed heartbeat.
pub fn machine_usage() -> MachineStats {
    let mut loads = [0.0; 3];
    // SAFETY: getloadavg writes at most the array length it is given.
    let n = unsafe { libc::getloadavg(loads.as_mut_ptr(), loads.len() as i32) };
    if n < 0 {
        loads = [0.0; 3];
    }
    // SAFETY: sysconf only reads configuration.
    let cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as u32;
    let (memory_used, memory_total) = memory_usage();
    MachineStats {
        load1: loads[0],
        load5: loads[1],
        load15: loads[2],
        cores,
        memory_used,
        memory_total,
    }
}

#[cfg(target_os = "linux")]
fn memory_usage() -> (u64, u64) {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_meminfo(&s))
        .unwrap_or((0, 0))
}

/// `/proc/meminfo` as (used, total) bytes. `MemAvailable` is the free measure that matters;
/// without it fall back to the older free+buffers+cached approximation.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let (mut total, mut available) = (None, None);
    let (mut free, mut buffers, mut cached) = (0, 0, 0);
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next().map(|k| k.trim_end_matches(':')) else {
            continue;
        };
        let Some(kb) = parts.next().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let bytes = kb * 1024;
        match key {
            "MemTotal" => total = Some(bytes),
            "MemAvailable" => available = Some(bytes),
            "MemFree" => free = bytes,
            "Buffers" => buffers = bytes,
            "Cached" => cached = bytes,
            _ => {}
        }
    }
    let total = total?;
    let avail = available.unwrap_or(free + buffers + cached);
    Some((total.saturating_sub(avail), total))
}

#[cfg(target_os = "macos")]
fn memory_usage() -> (u64, u64) {
    let total = sysctl_u64("hw.memsize").unwrap_or(0);
    let used = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_vm_stat(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(0);
    (used, total)
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: sysctlbyname writes the value and length of the type passed here.
    unsafe {
        let mut value: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        (libc::sysctlbyname(
            c.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        ) == 0)
            .then_some(value)
    }
}

/// `vm_stat` as bytes in use: active + wired + compressed pages. Free, inactive and speculative
/// pages are cheaply reclaimable and do not count as used here.
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat(s: &str) -> Option<u64> {
    let page = s.lines().next().and_then(|l| {
        let start = l.find("page size of ")? + "page size of ".len();
        l[start..].split_whitespace().next()?.parse::<u64>().ok()
    })?;
    let mut pages = std::collections::BTreeMap::new();
    for line in s.lines().skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if let Ok(n) = value.trim().trim_end_matches('.').parse::<u64>() {
            pages.insert(key.trim().to_string(), n);
        }
    }
    let get = |k: &str| pages.get(k).copied().unwrap_or(0);
    Some(
        (get("Pages active") + get("Pages wired down") + get("Pages occupied by compressor"))
            * page,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn memory_usage() -> (u64, u64) {
    (0, 0)
}

/// The direct children in one `dust -j -o b -d 1` report, as byte entries labelled with the
/// scan root (`home`, `worktrees` or `cache`). `dust` prints sizes like `8192B` with `-o b`.
pub(crate) fn parse_dust_json(root: &str, s: &str) -> Vec<DiskEntry> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
        return Vec::new();
    };
    v.get("children")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let bytes = c
                .get("size")?
                .as_str()?
                .strip_suffix('B')?
                .parse::<u64>()
                .ok()?;
            Some(DiskEntry {
                root: root.into(),
                path: c.get("name")?.as_str()?.to_string(),
                bytes,
            })
        })
        .collect()
}

/// Keep a cached disk-use breakdown small enough that its `host.health` stays far below the
/// event-size limit, however long the paths are. The first entry is kept even when it alone fills
/// the budget: the largest directory is exactly what a low-disk refusal needs.
pub(crate) fn fit_disk_entries(mut entries: Vec<DiskEntry>, budget: usize) -> Vec<DiskEntry> {
    entries.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    let mut out = Vec::new();
    let mut used = 0;
    for e in entries {
        // root, path, punctuation and the decimal byte count; a conservative JSON-size proxy.
        let cost = e.root.len() + e.path.len() + e.bytes.to_string().len() + 40;
        if !out.is_empty() && used + cost > budget {
            break;
        }
        used += cost;
        out.push(e);
    }
    out
}

/// Peak resident memory (KiB) and user + system CPU time (ms) of this process.
pub fn self_usage() -> (u64, u64) {
    // SAFETY: getrusage only writes the struct it is given.
    let u = unsafe {
        let mut u = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        u
    };
    let cpu = |t: libc::timeval| t.tv_sec as u64 * 1000 + t.tv_usec as u64 / 1000;
    // Linux reports `ru_maxrss` in KiB, macOS in bytes.
    let rss = if cfg!(target_os = "macos") {
        u.ru_maxrss as u64 / 1024
    } else {
        u.ru_maxrss as u64
    };
    (rss, cpu(u.ru_utime) + cpu(u.ru_stime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;

    fn at(e: Envelope<Known>, ts: DateTime<Utc>) -> Event {
        let mut e = e.into_event();
        e.ts = ts;
        e
    }

    fn host(body: Known) -> Envelope<Known> {
        Envelope::new(
            Actor {
                kind: ActorKind::Host,
                id: "host:h".into(),
            },
            "s",
            body,
        )
    }

    fn health(uptime_s: u64) -> Known {
        Known::HostHealth(Box::new(HostHealth {
            host: "h".into(),
            disk_free: 1,
            disk_total: 2,
            agents: 1,
            machine: None,
            disk: None,
            repos: Vec::new(),
            stats: Some(HostStats {
                uptime_s,
                ..Default::default()
            }),
            commit: None,
            behind_since: None,
        }))
    }

    #[test]
    fn dist_percentiles() {
        let d = Dist::of((1..=100).collect());
        assert_eq!((d.n, d.p50, d.p90, d.p99, d.max), (100, 51, 90, 99, 100));
        assert_eq!(Dist::of(vec![]), Dist::default());
    }

    #[test]
    fn doorbells_latency_loss_and_hosts() {
        let t0 = Utc::now() - Duration::minutes(30);
        let s = |sec: i64| t0 + Duration::seconds(sec);
        let send = |to: &str| {
            Envelope::new(
                Actor {
                    kind: ActorKind::Agent,
                    id: "agent:h/s/a".into(),
                },
                "p",
                Known::MsgSent(MsgSent {
                    to: to.into(),
                    kind: "FYI".into(),
                    reference: "x".into(),
                    text: "t".into(),
                    closes: Vec::new(),
                }),
            )
        };
        let (m1, m2) = (at(send("agent:h/s/b"), s(0)), at(send("agent:h/s/c"), s(1)));
        let delivered = at(
            host(Known::MsgDelivered(MsgDelivered {
                to: "agent:h/s/b".into(),
                observed: AgentState::Working,
                rung: Some(s(8)),
                attempts: Some(2),
                ack: false,
            }))
            .re(&m1.id),
            s(10),
        );
        let rows = vec![
            (
                at(host(health(100)), s(0)),
                Some(s(0).timestamp_millis() + 40),
            ),
            (m1, Some(s(0).timestamp_millis() + 5)),
            (m2, None),
            (delivered, None),
            (at(host(health(160)), s(60)), None),
            (at(host(health(5)), s(400)), None), // restarted, after a 340 s silence
        ];
        let st = compute(&rows, Utc::now(), Duration::hours(1));
        let d = &st.doorbells;
        assert_eq!(
            (d.sent, d.delivered, d.pending, d.lost, d.retried),
            (2, 1, 0, 1, 1)
        );
        assert_eq!(
            (d.total_ms.p50, d.wait_ms.p50, d.react_ms.p50),
            (10_000, 8_000, 2_000)
        );
        assert_eq!((st.ingest_lag_ms.n, st.ingest_lag_ms.max), (2, 40));
        let h = &st.hosts["h"];
        assert_eq!((h.reports, h.restarts), (3, 1));
        assert!(
            h.max_gap_s >= 1400,
            "silence since the last report counts: {}",
            h.max_gap_s
        );
        assert_eq!(h.last.as_ref().unwrap().uptime_s, 5);
        let narrow = compute(&rows, Utc::now(), Duration::minutes(5));
        assert_eq!(narrow.events, 0);
    }

    #[test]
    fn flapping_and_unknown_states() {
        let t0 = Utc::now() - Duration::minutes(5);
        let change = |state, sec| {
            let e = host(Known::AgentState(AgentStateChanged {
                agent: "agent:h/s/a".into(),
                state,
            }));
            (at(e, t0 + Duration::seconds(sec)), None)
        };
        let rows = vec![
            change(AgentState::Working, 0),
            change(AgentState::Idle, 2),
            change(AgentState::Unknown, 60),
        ];
        let p = compute(&rows, Utc::now(), Duration::hours(1)).presence;
        assert_eq!(
            (p.agents, p.state_changes, p.flaps, p.unknown_states),
            (1, 3, 1, 1)
        );
    }
    #[test]
    fn meminfo_and_vm_stat_parse_memory_used() {
        let meminfo = "MemTotal:       16384 kB
MemFree:         1024 kB
MemAvailable:    4096 kB
Buffers:          512 kB
Cached:          1024 kB
";
        assert_eq!(
            parse_meminfo(meminfo),
            Some((12 * 1024 * 1024, 16 * 1024 * 1024))
        );
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free: 10.
Pages active: 20.
Pages inactive: 30.
Pages wired down: 40.
Pages occupied by compressor: 50.
";
        assert_eq!(parse_vm_stat(vm), Some(110 * 16384));
    }

    #[test]
    fn machine_usage_and_dust_json_are_readable() {
        let m = machine_usage();
        assert!(m.cores >= 1 && m.load5 >= 0.0, "{m:?}");
        let j = r#"{"size":"3B","name":"/h","children":[{"size":"2B","name":"/h/a","children":[]},{"size":"1B","name":"/h/b","children":[]}]}"#;
        let d = parse_dust_json("home", j);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].root.as_str(), d[0].bytes), ("home", 2));
        let fit = fit_disk_entries(d.clone(), 1);
        assert_eq!(
            (fit.len(), fit[0].path.as_str()),
            (1, "/h/a"),
            "the largest stays"
        );
        assert_eq!(fit_disk_entries(d, 10_000).len(), 2);
    }
}
