use loom_oracle_proto::{Codespace, CodespaceState};
use loom_proto::{ArchivedLocation, ArchivedSessionInfo, SessionInfo, SessionState};

use crate::codespaces::{short_id, state_label};
use crate::settings::age;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Activity {
    NeedsApproval,
    Working,
    Completed,
    Idle,
}

impl Activity {
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

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsApproval => "needs approval",
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Idle => "idle",
        }
    }
}

pub fn combined<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>) -> Option<Activity> {
    sessions
        .into_iter()
        .filter(|s| !s.archived)
        .map(|s| Activity::of(s.state))
        .min()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodespaceRow {
    pub title: String,
    pub oracle: String,
    pub detail: String,
    pub sessions: usize,
    pub activity: Option<Activity>,
}

impl CodespaceRow {
    pub fn subtitle(&self) -> String {
        format!("{} · {}", self.oracle, self.detail)
    }
}

pub fn row(oracle: &str, codespace: &Codespace, sessions: &[SessionInfo]) -> CodespaceRow {
    let count = sessions.iter().filter(|s| !s.archived).count();
    let ready = codespace.state == CodespaceState::Ready;
    CodespaceRow {
        title: format!("{} · {}", codespace.workspace, codespace.branch),
        oracle: oracle.to_owned(),
        detail: format!(
            "{} · {} · {count} session{}",
            codespace.preset.as_deref().unwrap_or("no preset"),
            state_label(codespace),
            if count == 1 { "" } else { "s" }
        ),
        sessions: count,
        activity: combined(sessions).filter(|_| ready),
    }
}

pub fn archived_title(session: &ArchivedSessionInfo) -> String {
    session
        .title
        .clone()
        .unwrap_or_else(|| format!("Session {}", short_id(&session.id.0)))
}

pub fn archived_detail(now: u64, session: &ArchivedSessionInfo) -> String {
    let place = match &session.location {
        ArchivedLocation::Agent => "this codespace".to_owned(),
        ArchivedLocation::Codespace { branch, .. } => format!("closed codespace {branch}"),
    };
    format!("{} · {place}", age(now, session.updated_at))
}

pub fn archived_codespace(session: &ArchivedSessionInfo) -> Option<String> {
    match &session.location {
        ArchivedLocation::Agent => None,
        ArchivedLocation::Codespace { codespace, .. } => Some(codespace.clone()),
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
            reasoning: loom_proto::ReasoningEffort::Medium,
            policy_default: loom_proto::PolicyAction::Ask,
            comment_guard: true,
            state,
            last_seq: 0,
            created_at: 0,
            title: None,
            archived,
        }
    }

    fn codespace(state: CodespaceState) -> Codespace {
        Codespace {
            id: "k3x9".into(),
            workspace: "acme".into(),
            host: "box".into(),
            preset: Some("dev".into()),
            state,
            cpus: 4,
            memory_mib: 8192,
            instance: "loom-k3x9".into(),
            branch: "loom/k3x9".into(),
            base_branch: None,
            repositories: Vec::new(),
            agent: None,
            step: None,
            error: None,
            first_message: false,
            created_at: 1,
            updated_at: 1,
            active: None,
            active_at: None,
            metrics: None,
            stopped_at: None,
            stop_at: None,
            delete_at: None,
            archive: None,
        }
    }

    #[test]
    fn the_most_urgent_open_session_gives_the_activity() {
        assert_eq!(combined(&[]), None);
        let sessions = [
            session("a", SessionState::Idle, false),
            session("b", SessionState::Completed, false),
            session("c", SessionState::WaitingApproval, true),
            session("d", SessionState::RunningTool, false),
        ];
        assert_eq!(combined(&sessions), Some(Activity::Working));
        assert_eq!(combined(&sessions[..2]), Some(Activity::Completed));
        assert_eq!(combined(&sessions[2..3]), None);
        assert_eq!(
            combined(&[session("e", SessionState::WaitingApproval, false)]),
            Some(Activity::NeedsApproval)
        );
    }

    #[test]
    fn a_row_names_the_workspace_branch_oracle_and_sessions() {
        let sessions = [
            session("a", SessionState::Thinking, false),
            session("b", SessionState::Idle, true),
        ];
        let ready = row("home", &codespace(CodespaceState::Ready), &sessions);
        assert_eq!(ready.title, "acme · loom/k3x9");
        assert_eq!(ready.subtitle(), "home · dev · ready · 1 session");
        assert_eq!(ready.sessions, 1);
        assert_eq!(ready.activity, Some(Activity::Working));

        let stopped = row("home", &codespace(CodespaceState::Stopped), &[]);
        assert_eq!(stopped.subtitle(), "home · dev · stopped · 0 sessions");
        assert_eq!(stopped.activity, None);
    }

    #[test]
    fn archived_sessions_say_when_and_where_they_are() {
        let mut session = ArchivedSessionInfo {
            id: SessionId("0a1b2c3d4e5f".into()),
            title: None,
            model: None,
            created_at: 10,
            updated_at: 1000,
            location: ArchivedLocation::Agent,
        };
        assert_eq!(archived_title(&session), "Session 0a1b2c3d");
        assert_eq!(
            archived_detail(1000 + 7200, &session),
            "2 h ago · this codespace"
        );
        assert_eq!(archived_codespace(&session), None);
        session.title = Some("Fix the parser".into());
        session.location = ArchivedLocation::Codespace {
            codespace: "k3x9".into(),
            branch: "loom/k3x9".into(),
        };
        assert_eq!(archived_title(&session), "Fix the parser");
        assert_eq!(
            archived_detail(1000, &session),
            "just now · closed codespace loom/k3x9"
        );
        assert_eq!(archived_codespace(&session).as_deref(), Some("k3x9"));
    }
}
