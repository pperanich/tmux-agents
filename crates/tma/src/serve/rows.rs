//! Host rows and edges as the frames the wire carries.
//!
//! Both mappings go **through the shipped writers**, `RowSurface::Protocol` and
//! `render_edge_json`, and are read back into the typed frame rather than being rebuilt field by
//! field. That is what makes the title rule a property of the code path instead of a convention: the one place
//! `title` could be written is `write_row_fields`, and it writes it only for `RowSurface::Local`,
//! so no amount of editing here can put a pane title on the wire. The cost is one JSON parse per
//! row per snapshot, against a fleet of a dozen panes.

use tma_core::{AgentRow, Selector, StateToken};
use tma_proto::{Edge, FleetRow};
use tma_runtime::json::JsonWriter;
use tma_runtime::origin::Origin;
use tma_ui::surfaces::{render_edge_json, write_row_fields, RowSurface};

/// One row as its wire frame. PRECONDITION, inherited from [`write_row_fields`]: the caller has run
/// `tma_runtime::repo::annotate_rows` on the row, or its repo label reads as "no git checkout".
pub(crate) fn fleet_row(row: &AgentRow, origin: &Origin) -> Result<FleetRow, serde_json::Error> {
    let mut j = JsonWriter::new();
    j.begin_object();
    write_row_fields(&mut j, row, origin, RowSurface::Protocol);
    j.end_object();
    serde_json::from_str(&j.finish())
}

/// One observed transition as its wire frame. The host writer carries a `schema` key the frame
/// envelope already states; an unknown field is ignored on read, which is the protocol's own rule.
pub(crate) fn edge(edge: &tma_core::Edge, at_ms: u64) -> Result<Edge, serde_json::Error> {
    serde_json::from_str(&render_edge_json(edge, at_ms))
}

/// The wire selector as the host's own. An absent selector is the whole fleet, and a state token
/// this build cannot parse narrows nothing rather than matching everything: the device asked for a
/// class that does not exist here, so it gets no rows from it.
pub(crate) fn selector(wire: Option<&tma_proto::Selector>) -> Selector {
    let Some(wire) = wire else {
        return Selector::default();
    };
    Selector {
        session: wire.session.clone(),
        repo: wire.repo.clone(),
        branch: wire.branch.clone(),
        agent: wire.agent.clone(),
        state: wire
            .state
            .iter()
            .filter_map(|s| s.token().parse::<StateToken>().ok())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::AgentState;
    use tma_proto::{State, StateFilter};

    fn row() -> AgentRow {
        AgentRow {
            pane_id: "%5".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Blocked,
            detail: Some("permission".to_string()),
            since: 1_730_000_000,
            turn_at: 0,
            session: "s1".to_string(),
            window_index: 2,
            pane_index: 3,
            title: "a pane title nothing off this machine may see".to_string(),
            attention: false,
            agent_session: None,
            transcript: None,
            permission_request: Some("per_1".to_string()),
            stamped_at: Some(1_730_000_000_500),
            context_pct: None,
            context_at: None,
            tokens: None,
            quota: None,
            cost_usd: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
            pending: None,
        }
    }

    fn origin() -> Origin {
        Origin {
            server: "/tmp/tmux-501/default".to_string(),
            host: "example".to_string(),
        }
    }

    /// The mapping's whole job. `title` is the one key the local surface has and this one does not,
    /// and the row is built by the same writer, so this cannot pass while the wire carries one.
    #[test]
    fn a_wire_row_carries_the_pane_but_never_its_title() {
        let mut j = JsonWriter::new();
        j.begin_object();
        write_row_fields(&mut j, &row(), &origin(), RowSurface::Protocol);
        j.end_object();
        let text = j.finish();
        assert!(!text.contains("title"), "{text}");
        assert!(!text.contains("a pane title"), "{text}");

        let wire = fleet_row(&row(), &origin()).expect("the protocol row parses as a FleetRow");
        assert_eq!(wire.pane, "%5");
        assert_eq!(wire.agent, "claude");
        assert_eq!(wire.state, State::Blocked);
        assert_eq!(wire.detail, Some(tma_proto::Detail::Permission));
        assert_eq!(wire.locator, "s1:2.3");
        assert_eq!(wire.permission_request.as_deref(), Some("per_1"));
        assert_eq!(wire.stamped_at_ms, Some(1_730_000_000_500));
        assert_eq!(wire.server, "/tmp/tmux-501/default");
        assert_eq!(wire.host, "example");
    }

    #[test]
    fn an_edge_maps_through_the_host_writer() {
        let core = tma_core::Edge {
            pane_id: "%5".to_string(),
            agent: "claude".to_string(),
            from: Some(StateToken::Closed(AgentState::Working)),
            to: Some(StateToken::Closed(AgentState::Blocked)),
            detail: Some("permission".to_string()),
            locator: "s1:2.3".to_string(),
            repo: None,
        };
        let wire = edge(&core, 1_730_000_000_000).expect("the edge line parses as an Edge");
        assert_eq!(wire.at_ms, 1_730_000_000_000);
        assert_eq!(wire.from, Some(State::Working));
        assert_eq!(wire.to, Some(State::Blocked));
        assert_eq!(wire.repo, None);
    }

    #[test]
    fn an_absent_selector_is_the_whole_fleet_and_a_present_one_maps_token_for_token() {
        assert!(selector(None).is_empty());
        let wire = tma_proto::Selector {
            session: Some("s1".to_string()),
            repo: Some("tma".to_string()),
            branch: None,
            agent: Some("claude".to_string()),
            state: vec![StateFilter::Blocked, StateFilter::Done],
        };
        let host = selector(Some(&wire));
        assert_eq!(host.session.as_deref(), Some("s1"));
        assert_eq!(host.repo.as_deref(), Some("tma"));
        assert_eq!(host.agent.as_deref(), Some("claude"));
        assert_eq!(
            host.state,
            vec![StateToken::Closed(AgentState::Blocked), StateToken::Done]
        );
    }
}
