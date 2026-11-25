//! `agent-fleet adopt <pane> <name>`: re-register a live herdr pane as a named agent the hub
//! lost (#206). The CLI renames the pane in herdr and logs `agent.renamed` (rule `adopt`); the
//! host's tick sees that event and registers the pane (`Host::adopted`). The decisions are pure.

use std::collections::HashSet;

use chrono::{Duration, Utc};

use crate::event::{Body, Known};
use crate::spawn::{agent_prefix, short};
use crate::state::State;

/// The host acts on an `adopt` event this young only: a restart does not replay the log.
const FRESH: Duration = Duration::minutes(10);

#[derive(Debug, PartialEq)]
pub struct Plan {
    /// The full id the pane becomes.
    pub agent: String,
    /// The pane-id record the hub holds for it now, taken down once the new one is up.
    pub from: Option<String>,
}

/// Who `pane` becomes, or why not. A pane another agent holds is refused; the pane-id record
/// (`agent:<host>/<session>/<pane>`, what a pane herdr cannot name gets) is the lost agent
/// itself and is replaced. A name already up in another pane is refused too.
pub fn plan(s: &State, host: &str, session: &str, pane: &str, name: &str) -> Result<Plan, String> {
    if name.is_empty() || name.contains(['/', ' ']) {
        return Err(format!("`{name}` is not an agent name"));
    }
    let agent = format!("{}{name}", agent_prefix(host, session));
    let mut from = None;
    for (id, a) in &s.agents {
        if a.host != host || !id.starts_with(&agent_prefix(host, session)) {
            continue;
        }
        let here = a.pane.as_deref() == Some(pane);
        if *id == agent && !here {
            let at = a.pane.as_deref().unwrap_or("another pane");
            return Err(format!("{name} is already up in {at}"));
        }
        if here && *id != agent {
            if short(id) == pane {
                from = Some(id.clone());
            } else {
                return Err(format!("pane {pane} is held by {}", short(id)));
            }
        }
    }
    Ok(Plan { agent, from })
}

/// The host's side: register each pane adopted on this host, once.
pub async fn serve(
    host: &std::sync::Arc<crate::host::Host>,
    events: &[crate::event::Event],
    seen: &mut HashSet<String>,
) {
    let prefix = agent_prefix(&host.cfg.host, &host.cfg.session);
    let now = Utc::now();
    for e in events {
        let Body::Known(Known::AgentRenamed(r)) = &e.body else {
            continue;
        };
        if r.rule != "adopt" || !r.agent.starts_with(&prefix) || now - e.ts > FRESH {
            continue;
        }
        if seen.insert(e.id.clone()) {
            host.adopt(&r.pane).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentState;
    use crate::state::Agent;

    fn up(s: &mut State, id: &str, pane: &str) {
        s.agents.insert(
            id.into(),
            Agent {
                host: "h".into(),
                kind: None,
                pane: Some(pane.into()),
                state: AgentState::Idle,
                since: Utc::now(),
                worked: None,
                finished: None,
                usage: None,
            },
        );
    }

    #[test]
    fn a_pane_the_hub_knows_only_by_its_id_is_adopted_and_the_stub_is_named() {
        let mut s = State::default();
        up(&mut s, "agent:h/s/w1:p1", "w1:p1");
        let p = plan(&s, "h", "s", "w1:p1", "asa").unwrap();
        assert_eq!(p.agent, "agent:h/s/asa");
        assert_eq!(p.from.as_deref(), Some("agent:h/s/w1:p1"));
    }

    #[test]
    fn an_unknown_pane_is_adopted_with_no_stub() {
        let p = plan(&State::default(), "h", "s", "w1:p1", "asa").unwrap();
        assert_eq!(p.from, None);
    }

    #[test]
    fn a_pane_another_agent_holds_is_refused() {
        let mut s = State::default();
        up(&mut s, "agent:h/s/other", "w1:p1");
        let e = plan(&s, "h", "s", "w1:p1", "asa").unwrap_err();
        assert!(e.contains("held by other"), "{e}");
    }

    #[test]
    fn a_name_up_in_another_pane_is_refused_and_the_same_pane_is_a_no_op() {
        let mut s = State::default();
        up(&mut s, "agent:h/s/asa", "w2:p1");
        let e = plan(&s, "h", "s", "w1:p1", "asa").unwrap_err();
        assert!(e.contains("already up in w2:p1"), "{e}");
        assert!(plan(&s, "h", "s", "w2:p1", "asa").is_ok());
        assert!(plan(&s, "h", "s", "w1:p1", "a b").is_err());
    }
}
