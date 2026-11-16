//! `agent-fleet hub`: the log, the fold, and the one sequencer for exclusive writes, over HTTP + SSE.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::{Extension, Query, Request, State as AxState};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::event::{self, ActorKind, Known, Reject};
use crate::state::State;
use crate::stats::{self, Dist};
use crate::store::{Store, UnknownCursor};

/// How many recent `POST /events` timings the hub keeps for its percentiles.
const TIMINGS: usize = 1000;

pub struct Hub {
    store: Store,
    state: State,
    tx: broadcast::Sender<(String, String)>,
    started: Instant,
    /// Lock wait + validation + append of recent posts, µs.
    post_us: VecDeque<u64>,
    /// Posts by outcome since start: `accepted`, `duplicate`, or the status code.
    outcomes: BTreeMap<&'static str, u64>,
}

#[derive(Debug, PartialEq)]
pub struct Accepted {
    pub id: String,
    pub duplicate: bool,
}

/// Folds the whole log. The hub only ever appends events it validated: one it cannot fold now
/// means the binary and the log drifted apart, and a skipped `task.assigned` would let a task be
/// granted twice.
fn fold_log(store: &Store) -> Result<State> {
    let state = State::fold(&store.all()?);
    if state.unknown > 0 {
        bail!(
            "the log holds {} events this hub cannot read; run the build that wrote them",
            state.unknown
        );
    }
    Ok(state)
}

/// `agent-fleet hub --check`: whether this binary can run the log at `db`, read-only. Returns
/// how many events it holds.
pub fn check(db: &Path) -> Result<u64> {
    let store = Store::open_readonly(db)?;
    fold_log(&store)?;
    store.len()
}

impl Hub {
    pub fn new(store: Store) -> Result<Self> {
        let state = fold_log(&store)?;
        Ok(Self {
            store,
            state,
            tx: broadcast::channel(4096).0,
            started: Instant::now(),
            post_us: VecDeque::with_capacity(TIMINGS),
            outcomes: BTreeMap::new(),
        })
    }

    fn record(&mut self, outcome: &'static str, started: Instant) {
        if self.post_us.len() == TIMINGS {
            self.post_us.pop_front();
        }
        self.post_us.push_back(started.elapsed().as_micros() as u64);
        *self.outcomes.entry(outcome).or_default() += 1;
    }

    /// The hub's own vitals plus `stats::compute` over the last `window`.
    pub fn stats(&self, window: chrono::Duration) -> anyhow::Result<Value> {
        let (max_rss_kb, cpu_ms) = stats::self_usage();
        let log = stats::compute(&self.store.all_with_arrival()?, chrono::Utc::now(), window);
        Ok(json!({
            "hub": {
                "uptime_s": self.started.elapsed().as_secs(),
                "max_rss_kb": max_rss_kb,
                "cpu_ms": cpu_ms,
                "events": self.store.len()?,
                "db_bytes": self.store.bytes()?,
                "followers": self.tx.receiver_count(),
                "post_us": Dist::of(self.post_us.iter().copied().collect()),
                "outcomes": self.outcomes,
            },
            "log": log,
        }))
    }

    /// Validates and appends one event posted with a token of `scope`. Runs under the hub lock,
    /// so the 409 check and the append are one step: this is the sequencer.
    pub fn submit_as(&mut self, raw: &Value, scope: &Scope) -> Result<Accepted, Reject> {
        let internal = |err: anyhow::Error| Reject::Internal(err.to_string());
        // A known id is acknowledged before anything else, so replaying an event this hub
        // already accepted never fails, even if the rules have tightened since.
        if let Some(id) = raw["id"].as_str()
            && self.store.contains(id).map_err(internal)?
        {
            return Ok(Accepted {
                id: id.to_string(),
                duplicate: true,
            });
        }
        let e = event::parse_strict(raw)?;
        if !scope.may_post(&e.actor) {
            return Err(Reject::Forbidden(format!(
                "this token may not post as {}",
                e.actor.id
            )));
        }
        if let (Known::TaskAssigned(_), Some(task)) = (&e.body, &e.task)
            && let Some(owner) = self.state.owner_of(task)
        {
            return Err(Reject::Conflict(format!(
                "{task} is already assigned to {owner}"
            )));
        }
        self.append(e.into_event())
    }

    fn append(&mut self, e: event::Event) -> Result<Accepted, Reject> {
        let internal = |err: anyhow::Error| Reject::Internal(err.to_string());
        self.store.append(&e).map_err(internal)?;
        self.state.apply(&e);
        let json = serde_json::to_string(&e).map_err(|err| internal(err.into()))?;
        let _ = self.tx.send((e.id.clone(), json));
        Ok(Accepted {
            id: e.id,
            duplicate: false,
        })
    }

    /// The board's human channel: the operator answers an open Inbox question. Appends the
    /// doorbell to the asker (`msg.sent`) and then `ask.answered` naming it, under one lock, so
    /// the answer is never recorded without its message. `approve` makes it an approval of the
    /// exact question text. Only an ask in the Inbox (a lead's or the coordinator's) can be answered.
    pub fn answer(&mut self, ask: &str, answer: &str, approve: bool) -> Result<Accepted, Reject> {
        if self.state.answers.contains_key(ask) {
            return Err(Reject::Conflict(format!("{ask} is already answered")));
        }
        let Some(open) = self.state.asks.get(ask).cloned() else {
            return Err(Reject::BadRequest(format!("no open ask {ask}")));
        };
        if !self.state.inbox().iter().any(|i| i.id == ask) {
            return Err(Reject::Forbidden(format!(
                "{} is not a lead; its lead answers for it",
                open.by
            )));
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(Reject::BadRequest("the answer is empty".into()));
        }
        let operator = event::Actor {
            kind: event::ActorKind::Human,
            id: "human:board".into(),
        };
        let msg = event::Envelope::new(
            operator.clone(),
            open.project.clone(),
            Known::MsgSent(event::MsgSent {
                to: open.by.clone(),
                kind: if approve { "APPROVED" } else { "ANSWER" }.into(),
                reference: ask.into(),
                text: if approve {
                    format!("{answer} (approves: {})", open.question)
                } else {
                    answer.into()
                },
            }),
        );
        let mut answered = event::Envelope::new(
            operator,
            open.project.clone(),
            Known::AskAnswered(event::AskAnswered {
                answer: answer.into(),
                approve,
                approves: approve.then(|| open.question.clone()),
                msg: msg.id.clone(),
            }),
        )
        .re(ask);
        answered.task = open.task.clone();
        // Through the same parser as every write, with the human channel open.
        for e in [msg.clone(), answered.clone()] {
            let raw = serde_json::to_value(&e).map_err(|err| Reject::Internal(err.to_string()))?;
            event::parse_human(&raw)?;
        }
        self.append(msg.into_event())?;
        self.append(answered.into_event())
    }
}

/// Which actors a bearer token may post as.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    /// The shared token file: every actor, except a host that has a token of its own (named here).
    Shared(Arc<BTreeSet<String>>),
    /// `host:<name>` in the token table: that host and the agents on it.
    Host(String),
    /// `agent:<name>` in the token table: the agents on that host.
    Agents(String),
}

impl Scope {
    fn may_post(&self, a: &event::Actor) -> bool {
        let host = a.id.strip_prefix("host:");
        let agent_host =
            a.id.strip_prefix("agent:")
                .and_then(|r| r.split('/').next());
        match (self, &a.kind) {
            (Scope::Shared(own), ActorKind::Host) => !host.is_some_and(|h| own.contains(h)),
            (Scope::Shared(_), _) => true,
            (Scope::Host(h), ActorKind::Host) => host == Some(h),
            (Scope::Host(h) | Scope::Agents(h), ActorKind::Agent) => agent_host == Some(h),
            _ => false,
        }
    }
}

fn parse_scope(s: &str) -> Result<Scope> {
    let (kind, name) = s.split_once(':').unwrap_or_default();
    if name.is_empty() || name.contains(|c: char| c == '/' || c.is_whitespace()) {
        bail!("scope {s:?}: expected host:<name> or agent:<name>");
    }
    match kind {
        "host" => Ok(Scope::Host(name.into())),
        "agent" => Ok(Scope::Agents(name.into())),
        _ => bail!("scope {s:?}: expected host:<name> or agent:<name>"),
    }
}

/// The hub's bearer tokens: the shared token file, and the scoped ones in the token table.
pub struct Tokens {
    shared: String,
    scoped: Vec<(String, Scope)>,
    /// Hosts with a `host:` token, which the shared token may no longer post as.
    own: Arc<BTreeSet<String>>,
}

impl Tokens {
    pub fn new(shared: String) -> Self {
        Self {
            shared,
            scoped: Vec::new(),
            own: Arc::default(),
        }
    }

    /// The shared token plus the table's `<scope> <token>` lines; no table is an empty one.
    pub fn load(shared: String, table: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(table) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", table.display())),
        };
        let mut t = Self::new(shared);
        let mut own = BTreeSet::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Never echo the line: it holds a token.
            let mut words = line.split_whitespace();
            let (Some(scope), Some(token), None) = (words.next(), words.next(), words.next())
            else {
                bail!("{}:{}: expected `<scope> <token>`", table.display(), n + 1);
            };
            let scope =
                parse_scope(scope).with_context(|| format!("{}:{}", table.display(), n + 1))?;
            if let Scope::Host(h) = &scope {
                own.insert(h.clone());
            }
            t.scoped.push((token.into(), scope));
        }
        t.own = Arc::new(own);
        Ok(t)
    }

    /// Compares against every token, so timing does not tell which one matched.
    fn scope_of(&self, presented: &str) -> Option<Scope> {
        let p = presented.as_bytes();
        let mut found = same(p, self.shared.as_bytes()).then(|| Scope::Shared(self.own.clone()));
        for (t, s) in &self.scoped {
            if same(p, t.as_bytes()) {
                found = Some(s.clone());
            }
        }
        found
    }
}

/// `agent-fleet mint-token`: a new token for `scope` in `out` (mode 0600, never overwritten),
/// and its line in the hub's token table. The hub reads the table when it starts.
pub fn mint(table: &Path, scope: &str, out: &Path) -> Result<()> {
    parse_scope(scope)?;
    let token = crate::client::write_new_token(out)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(table)
        .with_context(|| format!("open {}", table.display()))?;
    writeln!(f, "{scope} {token}")?;
    Ok(())
}

type Shared = Arc<Mutex<Hub>>;

/// How often the hub looks whether its binary was replaced.
const BINARY_POLL: Duration = Duration::from_secs(5);

/// A file's identity (device, inode): a rename over the path gives it a new one.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// The command that starts this same hub again: the same arguments and environment.
fn restart_command(exe: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(exe);
    c.args(std::env::args_os().skip(1));
    c
}

/// Replaces this process with the new binary: same pid, so a supervisor sees no exit, and with
/// none nothing is lost. Its sockets are close-on-exec, so the new hub binds them again. If the
/// exec fails, exit 75 and let a supervisor start it.
fn restart_in_place(exe: &Path) -> ! {
    use std::os::unix::process::CommandExt;
    let err = restart_command(exe).exec();
    eprintln!(
        "agent-fleet hub: cannot start {}: {err}; exiting",
        exe.display()
    );
    std::process::exit(crate::update::RESTART_STATUS)
}

/// Watches a file's identity. A replacement counts once the same new identity is seen on two
/// polls in a row: a copy still being written (`cargo install` over a running binary, a hand
/// `cp`) changes it more than once.
struct Settle {
    start: Option<(u64, u64)>,
    pending: Option<(u64, u64)>,
}

#[derive(Debug, PartialEq)]
enum Seen {
    Quiet,
    Settling,
    /// A new file that has stayed the same for two polls.
    Replaced((u64, u64)),
}

impl Settle {
    fn new(start: Option<(u64, u64)>) -> Self {
        Settle {
            start,
            pending: None,
        }
    }

    /// `now`: the file's identity this poll; none when it is briefly missing.
    fn step(&mut self, now: Option<(u64, u64)>) -> Seen {
        match now {
            Some(id) if Some(id) != self.start => {
                if self.pending == Some(id) {
                    Seen::Replaced(id)
                } else {
                    self.pending = Some(id);
                    Seen::Settling
                }
            }
            _ => {
                self.pending = None;
                Seen::Quiet
            }
        }
    }
}

/// Calls `changed` once when the file at `path` has been replaced by one that `verify` accepts
/// (a host's self-update renames a new binary over the installed one, and a hub still running the
/// old one would stay on the old code and protocol). Settled but refused (a binary that does not
/// run, a rollback): the hub keeps running, says why once, and looks again only when the file is
/// replaced again. A file that is briefly missing is not a change.
async fn follow_binary<V, Fut>(
    path: PathBuf,
    start: Option<(u64, u64)>,
    poll: Duration,
    verify: V,
    changed: impl FnOnce(),
) where
    V: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut settle = Settle::new(start);
    let mut refused: Option<(u64, u64)> = None;
    loop {
        tokio::time::sleep(poll).await;
        let Seen::Replaced(id) = settle.step(file_id(&path)) else {
            continue;
        };
        if refused == Some(id) {
            continue;
        }
        match verify().await {
            Ok(()) => {
                changed();
                return;
            }
            Err(why) => {
                eprintln!(
                    "agent-fleet hub: {} was replaced but is not followed: {why}",
                    path.display()
                );
                refused = Some(id);
            }
        }
    }
}

/// How long the new binary may take to read the whole log.
const CHECK_LIMIT: Duration = Duration::from_secs(120);

/// The real check before the exec: the new binary runs and names itself, it is the same commit
/// as this one or built on it (not a rollback; unknown refuses), and it can run this hub's log
/// (`hub --check`, read-only, with this hub's own arguments): replacing the only hub with a
/// binary that cannot start would leave no hub.
async fn followable(exe: &Path) -> Result<(), String> {
    let says = crate::update::binary_says(exe)
        .await
        .map_err(|e| format!("{e:#}"))?;
    crate::update::can_follow(
        Path::new(crate::update::CLONE),
        crate::update::COMMIT,
        &says,
    )
    .await?;
    let mut check = tokio::process::Command::from(restart_command(exe));
    check.arg("--check");
    crate::update::probe(check, CHECK_LIMIT)
        .await
        .map_err(|e| format!("it cannot run this hub's log: {e}"))
}

/// Serves the hub on every address in `listen`. Once bound, writes one URL to `hub_file`, where
/// this instance's clients look: the loopback one when given, so a client on this host does not
/// depend on the network the other addresses ride (3 Nov: a Tailscale blip cut the hub off from
/// its own host).
pub async fn serve(
    listen: &[SocketAddr],
    db: &Path,
    hub_file: &Path,
    tokens: Tokens,
) -> Result<()> {
    if let Some(a) = listen.iter().find(|a| a.ip().is_unspecified()) {
        bail!("refusing to listen on {a}: bind a specific address (default 127.0.0.1)");
    }
    // Taken before the log loads, which can take a while: a replacement meanwhile is noticed.
    let exe = crate::update::this_exe();
    let started_as = exe.as_ref().ok().and_then(|e| file_id(e));
    let hub = Hub::new(Store::open(db)?)?;
    // With a loopback address to fall back on, an address that will not bind yet (a Tailscale IP
    // before Tailscale is up, 3 Nov) is retried in the background instead of taking the hub down.
    let has_loopback = listen.iter().any(|a| a.ip().is_loopback());
    let mut listeners = Vec::new();
    let mut late = Vec::new();
    for a in listen {
        match tokio::net::TcpListener::bind(a).await {
            Ok(l) => listeners.push(l),
            Err(e)
                if has_loopback
                    && !a.ip().is_loopback()
                    && e.kind() == std::io::ErrorKind::AddrNotAvailable =>
            {
                eprintln!("agent-fleet hub: bind {a}: {e}; retrying every {LATE_BIND_RETRY:?}");
                late.push(*a);
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("bind {a}; if another instance's hub holds it, pass --listen")
                });
            }
        }
    }
    let bound = listeners
        .iter()
        .map(|l| l.local_addr())
        .collect::<std::io::Result<Vec<_>>>()?;
    let shown: Vec<String> = bound.iter().map(|a| format!("http://{a}")).collect();
    eprintln!(
        "agent-fleet hub: {} events in {}, listening on {}",
        hub.store.len()?,
        db.display(),
        shown.join(", ")
    );
    std::fs::write(hub_file, format!("http://{}\n", hub_file_addr(&bound)?))?;
    let shared: Shared = Arc::new(Mutex::new(hub));
    let app = router_shared(shared.clone(), tokens);
    match exe {
        Ok(exe) => {
            let at = exe.clone();
            let check = exe.clone();
            tokio::spawn(follow_binary(
                exe,
                started_as,
                BINARY_POLL,
                move || {
                    let exe = check.clone();
                    async move { followable(&exe).await }
                },
                move || {
                    // Between requests: one that writes twice (an answer) is not cut in half.
                    let _held = shared.lock().unwrap_or_else(|e| e.into_inner());
                    eprintln!("agent-fleet hub: binary replaced; starting the new one");
                    restart_in_place(&at)
                },
            ));
        }
        Err(e) => eprintln!("agent-fleet hub: cannot follow its binary: {e:#}"),
    }
    let mut set = tokio::task::JoinSet::new();
    for l in listeners {
        let app = app.clone();
        set.spawn(async move { axum::serve(l, app).await });
    }
    for a in late {
        let app = app.clone();
        set.spawn(async move {
            loop {
                tokio::time::sleep(LATE_BIND_RETRY).await;
                if let Ok(l) = tokio::net::TcpListener::bind(a).await {
                    eprintln!("agent-fleet hub: now also listening on http://{a}");
                    return axum::serve(l, app).await;
                }
            }
        });
    }
    if let Some(r) = set.join_next().await {
        r??;
    }
    Ok(())
}

const LATE_BIND_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// The address the hub file names: the first loopback one, else the first.
fn hub_file_addr(listen: &[SocketAddr]) -> Result<SocketAddr> {
    listen
        .iter()
        .find(|a| a.ip().is_loopback())
        .or_else(|| listen.first())
        .copied()
        .context("--listen: no address")
}

/// A router whose one token may post as anyone the hub accepts.
pub fn router(hub: Hub, token: String) -> Router {
    scoped_router(hub, Tokens::new(token))
}

pub fn scoped_router(hub: Hub, tokens: Tokens) -> Router {
    router_shared(Arc::new(Mutex::new(hub)), tokens)
}

fn router_shared(hub: Shared, tokens: Tokens) -> Router {
    let api = Router::new()
        .route("/events", post(post_event).get(get_events))
        .route("/events/stream", get(stream))
        .route("/state", get(get_state))
        .route("/stats", get(get_stats))
        .route("/board", get(get_board))
        .route("/board/answer", post(post_answer))
        .route("/schema", get(|| async { Json(event::api_schema()) }))
        .layer(middleware::from_fn_with_state(Arc::new(tokens), auth));
    // The board page holds no data: it asks for the token and reads `/board` with it.
    Router::new()
        .route("/", get(board_page))
        .merge(api)
        .with_state(hub)
}

async fn board_page() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("board.html"),
    )
        .into_response()
}

/// What the board shows: the state, `doctor`'s findings, and which worktree each agent is in.
async fn get_board(AxState(hub): AxState<Shared>) -> Response {
    let hub = hub.lock().unwrap();
    let (s, now) = (&hub.state, chrono::Utc::now());
    let worktree_of: BTreeMap<&String, &String> = s
        .agents
        .iter()
        .filter_map(|(id, a)| Some((id, crate::doctor::worktree_of(s, a)?.0)))
        .collect();
    Json(json!({
        "now": now,
        "state": s,
        "findings": crate::doctor::findings(s, now),
        "worktree_of": worktree_of,
        "inbox": s.inbox(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AnswerBody {
    ask: String,
    answer: String,
    #[serde(default)]
    approve: bool,
}

/// The one way a human writes: the holder of the shared token answers an Inbox question from
/// the board. A scoped token is a host's or an agent's, never the operator's.
async fn post_answer(
    AxState(hub): AxState<Shared>,
    Extension(scope): Extension<Scope>,
    Json(b): Json<AnswerBody>,
) -> Response {
    if !matches!(scope, Scope::Shared(_)) {
        return error(
            StatusCode::FORBIDDEN,
            "only the shared token answers the board",
        );
    }
    let result = hub.lock().unwrap().answer(&b.ask, &b.answer, b.approve);
    match result {
        Ok(a) => Json(json!({ "id": a.id })).into_response(),
        Err(Reject::BadRequest(m)) => error(StatusCode::BAD_REQUEST, &m),
        Err(Reject::Forbidden(m)) => error(StatusCode::FORBIDDEN, &m),
        Err(Reject::Conflict(m)) => error(StatusCode::CONFLICT, &m),
        Err(Reject::Internal(m)) => error(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

async fn auth(AxState(tokens): AxState<Arc<Tokens>>, mut req: Request, next: Next) -> Response {
    let scope = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| tokens.scope_of(t));
    let Some(scope) = scope else {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong bearer token");
    };
    req.extensions_mut().insert(scope);
    next.run(req).await
}

/// Constant-time comparison, so response timing does not leak how much of a guess was right.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// 410 for a cursor this log does not have (the follower should start over), 500 otherwise.
fn read_error(e: anyhow::Error) -> Response {
    let status = if e.is::<UnknownCursor>() {
        StatusCode::GONE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    error(status, &e.to_string())
}

async fn post_event(
    AxState(hub): AxState<Shared>,
    Extension(scope): Extension<Scope>,
    Json(raw): Json<Value>,
) -> Response {
    let started = Instant::now();
    let mut h = hub.lock().unwrap();
    let result = h.submit_as(&raw, &scope);
    let outcome = match &result {
        Ok(a) if a.duplicate => "duplicate",
        Ok(_) => "accepted",
        Err(Reject::BadRequest(_)) => "400",
        Err(Reject::Forbidden(_)) => "403",
        Err(Reject::Conflict(_)) => "409",
        Err(Reject::Internal(_)) => "500",
    };
    h.record(outcome, started);
    drop(h);
    match result {
        Ok(a) => Json(json!({ "id": a.id, "duplicate": a.duplicate })).into_response(),
        Err(Reject::BadRequest(m)) => error(StatusCode::BAD_REQUEST, &m),
        Err(Reject::Forbidden(m)) => error(StatusCode::FORBIDDEN, &m),
        Err(Reject::Conflict(m)) => error(StatusCode::CONFLICT, &m),
        Err(Reject::Internal(m)) => error(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

#[derive(Deserialize)]
struct Cursor {
    after: Option<String>,
    limit: Option<usize>,
}

async fn get_events(AxState(hub): AxState<Shared>, Query(q): Query<Cursor>) -> Response {
    let events = hub
        .lock()
        .unwrap()
        .store
        .after(q.after.as_deref(), q.limit.unwrap_or(1000));
    match events {
        Ok(v) => Json(v).into_response(),
        Err(e) => read_error(e),
    }
}

async fn get_state(AxState(hub): AxState<Shared>) -> Response {
    Json(hub.lock().unwrap().state.clone()).into_response()
}

#[derive(Deserialize)]
struct Window {
    /// Seconds back from now; a day by default.
    since: Option<i64>,
}

async fn get_stats(AxState(hub): AxState<Shared>, Query(q): Query<Window>) -> Response {
    let window = chrono::Duration::seconds(q.since.unwrap_or(86_400));
    match hub.lock().unwrap().stats(window) {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The backlog after the cursor, then live events. Subscribing and reading the backlog happen
/// under the hub lock, so nothing is missed or sent twice between the two.
async fn stream(AxState(hub): AxState<Shared>, Query(q): Query<Cursor>) -> Response {
    let (backlog, rx) = {
        let hub = hub.lock().unwrap();
        let rx = hub.tx.subscribe();
        match hub.store.after(q.after.as_deref(), usize::MAX >> 1) {
            Ok(backlog) => (backlog, rx),
            Err(e) => return read_error(e),
        }
    };
    let backlog = backlog
        .into_iter()
        .map(|e| (e.id.clone(), serde_json::to_string(&e).unwrap_or_default()));
    // A lagging subscriber ends its stream instead of skipping events; it reconnects with its cursor.
    let live = BroadcastStream::new(rx)
        .take_while(|r| std::future::ready(r.is_ok()))
        .filter_map(|r| std::future::ready(r.ok()));
    let events = tokio_stream::iter(backlog)
        .chain(live)
        .map(|(id, json)| Ok::<_, Infallible>(SseEvent::default().id(id).data(json)));
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_replacement_counts_only_once_the_same_new_file_stays_for_two_polls() {
        let (old, a, b) = (Some((1, 1)), Some((1, 2)), Some((1, 3)));
        let mut w = Settle::new(old);
        assert_eq!(w.step(old), Seen::Quiet);
        assert_eq!(w.step(a), Seen::Settling, "first sight of a new file");
        assert_eq!(
            w.step(b),
            Seen::Settling,
            "it changed again: still being written"
        );
        assert_eq!(w.step(b), Seen::Replaced((1, 3)));
        // Missing or back to the old one in between: start over.
        let mut w = Settle::new(old);
        assert_eq!(w.step(a), Seen::Settling);
        assert_eq!(w.step(None), Seen::Quiet);
        assert_eq!(w.step(a), Seen::Settling);
        assert_eq!(w.step(old), Seen::Quiet);
        assert_eq!(w.step(a), Seen::Settling);
    }

    #[tokio::test]
    async fn a_replacement_that_is_refused_is_kept_out_once_and_a_later_good_one_is_followed() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("agent-fleet");
        std::fs::write(&exe, "old").unwrap();
        std::fs::hard_link(&exe, dir.path().join("held")).unwrap();
        let (changed, asked) = (
            std::sync::Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(AtomicUsize::new(0)),
        );
        let (flag, count) = (changed.clone(), asked.clone());
        let task = tokio::spawn(follow_binary(
            exe.clone(),
            file_id(&exe),
            Duration::from_millis(10),
            move || {
                let n = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        Err("half written".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
            move || flag.store(true, Ordering::SeqCst),
        ));
        let put = |name: &str, body: &str| {
            let new = dir.path().join(name);
            std::fs::write(&new, body).unwrap();
            std::fs::rename(&new, &exe).unwrap();
        };
        put(".a", "a");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!changed.load(Ordering::SeqCst), "refused: keeps running");
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "asked once, not every poll"
        );
        std::fs::hard_link(&exe, dir.path().join("held-a")).unwrap();
        put(".b", "b");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("followed the next one")
            .unwrap();
        assert!(changed.load(Ordering::SeqCst));
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_hub_restarts_as_the_same_command() {
        let c = restart_command(Path::new("/opt/agent-fleet"));
        assert_eq!(c.get_program(), "/opt/agent-fleet");
        let args: Vec<_> = c.get_args().collect();
        let mine: Vec<_> = std::env::args_os().skip(1).collect();
        assert_eq!(args, mine.iter().map(|a| a.as_os_str()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn the_hub_follows_its_binary_and_ignores_a_file_that_is_briefly_missing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("agent-fleet");
        std::fs::write(&exe, "old").unwrap();
        // Keep the old file's inode taken, as the running hub does: a filesystem reuses a freed
        // one for the next file.
        std::fs::hard_link(&exe, dir.path().join("held")).unwrap();
        let changed = std::sync::Arc::new(AtomicBool::new(false));
        let flag = changed.clone();
        let task = tokio::spawn(follow_binary(
            exe.clone(),
            file_id(&exe),
            Duration::from_millis(10),
            || async { Ok(()) },
            move || flag.store(true, Ordering::SeqCst),
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!changed.load(Ordering::SeqCst), "untouched: no change");
        std::fs::remove_file(&exe).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!changed.load(Ordering::SeqCst), "missing is not replaced");
        // A self-update writes the new binary beside it and renames it over.
        let new = dir.path().join(".new");
        std::fs::write(&new, "new").unwrap();
        std::fs::rename(&new, &exe).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("noticed")
            .unwrap();
        assert!(changed.load(Ordering::SeqCst));
    }

    use super::*;
    use crate::event::*;

    fn hub() -> Hub {
        Hub::new(Store::memory().unwrap()).unwrap()
    }

    impl Hub {
        /// Posted with the shared token and no host holding its own.
        fn submit(&mut self, raw: &Value) -> Result<Accepted, Reject> {
            self.submit_as(raw, &Scope::Shared(Arc::default()))
        }
    }

    /// A note from `actor` (`host:<h>` or `agent:<h>/…`): both kinds may write `msg.sent`.
    fn note(actor: &str) -> Value {
        let kind = if actor.starts_with("host:") {
            ActorKind::Host
        } else {
            ActorKind::Agent
        };
        let e = Envelope::new(
            Actor {
                kind,
                id: actor.into(),
            },
            "p",
            Known::MsgSent(MsgSent {
                to: "agent:a/s/x".into(),
                kind: "NOTE".into(),
                reference: "r".into(),
                text: "hi".into(),
            }),
        );
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn a_token_posts_only_the_actors_its_scope_names() {
        let mut h = hub();
        let host_a = Scope::Host("a".into());
        let agents_a = Scope::Agents("a".into());
        let shared = Scope::Shared(Arc::new(BTreeSet::from(["a".to_string()])));
        for (scope, actor, ok) in [
            (&host_a, "host:a", true),
            (&host_a, "agent:a/s/x", true),
            (&host_a, "host:b", false),
            (&host_a, "host:ab", false),
            (&host_a, "agent:b/s/x", false),
            (&agents_a, "agent:a/s/x", true),
            (&agents_a, "host:a", false),
            (&agents_a, "agent:b/s/x", false),
            // The shared token keeps every host that has no token of its own.
            (&shared, "host:b", true),
            (&shared, "agent:a/s/x", true),
            (&shared, "host:a", false),
        ] {
            let got = h.submit_as(&note(actor), scope);
            assert_eq!(got.is_ok(), ok, "{scope:?} posting as {actor}: {got:?}");
            if !ok {
                assert!(matches!(got, Err(Reject::Forbidden(_))), "{got:?}");
            }
        }
    }

    #[test]
    fn a_bad_token_table_is_refused_without_echoing_it() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("hub-tokens");
        for bad in [
            "host:a",
            "human:x s3cret",
            "host: s3cret",
            "host:a/b s3cret",
            "host:a s3cret x",
        ] {
            std::fs::write(&table, bad).unwrap();
            let err = format!("{:#}", Tokens::load("t".into(), &table).err().unwrap());
            assert!(!err.contains("s3cret"), "{err}");
        }
        assert!(Tokens::load("t".into(), &dir.path().join("none")).is_ok());
        assert!(mint(&table, "operator", &dir.path().join("x")).is_err());
        assert!(!dir.path().join("x").exists(), "no token for a bad scope");
    }

    #[tokio::test]
    async fn minted_tokens_bind_the_actor_over_http() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (table, host_a, agent_a) = (
            dir.path().join("hub-tokens"),
            dir.path().join("host-a"),
            dir.path().join("agent-a"),
        );
        mint(&table, "host:a", &host_a).unwrap();
        mint(&table, "agent:a", &agent_a).unwrap();
        assert!(mint(&table, "host:a", &host_a).is_err(), "never overwrites");
        for f in [&table, &host_a] {
            let mode = std::fs::metadata(f).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", f.display());
        }
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        let (host_a, agent_a) = (read(&host_a), read(&agent_a));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = scoped_router(hub(), Tokens::load("shared".into(), &table).unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let post = |token: &str, actor: &str| {
            http.post(format!("{base}/events"))
                .bearer_auth(token)
                .json(&note(actor))
                .send()
        };
        for (token, actor, status) in [
            (host_a.as_str(), "host:a", 200),
            (host_a.as_str(), "agent:a/s/x", 200),
            (host_a.as_str(), "host:b", 403),
            (agent_a.as_str(), "host:a", 403),
            (agent_a.as_str(), "agent:a/s/x", 200),
            ("shared", "host:a", 403),
            ("shared", "host:b", 200),
            ("shared", "agent:b/s/x", 200),
            ("wrong", "agent:a/s/x", 401),
        ] {
            let got = post(token, actor).await.unwrap().status();
            assert_eq!(got, status, "posting as {actor}");
        }
        // Reads stay open to every valid token; the board's human channel does not.
        let state = http.get(format!("{base}/state")).bearer_auth(&agent_a);
        assert_eq!(state.send().await.unwrap().status(), 200);
        let answer = http
            .post(format!("{base}/board/answer"))
            .bearer_auth(&host_a)
            .json(&json!({ "ask": "x", "answer": "y" }));
        assert_eq!(answer.send().await.unwrap().status(), 403);
    }

    fn assign(agent: &str) -> Value {
        let e = Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: agent.into(),
            },
            "p",
            Known::TaskAssigned(TaskAssigned {
                agent: agent.into(),
                lead: None,
            }),
        )
        .task("task:p/1");
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn second_assignment_is_a_conflict() {
        let mut h = hub();
        assert!(!h.submit(&assign("agent:h/s/a")).unwrap().duplicate);
        let err = h.submit(&assign("agent:h/s/b")).unwrap_err();
        assert!(
            matches!(err, Reject::Conflict(ref m) if m.contains("agent:h/s/a")),
            "{err:?}"
        );
        assert!(
            matches!(h.submit(&assign("agent:h/s/a")), Err(Reject::Conflict(_))),
            "even for the owner"
        );
        assert_eq!(h.store.len().unwrap(), 1);
    }

    #[test]
    fn replaying_an_accepted_assignment_is_not_a_conflict() {
        let mut h = hub();
        let a = assign("agent:h/s/a");
        h.submit(&a).unwrap();
        assert!(h.submit(&a).unwrap().duplicate);
        assert_eq!(h.store.len().unwrap(), 1);
    }

    #[test]
    fn released_or_finished_tasks_can_be_assigned_again() {
        let mut h = hub();
        h.submit(&assign("agent:h/s/a")).unwrap();
        let release = Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::TaskUnassigned(TaskUnassigned { reason: None }),
        )
        .task("task:p/1");
        h.submit(&serde_json::to_value(release).unwrap()).unwrap();
        h.submit(&assign("agent:h/s/b")).unwrap();
        assert_eq!(h.state.owner_of("task:p/1"), Some("agent:h/s/b"));
    }

    #[test]
    fn a_reclaimed_finished_task_is_exclusive_again() {
        let mut h = hub();
        h.submit(&assign("agent:h/s/a")).unwrap();
        let done = Envelope::new(
            Actor {
                kind: ActorKind::Agent,
                id: "agent:h/s/a".into(),
            },
            "p",
            Known::TaskState(TaskStateChanged {
                state: TaskState::Completed,
            }),
        )
        .task("task:p/1");
        h.submit(&serde_json::to_value(done).unwrap()).unwrap();
        h.submit(&assign("agent:h/s/b")).unwrap();
        assert!(matches!(
            h.submit(&assign("agent:h/s/c")),
            Err(Reject::Conflict(_))
        ));
    }

    #[test]
    fn a_known_id_is_a_duplicate_before_any_validation() {
        let mut h = hub();
        let mut a = assign("agent:h/s/a");
        h.submit(&a).unwrap();
        a["v"] = 2.into(); // would now be a 400
        assert!(h.submit(&a).unwrap().duplicate);
    }

    #[test]
    fn refuses_to_start_on_a_log_it_cannot_read() {
        let store = Store::memory().unwrap();
        let mut v = assign("agent:h/s/a");
        v["type"] = "task.assigned.v9".into();
        store.append(&serde_json::from_value(v).unwrap()).unwrap();
        let err = Hub::new(store).err().unwrap();
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    #[tokio::test]
    async fn the_board_page_is_open_and_its_data_is_not() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(hub(), "tok".into());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let page = http.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("agent-fleet board"));
        let anon = http.get(format!("{base}/board")).send().await.unwrap();
        assert_eq!(anon.status(), 401);
        let board: Value = http
            .get(format!("{base}/board"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(board["state"]["agents"].is_object() && board["findings"].is_array());
    }

    fn agent(id: &str) -> Actor {
        Actor {
            kind: ActorKind::Agent,
            id: id.into(),
        }
    }

    /// Declares `who` at `level`, then has it raise a 3-option ask; returns the ask's id.
    fn lead_asks(h: &mut Hub, who: &str, level: Level) -> String {
        let role = Envelope::new(
            agent(who),
            "p",
            Known::AgentRole(AgentRole {
                agent: who.into(),
                level: Some(level),
                lead: None,
                domain: None,
            }),
        );
        h.submit(&serde_json::to_value(role).unwrap()).unwrap();
        let ask = Envelope::new(
            agent(who),
            "p",
            Known::AskRaised(AskRaised {
                question: "which one?".into(),
                options: vec!["a".into(), "b".into(), "c".into()],
                recommendation: None,
                context: None,
            }),
        )
        .task("task:p/1");
        let id = ask.id.clone();
        h.submit(&serde_json::to_value(ask).unwrap()).unwrap();
        id
    }

    #[test]
    fn a_board_answer_reaches_the_lead_and_the_ack_flips_it() {
        let mut h = hub();
        let lead = "agent:h/s/lead";
        let ask = lead_asks(&mut h, lead, Level::DomainLead);
        assert_eq!(h.state.inbox().len(), 1);

        h.answer(&ask, "b", false).unwrap();
        assert!(h.state.asks.is_empty());
        let (mid, m) = h.state.msgs.iter().next().expect("a doorbell is queued");
        assert_eq!(
            (m.to.as_str(), m.kind.as_str(), m.text.as_str()),
            (lead, "ANSWER", "b")
        );
        assert_eq!(m.reference, ask);
        assert_eq!(h.state.answers[&ask].msg, *mid);
        assert_eq!(h.state.answers[&ask].acked, None, "sent");

        let ack = Envelope::new(
            Actor {
                kind: ActorKind::Host,
                id: "host:h".into(),
            },
            "p",
            Known::MsgDelivered(MsgDelivered {
                to: lead.into(),
                observed: AgentState::Working,
                rung: None,
                attempts: None,
            }),
        )
        .re(mid.clone());
        h.submit(&serde_json::to_value(ack).unwrap()).unwrap();
        assert!(h.state.answers[&ask].acked.is_some(), "acked");
        // Both board events are in the log, with a human actor on the answer.
        let log = h.store.after(None, 100).unwrap();
        assert!(log.iter().any(|e| e.actor.kind == ActorKind::Human
            && matches!(&e.body, Body::Known(Known::AskAnswered(_)))));
    }

    #[test]
    fn approve_is_explicit_and_records_what_it_applies_to() {
        let mut h = hub();
        let ask = lead_asks(&mut h, "agent:h/s/lead", Level::RootCoordinator);
        h.answer(&ask, "yes", true).unwrap();
        let a = &h.state.answers[&ask];
        assert!(a.approve);
        assert_eq!(a.approves.as_deref(), Some("which one?"));
        let m = h.state.msgs.values().next().unwrap();
        assert_eq!(m.kind, "APPROVED");
    }

    #[test]
    fn the_board_does_not_answer_for_implementers_or_twice() {
        let mut h = hub();
        let ask = lead_asks(&mut h, "agent:h/s/impl", Level::Implementer);
        assert!(matches!(
            h.answer(&ask, "a", false),
            Err(Reject::Forbidden(_))
        ));
        assert!(h.state.msgs.is_empty() && h.state.answers.is_empty());

        let ask = lead_asks(&mut h, "agent:h/s/lead", Level::ImplementationLead);
        assert!(matches!(
            h.answer(&ask, "  ", false),
            Err(Reject::BadRequest(_))
        ));
        assert!(matches!(
            h.answer("nope", "a", false),
            Err(Reject::BadRequest(_))
        ));
        h.answer(&ask, "a", false).unwrap();
        assert!(matches!(
            h.answer(&ask, "b", false),
            Err(Reject::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn answering_needs_the_token_and_the_board_shows_the_inbox() {
        let mut h = hub();
        let ask = lead_asks(&mut h, "agent:h/s/lead", Level::DomainLead);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(h, "tok".into());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let body = json!({ "ask": ask, "answer": "c" });
        let anon = http
            .post(format!("{base}/board/answer"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(anon.status(), 401);
        let board =
            |http: &reqwest::Client| http.get(format!("{base}/board")).bearer_auth("tok").send();
        let b: Value = board(&http).await.unwrap().json().await.unwrap();
        assert_eq!(b["inbox"][0]["ask"]["options"], json!(["a", "b", "c"]));
        let ok = http
            .post(format!("{base}/board/answer"))
            .bearer_auth("tok")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        let b: Value = board(&http).await.unwrap().json().await.unwrap();
        assert_eq!(b["inbox"][0]["answer"]["answer"], "c");
        let again = http
            .post(format!("{base}/board/answer"))
            .bearer_auth("tok")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 409);
    }

    #[test]
    fn token_check() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"", b"abc"));
    }

    #[test]
    fn human_actor_is_forbidden() {
        let mut h = hub();
        let mut v = assign("agent:h/s/a");
        v["actor"] = json!({ "kind": "human", "id": "human:x" });
        assert!(matches!(h.submit(&v), Err(Reject::Forbidden(_))));
        v["actor"]["kind"] = "hub".into();
        assert!(matches!(h.submit(&v), Err(Reject::Forbidden(_))));
        assert_eq!(h.store.len().unwrap(), 0);
    }

    #[test]
    fn hub_state_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hub.db");
        Hub::new(Store::open(&db).unwrap())
            .unwrap()
            .submit(&assign("agent:h/s/a"))
            .unwrap();
        let mut h = Hub::new(Store::open(&db).unwrap()).unwrap();
        assert!(matches!(
            h.submit(&assign("agent:h/s/b")),
            Err(Reject::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn an_unbindable_address_beside_loopback_does_not_stop_the_hub() {
        let dir = tempfile::tempdir().unwrap();
        let hub_file = dir.path().join("hub");
        // 192.0.2.1 is TEST-NET-1: never assigned to this host, so its bind fails.
        let addrs: Vec<SocketAddr> = ["127.0.0.1:0", "192.0.2.1:7710"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let (db, hf) = (dir.path().join("h.db"), hub_file.clone());
        let task =
            tokio::spawn(async move { serve(&addrs, &db, &hf, Tokens::new("t".into())).await });
        for _ in 0..100 {
            if std::fs::read_to_string(&hub_file).is_ok_and(|s| s.ends_with('\n')) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let url = std::fs::read_to_string(&hub_file).unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(!task.is_finished(), "hub exited: {:?}", task.await);
        task.abort();
    }

    #[test]
    fn hub_file_names_the_loopback_address() {
        let tail: SocketAddr = "100.94.83.110:7710".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:7710".parse().unwrap();
        assert_eq!(hub_file_addr(&[tail, lo]).unwrap(), lo);
        assert_eq!(hub_file_addr(&[tail]).unwrap(), tail);
        assert!(hub_file_addr(&[]).is_err());
    }

    #[tokio::test]
    async fn refuses_unspecified_listen_address() {
        let dir = tempfile::tempdir().unwrap();
        for addrs in [
            vec!["0.0.0.0:7710"],
            vec!["[::]:7710"],
            vec!["127.0.0.1:0", "0.0.0.0:7711"],
        ] {
            let addrs: Vec<SocketAddr> = addrs.iter().map(|a| a.parse().unwrap()).collect();
            let err = serve(
                &addrs,
                &dir.path().join("h.db"),
                &dir.path().join("hub"),
                Tokens::new("t".into()),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("refusing"), "{err}");
        }
    }
}
