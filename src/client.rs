//! Talking to the hub: writes with a local spool for when it is unreachable, reads, and the SSE feed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::StatusCode;
use rustix::fs::{FlockOperation, flock};
use serde_json::Value;

use crate::event::{Envelope, Event, Known};
use crate::state::State;

/// Where this machine keeps one instance's files. `FLEET_HOME` names the directory outright;
/// otherwise the platform data directory holds the default instance, and `instances/<name>`
/// every other one.
pub struct Paths {
    pub dir: PathBuf,
}

pub const DEFAULT_INSTANCE: &str = "default";
const DEFAULT_HUB: &str = "http://127.0.0.1:7710";

impl Paths {
    pub fn from_env(instance: &str) -> Result<Self> {
        if instance.is_empty()
            || !instance
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("instance names are letters, digits, - and _: {instance:?}");
        }
        let dir = match std::env::var_os("FLEET_HOME") {
            Some(d) => PathBuf::from(d),
            None => {
                let base = Self::base()?;
                match instance {
                    DEFAULT_INSTANCE => base,
                    name => base.join("instances").join(name),
                }
            }
        };
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn base() -> Result<PathBuf> {
        Ok(directories::ProjectDirs::from("", "", "fleet")
            .ok_or_else(|| anyhow!("no home directory; set FLEET_HOME"))?
            .data_dir()
            .to_path_buf())
    }

    /// The instances on this machine, by name, with the hub each one points at.
    pub fn instances() -> Result<Vec<(String, String)>> {
        let base = Self::base()?;
        let mut out = vec![(
            DEFAULT_INSTANCE.to_string(),
            Self { dir: base.clone() }.hub_url(),
        )];
        if let Ok(dir) = std::fs::read_dir(base.join("instances")) {
            let mut named: Vec<(String, String)> = dir
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_str()?.to_string();
                    Some((name, Self { dir: e.path() }.hub_url()))
                })
                .collect();
            named.sort();
            out.extend(named);
        }
        Ok(out)
    }

    /// The hub this instance uses: what `fleet hub` last wrote here, or the local default.
    pub fn hub_url(&self) -> String {
        std::fs::read_to_string(self.hub_file())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_HUB.into())
    }

    pub fn hub_file(&self) -> PathBuf {
        self.dir.join("hub")
    }
    pub fn hub_db(&self) -> PathBuf {
        self.dir.join("hub.db")
    }
    pub fn replica(&self) -> PathBuf {
        self.dir.join("replica.db")
    }
    pub fn spool(&self) -> PathBuf {
        self.dir.join("spool.jsonl")
    }
    pub fn token(&self) -> PathBuf {
        self.dir.join("token")
    }
}

pub fn read_token(path: &Path) -> Result<String> {
    let t = std::fs::read_to_string(path).with_context(|| {
        format!(
            "read token {} (start the hub here, or copy its token file)",
            path.display()
        )
    })?;
    let t = t.trim();
    if t.is_empty() {
        bail!("token file {} is empty", path.display());
    }
    Ok(t.to_string())
}

/// The hub's token file, created with 32 random bytes and mode 0600 on first start.
pub fn read_or_create_token(path: &Path) -> Result<String> {
    if !path.exists() {
        let mut bytes = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
            .write_all(hex.as_bytes())?;
        eprintln!("fleet hub: wrote a new token to {}", path.display());
    }
    read_token(path)
}

pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
    spool: PathBuf,
}

#[derive(Debug, PartialEq)]
pub enum Sent {
    Accepted {
        duplicate: bool,
    },
    /// Kept in the local spool; the string says why it was not sent.
    Spooled(String),
}

#[derive(Debug, Default, PartialEq)]
pub struct Flushed {
    pub sent: usize,
    pub rejected: usize,
    pub left: usize,
    /// Why replay stopped with events left, so callers can say it instead of guessing.
    pub stuck: Option<String>,
}

/// A non-success answer from the hub. On POST it is a verdict on the event (400, 403, 409, 413,
/// 415, 422 with an error body) and is not retried; on GET it is any non-2xx status.
#[derive(Debug)]
pub struct Refused {
    pub status: StatusCode,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hub refused ({}): {}", self.status, self.message)
    }
}

impl std::error::Error for Refused {}

enum Post {
    Ok {
        duplicate: bool,
    },
    Refused(Refused),
    /// Unreachable, timed out, or any answer that says nothing about the event (401, 404, 429,
    /// 5xx…): kept and retried, never dropped.
    Transient(String),
}

impl Client {
    pub fn new(base: &str, token: String, spool: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("http client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            http,
            spool,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn post(&self, event: &Value) -> Post {
        let res = self
            .http
            .post(self.url("/events"))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .json(event)
            .send()
            .await;
        let res = match res {
            Ok(r) => r,
            Err(e) => return Post::Transient(e.to_string()),
        };
        let status = res.status();
        let body: Value = res.json().await.unwrap_or_default();
        if status.is_success() {
            Post::Ok {
                duplicate: body["duplicate"].as_bool().unwrap_or(false),
            }
        } else if matches!(
            status,
            StatusCode::BAD_REQUEST
                | StatusCode::FORBIDDEN
                | StatusCode::CONFLICT
                | StatusCode::PAYLOAD_TOO_LARGE
                | StatusCode::UNSUPPORTED_MEDIA_TYPE
                | StatusCode::UNPROCESSABLE_ENTITY
        ) && (body["error"].is_string() || status == StatusCode::PAYLOAD_TOO_LARGE)
        {
            // A verdict needs the hub's own error body: a bare 403 from a proxy is not one.
            Post::Refused(Refused {
                status,
                message: body["error"].as_str().unwrap_or("").to_string(),
            })
        } else {
            Post::Transient(format!("{status} {}", body["error"].as_str().unwrap_or("")))
        }
    }

    /// An ordinary write: sent if the hub is reachable, otherwise spooled for a later replay.
    /// Spooled events go first, so the hub sees writes in the order they were made.
    /// The event is checked here first, so nothing the hub would refuse is ever spooled.
    pub async fn write(&self, e: &Envelope<Known>) -> Result<Sent> {
        let raw = self.check(e)?;
        if let Known::TaskAssigned(_) = e.body {
            bail!("task.assigned is exclusive: use write_exclusive, it is never spooled");
        }
        let f = self.flush_spool().await?;
        if f.left > 0 {
            self.spool_append(&raw)?;
            let why = f
                .stuck
                .unwrap_or_else(|| "another process is replaying".into());
            return Ok(Sent::Spooled(format!(
                "{} earlier writes still spooled: {why}",
                f.left
            )));
        }
        match self.post(&raw).await {
            Post::Ok { duplicate } => Ok(Sent::Accepted { duplicate }),
            Post::Refused(r) => Err(r.into()),
            Post::Transient(why) => {
                self.spool_append(&raw)?;
                Ok(Sent::Spooled(why))
            }
        }
    }

    /// An exclusive write needs the sequencer: with the hub unreachable it fails instead of spooling.
    /// It also waits for the spool: overtaking a spooled release would earn a false 409.
    pub async fn write_exclusive(&self, e: &Envelope<Known>) -> Result<bool> {
        let raw = self.check(e)?;
        let f = self.flush_spool().await?;
        if f.left > 0 {
            bail!(
                "{} earlier writes are still spooled ({}); {} waits for them",
                f.left,
                f.stuck
                    .unwrap_or_else(|| "another process is replaying".into()),
                e.body.type_name()
            );
        }
        match self.post(&raw).await {
            Post::Ok { duplicate } => Ok(duplicate),
            Post::Refused(r) => Err(r.into()),
            Post::Transient(why) => {
                bail!(
                    "hub unreachable ({why}); {} is exclusive, so it is refused, not spooled",
                    e.body.type_name()
                )
            }
        }
    }

    /// The hub's own write rules, applied locally.
    fn check(&self, e: &Envelope<Known>) -> Result<Value> {
        let raw = serde_json::to_value(e)?;
        if let Err(r) = crate::event::parse_strict(&raw) {
            bail!("invalid event, not sent: {r:?}");
        }
        Ok(raw)
    }

    /// Runs `f` holding the spool lock. Held only for local file work, never across a request.
    fn locked<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.spool.with_extension("lock"))?;
        flock(&lock, FlockOperation::LockExclusive)?;
        f()
    }

    fn spool_lines(&self) -> Result<Vec<String>> {
        match std::fs::read_to_string(&self.spool) {
            Ok(t) => Ok(t
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn spool_append(&self, raw: &Value) -> Result<()> {
        self.locked(|| {
            let mut f = OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(&self.spool)?;
            // A writer killed mid-line leaves a tail with no newline; start on a fresh line so
            // this event is not glued onto it.
            let mut line = String::new();
            let len = f.metadata()?.len();
            if len > 0 {
                let mut last = [0u8; 1];
                f.read_exact_at(&mut last, len - 1)?;
                if last[0] != b'\n' {
                    line.push('\n');
                }
            }
            line.push_str(&serde_json::to_string(raw)?);
            line.push('\n');
            f.write_all(line.as_bytes())?;
            Ok(())
        })
    }

    fn reject(&self, line: &str, why: &str) -> Result<()> {
        eprintln!("fleet: moving a spooled event to the rejected file: {why}");
        let mut rej = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.spool.with_extension("rejected.jsonl"))?;
        writeln!(rej, "{line}")?;
        Ok(())
    }

    /// Replays the spool in order. Stops at the first transient failure and keeps the rest.
    /// Replay is safe to repeat: the hub ignores ids it already has.
    pub async fn flush_spool(&self) -> Result<Flushed> {
        let pending = self.locked(|| self.spool_lines())?.len();
        if pending == 0 {
            return Ok(Flushed::default());
        }
        // One replay at a time. While another runs, report everything as left so that new
        // writes queue behind it and keep their order.
        let flusher = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.spool.with_extension("flush"))?;
        match flock(&flusher, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                return Ok(Flushed {
                    left: pending,
                    ..Flushed::default()
                });
            }
            Err(e) => return Err(e.into()),
        }
        let lines = self.locked(|| self.spool_lines())?;
        let mut out = Flushed::default();
        let mut done = 0;
        for line in &lines {
            match serde_json::from_str::<Value>(line) {
                Err(e) => {
                    self.reject(line, &format!("unreadable line: {e}"))?;
                    out.rejected += 1;
                }
                Ok(raw) => match self.post(&raw).await {
                    Post::Ok { .. } => out.sent += 1,
                    Post::Refused(r) => {
                        self.reject(line, &format!("{} {r}", raw["id"]))?;
                        out.rejected += 1;
                    }
                    Post::Transient(why) => {
                        out.stuck = Some(why);
                        break;
                    }
                },
            }
            done += 1;
        }
        // Drop the replayed head and keep whatever was appended meanwhile. Write, then rename:
        // a crash at any point leaves a whole spool, and replaying it again is harmless.
        out.left = self.locked(|| {
            let now = self.spool_lines()?;
            let tmp = self.spool.with_extension("tmp");
            let mut f = File::create(&tmp)?;
            for l in &now[done..] {
                writeln!(f, "{l}")?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, &self.spool)?;
            Ok(now.len() - done)
        })?;
        Ok(out)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let res = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            let body: Value = res.json().await.unwrap_or_default();
            return Err(Refused {
                status,
                message: body["error"].as_str().unwrap_or("").to_string(),
            }
            .into());
        }
        Ok(res)
    }

    pub async fn state(&self) -> Result<State> {
        Ok(self.get("/state").await?.json().await?)
    }

    pub async fn stats(&self, since_s: i64) -> Result<Value> {
        Ok(self
            .get(&format!("/stats?since={since_s}"))
            .await?
            .json()
            .await?)
    }

    pub async fn events_after(&self, after: Option<&str>) -> Result<Vec<Event>> {
        let path = match after {
            Some(id) => format!("/events?after={id}"),
            None => "/events".into(),
        };
        Ok(self.get(&path).await?.json().await?)
    }

    /// Follows `GET /events/stream` from `after`, calling `f` for each event until the stream ends.
    pub async fn follow(
        &self,
        after: Option<&str>,
        mut f: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let mut req = self
            .http
            .get(self.url("/events/stream"))
            .bearer_auth(&self.token);
        if let Some(id) = after {
            req = req.query(&[("after", id)]);
        }
        let res = req.send().await?.error_for_status()?;
        let mut bytes = res.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = bytes.next().await {
            buf.extend_from_slice(&chunk?);
            for data in take_frames(&mut buf)? {
                f(serde_json::from_str(&data)?)?;
            }
        }
        Ok(())
    }
}

/// Removes every complete SSE frame from `buf` and returns its `data`. Works on bytes, so a
/// UTF-8 character split across network chunks is decoded whole.
fn take_frames(buf: &mut Vec<u8>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
        let frame: Vec<u8> = buf.drain(..end + 2).collect();
        let data: Vec<&str> = std::str::from_utf8(&frame)?
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        if !data.is_empty() {
            out.push(data.join("\n"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;
    use crate::hub::{Hub, router};
    use crate::store::Store;

    async fn start_hub(
        listener: tokio::net::TcpListener,
        db: &Path,
    ) -> tokio::task::JoinHandle<()> {
        let app = router(Hub::new(Store::open(db).unwrap()).unwrap(), "tok".into());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })
    }

    fn block(n: u32) -> Envelope<Known> {
        Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::BlockerRaised(BlockerRaised {
                what: format!("b{n}"),
                owner: None,
                unblocks_when: None,
            }),
        )
        .task("task:p/1")
    }

    #[tokio::test]
    async fn stats_count_outcomes_and_render() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        start_hub(listener, &dir.path().join("hub.db")).await;
        let c = Client::new(&base, "tok".into(), dir.path().join("spool.jsonl"));
        let e = block(1);
        c.write(&e).await.unwrap();
        c.write(&e).await.unwrap();
        let v = c.stats(3600).await.unwrap();
        assert_eq!(
            v["hub"]["outcomes"],
            serde_json::json!({ "accepted": 1, "duplicate": 1 })
        );
        assert_eq!(v["hub"]["post_us"]["n"], 2);
        assert_eq!(v["log"]["by_type"]["blocker.raised"], 1);
        let lag = &v["log"]["ingest_lag_ms"];
        assert_eq!(lag["n"], 1, "the hub stamps arrival: {lag}");
        let text = crate::ui::stats(&v).unwrap();
        assert!(text.contains("blocker.raised 1"), "{text}");
        assert!(text.contains("accepted 1, duplicate 1"), "{text}");
    }

    #[test]
    fn sse_frames_survive_a_utf8_split_across_chunks() {
        let frame = "id: 1\ndata: {\"text\":\"a→б\"}\n\n".as_bytes();
        let cut = frame.iter().position(|&b| b == 0xE2).unwrap() + 1; // inside '→'
        let mut buf = frame[..cut].to_vec();
        assert!(take_frames(&mut buf).unwrap().is_empty());
        buf.extend_from_slice(&frame[cut..]);
        assert_eq!(take_frames(&mut buf).unwrap(), vec!["{\"text\":\"a→б\"}"]);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn spool_keeps_events_the_hub_did_not_judge_and_skips_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hub = start_hub(listener, &dir.path().join("hub.db")).await;
        let spool = dir.path().join("spool.jsonl");

        // A wrong token is a 401: not a verdict on the event, so it is spooled, not dropped.
        let wrong = Client::new(&base, "wrong".into(), spool.clone());
        let b1 = block(1);
        assert!(
            matches!(wrong.write(&b1).await.unwrap(), Sent::Spooled(why) if why.contains("401"))
        );
        let f = wrong.flush_spool().await.unwrap();
        assert_eq!((f.sent, f.rejected, f.left), (0, 0, 1));
        assert!(f.stuck.unwrap().contains("401"), "the reason is kept");

        // A torn tail (a writer killed mid-line, no newline) goes to the rejected file on its
        // own; the next append starts a fresh line instead of being glued onto it.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&spool)
            .unwrap()
            .write_all(b"{\"id\":")
            .unwrap();
        let mut human = block(2);
        human.actor.kind = ActorKind::Human;
        wrong
            .spool_append(&serde_json::to_value(&human).unwrap())
            .unwrap();
        let c = Client::new(&base, "tok".into(), spool.clone());
        let f = c.flush_spool().await.unwrap();
        assert_eq!((f.sent, f.rejected, f.left), (1, 2, 0));
        let rejected = std::fs::read_to_string(spool.with_extension("rejected.jsonl")).unwrap();
        assert_eq!(
            rejected.lines().count(),
            2,
            "the torn line and the 403 are kept for a human"
        );
        let ids: Vec<_> = c
            .events_after(None)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec![b1.id]);
        hub.abort();
    }

    #[tokio::test]
    async fn spool_replays_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hub.db");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // hub down
        let c = Client::new(
            &format!("http://{addr}"),
            "tok".into(),
            dir.path().join("spool.jsonl"),
        );

        let (b1, b2) = (block(1), block(2));
        assert!(matches!(c.write(&b1).await.unwrap(), Sent::Spooled(_)));
        assert!(matches!(c.write(&b2).await.unwrap(), Sent::Spooled(_)));
        let assign = Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::TaskAssigned(TaskAssigned {
                agent: "agent:h/s/a".into(),
                lead: None,
            }),
        )
        .task("task:p/1");
        let err = c.write_exclusive(&assign).await.unwrap_err();
        assert!(err.to_string().contains("still spooled"), "{err}");
        assert!(
            c.write(&assign).await.is_err(),
            "task.assigned never goes to the spool"
        );

        let hub = start_hub(tokio::net::TcpListener::bind(addr).await.unwrap(), &db).await;
        let f = c.flush_spool().await.unwrap();
        assert_eq!((f.sent, f.rejected, f.left), (2, 0, 0));
        assert_eq!(
            c.flush_spool().await.unwrap(),
            Flushed::default(),
            "spool is empty after replay"
        );

        // A second replay of the same lines (e.g. a crash before truncating) adds nothing.
        c.spool_append(&serde_json::to_value(&b1).unwrap()).unwrap();
        assert_eq!(c.flush_spool().await.unwrap().sent, 1);

        let events = c.events_after(None).await.unwrap();
        let ids: Vec<_> = events.iter().map(|e| e.id.clone()).collect();
        assert_eq!(
            ids,
            vec![b1.id.clone(), b2.id.clone()],
            "each spooled event exactly once, in order"
        );
        assert_eq!(c.state().await.unwrap().blockers.len(), 2);
        hub.abort();
    }

    #[tokio::test]
    async fn http_status_codes() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hub = start_hub(listener, &dir.path().join("hub.db")).await;
        let c = Client::new(&base, "tok".into(), dir.path().join("spool.jsonl"));
        let status = |r: Result<bool>| r.unwrap_err().downcast::<Refused>().unwrap().status;

        let assign = |who: &str| {
            Envelope::new(
                Actor {
                    kind: ActorKind::Agent,
                    id: who.into(),
                },
                "p",
                Known::TaskAssigned(TaskAssigned {
                    agent: who.into(),
                    lead: None,
                }),
            )
            .task("task:p/1")
        };
        assert!(!c.write_exclusive(&assign("agent:h/s/a")).await.unwrap());
        assert_eq!(
            status(c.write_exclusive(&assign("agent:h/s/b")).await),
            StatusCode::CONFLICT
        );

        // Invalid events are refused locally, never sent or spooled.
        let mut human = block(1);
        human.actor.kind = ActorKind::Human;
        let err = c.write(&human).await.unwrap_err().to_string();
        assert!(err.contains("Forbidden"), "{err}");
        let mut big = block(4);
        if let Known::BlockerRaised(b) = &mut big.body {
            b.what = "x".repeat(MAX_EVENT_BYTES);
        }
        assert!(c.write(&big).await.is_err());
        assert!(
            !dir.path().join("spool.jsonl").exists(),
            "nothing was spooled"
        );

        let bad = Client::new(&base, "wrong".into(), dir.path().join("spool2.jsonl"));
        let err = bad
            .write_exclusive(&block(2))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");

        // The feed: backlog first, then live. The live write waits until the backlog event has
        // arrived, so the stream is subscribed and the event must come from the broadcast.
        let mut seen = Vec::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let follow = c.follow(None, |e| {
            seen.push(e.id);
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
            if seen.len() == 2 {
                Err(anyhow!("enough"))
            } else {
                Ok(())
            }
        });
        let live = block(3);
        let (r, _) = tokio::join!(follow, async {
            rx.await.unwrap();
            c.write(&live).await.unwrap()
        });
        assert!(r.is_err());
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], live.id);
        hub.abort();
    }
}
