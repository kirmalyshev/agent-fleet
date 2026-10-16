//! `agent-fleet bench`: load on a throwaway hub in this process, through the same client the CLI uses.
//! It measures the hub and the client, not herdr or the agents; `agent-fleet stats` covers those.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

use crate::client::Client;
use crate::event::*;
use crate::hub::{Hub, router};
use crate::stats::{Dist, self_usage};
use crate::store::Store;
use crate::ui::dur;

fn health(n: usize) -> Envelope<Known> {
    Envelope::new(
        Actor {
            kind: ActorKind::Host,
            id: "host:bench".into(),
        },
        "bench",
        Known::HostHealth(HostHealth {
            host: "bench".into(),
            disk_free: n as u64,
            disk_total: 1,
            agents: 0,
            stats: None,
        }),
    )
}

fn assign(task: usize, agent: usize) -> Envelope<Known> {
    let agent = format!("agent:bench/s/{agent}");
    Envelope::new(
        Actor {
            kind: ActorKind::Agent,
            id: agent.clone(),
        },
        "bench",
        Known::TaskAssigned(TaskAssigned { agent, lead: None }),
    )
    .task(format!("task:bench/{task}"))
}

fn us(d: Duration) -> u64 {
    d.as_micros() as u64
}

fn line(o: &mut String, name: &str, d: &Dist) {
    let f = |us: u64| {
        if us < 1000 {
            format!("{us}µs")
        } else {
            dur(us / 1000)
        }
    };
    let _ = writeln!(
        o,
        "{name:<12} p50 {:<8} p90 {:<8} p99 {:<8} max {:<8} n={}",
        f(d.p50),
        f(d.p90),
        f(d.p99),
        f(d.max),
        d.n
    );
}

pub async fn run(events: usize, concurrency: usize) -> Result<String> {
    ensure!(
        events > 0 && concurrency > 0,
        "events and concurrency must be > 0"
    );
    let dir = std::env::temp_dir().join(format!("agent-fleet-bench-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir)?;
    let result = bench(&dir, events, concurrency).await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

async fn bench(dir: &std::path::Path, events: usize, concurrency: usize) -> Result<String> {
    let hub = Hub::new(Store::open(&dir.join("hub.db"))?)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, router(hub, "bench".into())).await });
    let client = Arc::new(Client::new(&base, "bench".into(), dir.join("spool.jsonl")));

    // A follower records when each event reaches it, as a host agent's replica would.
    let seen: Arc<Mutex<HashMap<String, Instant>>> = Arc::default();
    let follower = Client::new(&base, "bench".into(), dir.join("spool.jsonl"));
    let rec = seen.clone();
    tokio::spawn(async move {
        follower
            .follow(None, |e| {
                rec.lock().unwrap().insert(e.id, Instant::now());
                Ok(())
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut o = String::new();
    let sent: Arc<Mutex<Vec<(String, Instant)>>> = Arc::default();

    // One writer at a time: the latency a single CLI call sees.
    let mut seq = Vec::with_capacity(events);
    for n in 0..events {
        let e = health(n);
        let t = Instant::now();
        client.write(&e).await?;
        seq.push(us(t.elapsed()));
        sent.lock().unwrap().push((e.id, t));
    }
    line(&mut o, "post, 1 by 1", &Dist::of(seq));

    // `concurrency` writers at once: throughput and the latency under contention.
    let started = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..concurrency {
        let (client, sent) = (client.clone(), sent.clone());
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            for n in (w..events).step_by(concurrency) {
                let e = health(n);
                let t = Instant::now();
                client.write(&e).await?;
                lat.push(us(t.elapsed()));
                sent.lock().unwrap().push((e.id, t));
            }
            anyhow::Ok(lat)
        }));
    }
    let mut par = Vec::new();
    for t in tasks {
        par.extend(t.await??);
    }
    let wall = started.elapsed();
    line(
        &mut o,
        &format!("post, {concurrency} at once"),
        &Dist::of(par),
    );
    let _ = writeln!(
        o,
        "throughput   {:.0} events/s ({} events in {})",
        events as f64 / wall.as_secs_f64(),
        events,
        dur(wall.as_millis() as u64)
    );

    // Post start to the follower having it: what a host agent's replica lags behind a writer.
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.lock().unwrap().len() < 2 * events && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (lag, total) = {
        let (seen, sent) = (seen.lock().unwrap(), sent.lock().unwrap());
        let lag: Vec<u64> = sent
            .iter()
            .filter_map(|(id, t)| seen.get(id).map(|r| us(r.saturating_duration_since(*t))))
            .collect();
        (lag, sent.len())
    };
    let missed = total - lag.len();
    line(&mut o, "to follower", &Dist::of(lag));
    let _ = writeln!(o, "follower     missed {missed} of {total}");

    // Racers claim the same task at once: exactly one may win each.
    let rounds = 50;
    let mut winners = 0;
    let mut race = Vec::new();
    for task in 0..rounds {
        let mut tasks = Vec::new();
        for agent in 0..concurrency.max(2) {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                let t = Instant::now();
                let won = client.write_exclusive(&assign(task, agent)).await.is_ok();
                (won, us(t.elapsed()))
            }));
        }
        for t in tasks {
            let (won, lat) = t.await?;
            winners += usize::from(won);
            race.push(lat);
        }
    }
    line(&mut o, "claim race", &Dist::of(race));
    let _ = writeln!(
        o,
        "claims       {winners} won of {rounds} tasks, {} racers each{}",
        concurrency.max(2),
        if winners == rounds {
            ""
        } else {
            "  ← NOT EXCLUSIVE"
        }
    );

    let hub: Dist = serde_json::from_value(client.stats(3600).await?["hub"]["post_us"].clone())?;
    line(&mut o, "  in the hub", &hub);
    let (rss, cpu) = self_usage();
    let _ = writeln!(
        o,
        "process      peak rss {:.1} MB, cpu {} (hub, client and follower together)",
        rss as f64 / 1024.0,
        dur(cpu)
    );
    Ok(o)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn bench_runs_and_claims_stay_exclusive() {
        let out = super::run(20, 3).await.unwrap();
        assert!(out.contains("50 won of 50"), "{out}");
        assert!(out.contains("missed 0 of 40"), "{out}");
    }
}
