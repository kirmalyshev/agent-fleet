//! Talking to the hub: writes with a local spool for when it is unreachable, reads, and the SSE feed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::StatusCode;
use rustix::fs::{FlockOperation, flock};
use serde_json::Value;

use crate::event::{Envelope, Event, Known};
use crate::state::State;

/// Where this machine keeps its files. `FLEET_HOME` overrides the platform data directory.
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let dir = match std::env::var_os("FLEET_HOME") {
            Some(d) => PathBuf::from(d),
            None => directories::ProjectDirs::from("", "", "fleet")
                .ok_or_else(|| anyhow!("no home directory; set FLEET_HOME"))?
                .data_dir()
                .to_path_buf(),
        };
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
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
    Ok(t.trim().to_string())
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
    Accepted { duplicate: bool },
    Spooled,
}

#[derive(Debug, Default, PartialEq)]
pub struct Flushed {
    pub sent: usize,
    pub rejected: usize,
    pub left: usize,
}

/// The hub answered with a refusal (4xx). Not retried.
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
    /// Unreachable, timed out, or a 5xx: worth spooling.
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
        } else if status.is_server_error() {
            Post::Transient(format!("{status}"))
        } else {
            Post::Refused(Refused {
                status,
                message: body["error"].as_str().unwrap_or("").to_string(),
            })
        }
    }

    /// An ordinary write: sent if the hub is reachable, otherwise spooled for a later replay.
    /// Spooled events go first, so the hub sees writes in the order they were made.
    pub async fn write(&self, e: &Envelope<Known>) -> Result<Sent> {
        let raw = serde_json::to_value(e)?;
        if self.flush_spool().await?.left > 0 {
            self.spool_append(&raw)?;
            return Ok(Sent::Spooled);
        }
        match self.post(&raw).await {
            Post::Ok { duplicate } => Ok(Sent::Accepted { duplicate }),
            Post::Refused(r) => Err(r.into()),
            Post::Transient(_) => {
                self.spool_append(&raw)?;
                Ok(Sent::Spooled)
            }
        }
    }

    /// An exclusive write needs the sequencer: with the hub unreachable it fails instead of spooling.
    pub async fn write_exclusive(&self, e: &Envelope<Known>) -> Result<bool> {
        self.flush_spool().await?;
        match self.post(&serde_json::to_value(e)?).await {
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

    fn lock_spool(&self) -> Result<File> {
        let f = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.spool)?;
        flock(&f, FlockOperation::LockExclusive)?;
        Ok(f)
    }

    fn spool_append(&self, raw: &Value) -> Result<()> {
        let mut f = self.lock_spool()?;
        writeln!(f, "{}", serde_json::to_string(raw)?)?;
        Ok(())
    }

    /// Replays the spool in order. Stops at the first transient failure and keeps the rest.
    /// Replay is safe to repeat: the hub ignores ids it already has.
    pub async fn flush_spool(&self) -> Result<Flushed> {
        if std::fs::metadata(&self.spool)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
        {
            return Ok(Flushed::default());
        }
        let mut f = self.lock_spool()?;
        let mut text = String::new();
        f.rewind()?;
        f.read_to_string(&mut text)?;
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut out = Flushed::default();
        let mut i = 0;
        while i < lines.len() {
            let raw: Value = serde_json::from_str(lines[i])?;
            match self.post(&raw).await {
                Post::Ok { .. } => out.sent += 1,
                Post::Refused(r) => {
                    eprintln!("fleet: dropping spooled event {}: {r}", raw["id"]);
                    let mut rej = OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(self.spool.with_extension("rejected.jsonl"))?;
                    writeln!(rej, "{}", lines[i])?;
                    out.rejected += 1;
                }
                Post::Transient(_) => break,
            }
            i += 1;
        }
        out.left = lines.len() - i;
        f.set_len(0)?;
        for l in &lines[i..] {
            writeln!(f, "{l}")?;
        }
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
        let mut buf = String::new();
        while let Some(chunk) = bytes.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                let data: Vec<&str> = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if !data.is_empty() {
                    f(serde_json::from_str(&data.join("\n"))?)?;
                }
            }
        }
        Ok(())
    }
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
        assert_eq!(c.write(&b1).await.unwrap(), Sent::Spooled);
        assert_eq!(c.write(&b2).await.unwrap(), Sent::Spooled);
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
        assert!(err.to_string().contains("refused, not spooled"), "{err}");

        let hub = start_hub(tokio::net::TcpListener::bind(addr).await.unwrap(), &db).await;
        let f = c.flush_spool().await.unwrap();
        assert_eq!(
            f,
            Flushed {
                sent: 2,
                rejected: 0,
                left: 0
            }
        );
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

        let mut human = block(1);
        human.actor.kind = ActorKind::Human;
        assert_eq!(
            status(c.write_exclusive(&human).await),
            StatusCode::FORBIDDEN
        );

        let bad = Client::new(&base, "wrong".into(), dir.path().join("spool2.jsonl"));
        assert_eq!(
            status(bad.write_exclusive(&block(2)).await),
            StatusCode::UNAUTHORIZED
        );

        // The feed: backlog first, then live.
        let mut seen = Vec::new();
        let follow = c.follow(None, |e| {
            seen.push(e.id);
            if seen.len() == 2 {
                Err(anyhow!("enough"))
            } else {
                Ok(())
            }
        });
        let live = block(3);
        let (r, _) = tokio::join!(follow, async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.write(&live).await.unwrap()
        });
        assert!(r.is_err());
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], live.id);
        hub.abort();
    }
}
