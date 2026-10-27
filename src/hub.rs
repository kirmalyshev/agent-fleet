//! `agent-fleet hub`: the log, the fold, and the one sequencer for exclusive writes, over HTTP + SSE.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use axum::extract::{Query, Request, State as AxState};
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

use crate::event::{self, Known, Reject};
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

impl Hub {
    pub fn new(store: Store) -> Result<Self> {
        let state = State::fold(&store.all()?);
        // The hub only ever appends events it validated. One it cannot fold now means the binary
        // and the log drifted apart, and a skipped `task.assigned` would let a task be granted twice.
        if state.unknown > 0 {
            bail!(
                "the log holds {} events this hub cannot read; run the build that wrote them",
                state.unknown
            );
        }
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

    /// Validates and appends one event. Runs under the hub lock, so the 409 check and the append
    /// are one step: this is the sequencer.
    pub fn submit(&mut self, raw: &Value) -> Result<Accepted, Reject> {
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

type Shared = Arc<Mutex<Hub>>;

/// Serves the hub. Once bound, writes its URL to `hub_file`, where this instance's clients look.
pub async fn serve(listen: SocketAddr, db: &Path, hub_file: &Path, token: String) -> Result<()> {
    if listen.ip().is_unspecified() {
        bail!("refusing to listen on {listen}: bind a specific address (default 127.0.0.1)");
    }
    let hub = Hub::new(Store::open(db)?)?;
    eprintln!(
        "agent-fleet hub: {} events in {}, listening on http://{listen}",
        hub.store.len()?,
        db.display()
    );
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| {
            format!("bind {listen}; if another instance's hub holds it, pass --listen")
        })?;
    std::fs::write(hub_file, format!("http://{listen}\n"))?;
    axum::serve(listener, router(hub, token)).await?;
    Ok(())
}

pub fn router(hub: Hub, token: String) -> Router {
    let hub: Shared = Arc::new(Mutex::new(hub));
    let api = Router::new()
        .route("/events", post(post_event).get(get_events))
        .route("/events/stream", get(stream))
        .route("/state", get(get_state))
        .route("/stats", get(get_stats))
        .route("/board", get(get_board))
        .route("/board/answer", post(post_answer))
        .route("/schema", get(|| async { Json(event::api_schema()) }))
        .layer(middleware::from_fn_with_state(Arc::new(token), auth));
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

/// The one way a human writes: the token holder answers an Inbox question from the board.
async fn post_answer(AxState(hub): AxState<Shared>, Json(b): Json<AnswerBody>) -> Response {
    let result = hub.lock().unwrap().answer(&b.ask, &b.answer, b.approve);
    match result {
        Ok(a) => Json(json!({ "id": a.id })).into_response(),
        Err(Reject::BadRequest(m)) => error(StatusCode::BAD_REQUEST, &m),
        Err(Reject::Forbidden(m)) => error(StatusCode::FORBIDDEN, &m),
        Err(Reject::Conflict(m)) => error(StatusCode::CONFLICT, &m),
        Err(Reject::Internal(m)) => error(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

async fn auth(AxState(token): AxState<Arc<String>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.as_bytes(), token.as_bytes()));
    if !ok {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong bearer token");
    }
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

async fn post_event(AxState(hub): AxState<Shared>, Json(raw): Json<Value>) -> Response {
    let started = Instant::now();
    let mut h = hub.lock().unwrap();
    let result = h.submit(&raw);
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
    use super::*;
    use crate::event::*;

    fn hub() -> Hub {
        Hub::new(Store::memory().unwrap()).unwrap()
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
    async fn refuses_unspecified_listen_address() {
        let dir = tempfile::tempdir().unwrap();
        for addr in ["0.0.0.0:7710", "[::]:7710"] {
            let err = serve(
                addr.parse().unwrap(),
                &dir.path().join("h.db"),
                &dir.path().join("hub"),
                "t".into(),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("refusing"), "{err}");
        }
    }
}
