//! `fleet hub`: the log, the fold, and the one sequencer for exclusive writes, over HTTP + SSE.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
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
use crate::store::Store;

pub struct Hub {
    store: Store,
    state: State,
    tx: broadcast::Sender<(String, String)>,
}

#[derive(Debug, PartialEq)]
pub struct Accepted {
    pub id: String,
    pub duplicate: bool,
}

impl Hub {
    pub fn new(store: Store) -> Result<Self> {
        let state = State::fold(&store.all()?);
        Ok(Self {
            store,
            state,
            tx: broadcast::channel(4096).0,
        })
    }

    /// Validates and appends one event. Runs under the hub lock, so the 409 check and the append
    /// are one step: this is the sequencer.
    pub fn submit(&mut self, raw: &Value) -> Result<Accepted, Reject> {
        let e = event::parse_strict(raw)?;
        let internal = |err: anyhow::Error| Reject::Internal(err.to_string());
        if self.store.contains(&e.id).map_err(internal)? {
            return Ok(Accepted {
                id: e.id,
                duplicate: true,
            });
        }
        if let (Known::TaskAssigned(_), Some(task)) = (&e.body, &e.task)
            && let Some(owner) = self.state.owner_of(task)
        {
            return Err(Reject::Conflict(format!(
                "{task} is already assigned to {owner}"
            )));
        }
        let e = e.into_event();
        self.store.append(&e).map_err(internal)?;
        self.state.apply(&e);
        let json = serde_json::to_string(&e).map_err(|err| internal(err.into()))?;
        let _ = self.tx.send((e.id.clone(), json));
        Ok(Accepted {
            id: e.id,
            duplicate: false,
        })
    }
}

type Shared = Arc<Mutex<Hub>>;

pub async fn serve(listen: SocketAddr, db: &Path, token: String) -> Result<()> {
    if listen.ip().is_unspecified() {
        bail!("refusing to listen on {listen}: bind a specific address (default 127.0.0.1)");
    }
    let hub = Hub::new(Store::open(db)?)?;
    eprintln!(
        "fleet hub: {} events in {}, listening on http://{listen}",
        hub.store.len()?,
        db.display()
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, router(hub, token)).await?;
    Ok(())
}

pub fn router(hub: Hub, token: String) -> Router {
    let hub: Shared = Arc::new(Mutex::new(hub));
    Router::new()
        .route("/events", post(post_event).get(get_events))
        .route("/events/stream", get(stream))
        .route("/state", get(get_state))
        .route("/schema", get(|| async { Json(event::api_schema()) }))
        .layer(middleware::from_fn_with_state(Arc::new(token), auth))
        .with_state(hub)
}

async fn auth(AxState(token): AxState<Arc<String>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == token.as_str());
    if !ok {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong bearer token");
    }
    next.run(req).await
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

async fn post_event(AxState(hub): AxState<Shared>, Json(raw): Json<Value>) -> Response {
    let result = hub.lock().unwrap().submit(&raw);
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
        Err(e) => error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

async fn get_state(AxState(hub): AxState<Shared>) -> Response {
    Json(hub.lock().unwrap().state.clone()).into_response()
}

/// The backlog after the cursor, then live events. Subscribing and reading the backlog happen
/// under the hub lock, so nothing is missed or sent twice between the two.
async fn stream(AxState(hub): AxState<Shared>, Query(q): Query<Cursor>) -> Response {
    let (backlog, rx) = {
        let hub = hub.lock().unwrap();
        let rx = hub.tx.subscribe();
        match hub.store.after(q.after.as_deref(), usize::MAX >> 1) {
            Ok(backlog) => (backlog, rx),
            Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
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
            let err = serve(addr.parse().unwrap(), &dir.path().join("h.db"), "t".into())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("refusing"), "{err}");
        }
    }
}
