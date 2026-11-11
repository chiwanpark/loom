//! The agents panel as a flat list: one row per session (or per agent that
//! has none), grouped by what it is doing.

use loom_client::proto::{SessionInfo, SessionState};

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
    Idle,
    Offline,
    Archived,
}

impl Group {
    pub const ALL: [Self; 5] = [
        Self::NeedsApproval,
        Self::Working,
        Self::Idle,
        Self::Offline,
        Self::Archived,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::NeedsApproval => "Needs approval",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Offline => "Offline",
            Self::Archived => "Archived",
        }
    }

    pub fn of(state: SessionState) -> Self {
        match state {
            SessionState::WaitingApproval => Self::NeedsApproval,
            SessionState::Thinking | SessionState::RunningTool | SessionState::WaitingLock => {
                Self::Working
            }
            SessionState::Idle => Self::Idle,
        }
    }
}

/// One row: an agent's session, or the agent itself when it is offline or
/// has no sessions. `agent` indexes the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub agent: usize,
    pub session: Option<SessionInfo>,
}

/// The rows of each non-empty group, in [`Group::ALL`] order; each group
/// keeps the agents' order and each agent's session order.
pub fn group(agents: &[AgentEntry]) -> Vec<(Group, Vec<Row>)> {
    let mut groups: Vec<(Group, Vec<Row>)> = Group::ALL.iter().map(|g| (*g, Vec::new())).collect();
    let mut push = |group: Group, row: Row| {
        if let Some((_, rows)) = groups.iter_mut().find(|(g, _)| *g == group) {
            rows.push(row);
        }
    };
    for (agent, entry) in agents.iter().enumerate() {
        if entry.link != Link::Connected {
            push(
                Group::Offline,
                Row {
                    agent,
                    session: None,
                },
            );
            continue;
        }
        let mut live = false;
        for session in &entry.sessions {
            let group = if session.archived {
                Group::Archived
            } else {
                live = true;
                Group::of(session.state)
            };
            push(
                group,
                Row {
                    agent,
                    session: Some(session.clone()),
                },
            );
        }
        if !live {
            push(
                Group::Idle,
                Row {
                    agent,
                    session: None,
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
    use loom_client::proto::SessionId;

    use super::*;

    fn session(id: &str, state: SessionState, archived: bool) -> SessionInfo {
        SessionInfo {
            id: SessionId(id.into()),
            model: None,
            reasoning: None,
            policy_default: None,
            state,
            last_seq: 0,
            created_at: 0,
            title: None,
            archived,
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
            .map(|row| match &row.session {
                Some(s) => format!("{}:{}", row.agent, s.id.0),
                None => format!("{}:-", row.agent),
            })
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
                    session("s6", SessionState::WaitingLock, false),
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
                // Agents without live sessions still get a row, so they can
                // start one.
                (
                    Group::Idle,
                    vec!["0:s1".to_owned(), "3:-".to_owned(), "5:-".to_owned()]
                ),
                (Group::Offline, vec!["1:-".to_owned(), "4:-".to_owned()]),
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
