//! The sessions panel as a flat list: one row per session of the connected
//! agents, grouped by what it is doing. Agents without sessions get no row;
//! the panel lists the agents under their hosts.

use loom_proto::{SessionInfo, SessionState};

/// How the client is linked to an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connected,
    Connecting,
    /// Not connected, with the reason when the link was lost.
    Offline(Option<String>),
}

/// An agent as the panel sees it. `sessions` only matter while connected.
#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub remote: String,
    pub workspace: String,
    pub link: Link,
    pub sessions: Vec<SessionInfo>,
}

/// The panel's sections, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    NeedsApproval,
    Working,
    Completed,
    Idle,
    Archived,
}

impl Group {
    pub const ALL: [Self; 5] = [
        Self::NeedsApproval,
        Self::Working,
        Self::Completed,
        Self::Idle,
        Self::Archived,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::NeedsApproval => "Needs approval",
            Self::Working => "Working",
            Self::Completed => "Completed",
            Self::Idle => "Idle",
            Self::Archived => "Archived",
        }
    }

    pub fn of(state: SessionState) -> Self {
        match state {
            SessionState::WaitingApproval => Self::NeedsApproval,
            SessionState::Thinking | SessionState::Responding | SessionState::RunningTool => {
                Self::Working
            }
            SessionState::Completed => Self::Completed,
            SessionState::Idle => Self::Idle,
        }
    }
}

/// One row: a session of the agent that `agent` indexes in the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub agent: usize,
    pub session: SessionInfo,
}

/// The rows of each non-empty group, in [`Group::ALL`] order; each group
/// keeps the agents' order and each agent's session order. Only connected
/// agents have rows.
pub fn group(agents: &[AgentEntry]) -> Vec<(Group, Vec<Row>)> {
    let mut groups: Vec<(Group, Vec<Row>)> = Group::ALL.iter().map(|g| (*g, Vec::new())).collect();
    let mut push = |group: Group, row: Row| {
        if let Some((_, rows)) = groups.iter_mut().find(|(g, _)| *g == group) {
            rows.push(row);
        }
    };
    for (agent, entry) in agents.iter().enumerate() {
        if entry.link != Link::Connected {
            continue;
        }
        for session in &entry.sessions {
            let group = if session.archived {
                Group::Archived
            } else {
                Group::of(session.state)
            };
            push(
                group,
                Row {
                    agent,
                    session: session.clone(),
                },
            );
        }
    }
    groups.retain(|(_, rows)| !rows.is_empty());
    groups
}

/// `/home/user/src/loom` as `~/src/loom` when it is under `home`.
pub fn short_path(path: &str, home: Option<&str>) -> String {
    match home.map(|h| h.trim_end_matches('/')) {
        Some(home) if !home.is_empty() => match path.strip_prefix(home) {
            Some("") => "~".into(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.into(),
        },
        _ => path.into(),
    }
}

#[cfg(test)]
mod tests {
    use loom_proto::SessionId;

    use super::*;

    fn session(id: &str, state: SessionState, archived: bool) -> SessionInfo {
        SessionInfo {
            id: SessionId(id.into()),
            model: None,
            reasoning: None,
            policy_default: loom_proto::PolicyAction::Ask,
            comment_guard: true,
            state,
            last_seq: 0,
            created_at: 0,
            title: None,
            archived,
            worktree: None,
        }
    }

    fn agent(remote: &str, link: Link, sessions: Vec<SessionInfo>) -> AgentEntry {
        AgentEntry {
            remote: remote.into(),
            workspace: format!("/w/{remote}"),
            link,
            sessions,
        }
    }

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| format!("{}:{}", row.agent, row.session.id.0))
            .collect()
    }

    #[test]
    fn groups_sessions_by_state_across_agents() {
        let agents = vec![
            agent(
                "a",
                Link::Connected,
                vec![
                    session("s1", SessionState::Idle, false),
                    session("s2", SessionState::RunningTool, false),
                    session("s3", SessionState::Idle, true),
                    session("s8", SessionState::Completed, false),
                ],
            ),
            agent(
                "b",
                Link::Connecting,
                vec![session("x", SessionState::Thinking, false)],
            ),
            agent(
                "c",
                Link::Connected,
                vec![
                    session("s4", SessionState::WaitingApproval, false),
                    session("s5", SessionState::Thinking, false),
                    session("s6", SessionState::Responding, false),
                ],
            ),
            agent("d", Link::Connected, vec![]),
            agent("e", Link::Offline(Some("gone".into())), vec![]),
            agent(
                "f",
                Link::Connected,
                vec![session("s7", SessionState::Idle, true)],
            ),
        ];
        let groups = group(&agents);
        let flat: Vec<(Group, Vec<String>)> =
            groups.iter().map(|(g, rows)| (*g, ids(rows))).collect();
        assert_eq!(
            flat,
            vec![
                (Group::NeedsApproval, vec!["2:s4".to_owned()]),
                (
                    Group::Working,
                    vec!["0:s2".to_owned(), "2:s5".to_owned(), "2:s6".to_owned()]
                ),
                (Group::Completed, vec!["0:s8".to_owned()]),
                // Agents without sessions and agents that are not connected
                // get no rows.
                (Group::Idle, vec!["0:s1".to_owned()]),
                (Group::Archived, vec!["0:s3".to_owned(), "5:s7".to_owned()]),
            ]
        );
    }

    #[test]
    fn omits_empty_groups() {
        assert!(group(&[]).is_empty());
        let groups = group(&[agent(
            "a",
            Link::Connected,
            vec![session("s", SessionState::Idle, false)],
        )]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, Group::Idle);
    }

    #[test]
    fn shortens_home_paths() {
        let home = Some("/home/me/");
        assert_eq!(short_path("/home/me/src/loom", home), "~/src/loom");
        assert_eq!(short_path("/home/me", home), "~");
        assert_eq!(short_path("/home/meow/x", home), "/home/meow/x");
        assert_eq!(short_path("/srv/x", None), "/srv/x");
    }
}
