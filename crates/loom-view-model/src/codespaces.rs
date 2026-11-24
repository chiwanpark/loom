use loom_oracle_proto::{
    Codespace, CodespaceState, DEFAULT_CPUS, DEFAULT_MEMORY_MIB, Repository, SessionSummary,
    repository_directory,
};
use loom_proto::{
    PolicyAction, ReasoningEffort, RepositoryStatus, SessionId, SessionInfo, SessionState,
};

pub fn state_label(codespace: &Codespace) -> String {
    match codespace.state {
        CodespaceState::Provisioning | CodespaceState::Starting => match &codespace.step {
            Some(step) => format!("{}: {step}", codespace.state),
            None => codespace.state.to_string(),
        },
        CodespaceState::Failed => match &codespace.error {
            Some(error) => format!("failed: {error}"),
            None => "failed".to_owned(),
        },
        CodespaceState::Ready if codespace.step.is_some() => {
            format!("ready: {}", codespace.step.as_deref().unwrap_or_default())
        }
        CodespaceState::Ready | CodespaceState::Stopped if codespace.error.is_some() => {
            format!(
                "{}: {}",
                codespace.state,
                codespace.error.as_deref().unwrap_or_default()
            )
        }
        state => state.to_string(),
    }
}

pub fn can_stop(codespace: &Codespace) -> bool {
    matches!(
        codespace.state,
        CodespaceState::Ready | CodespaceState::Failed
    ) && codespace.step.is_none()
}

pub fn is_starting(state: CodespaceState) -> bool {
    matches!(
        state,
        CodespaceState::Provisioning | CodespaceState::Starting
    )
}

pub fn is_listed(codespace: &Codespace) -> bool {
    codespace.state != CodespaceState::Closed
}

pub fn has_archive(codespace: &Codespace) -> bool {
    codespace.state == CodespaceState::Closed
        && codespace
            .archive
            .as_ref()
            .is_some_and(|archive| archive.saved_at.is_some())
}

pub fn directory(repository: &Repository) -> String {
    repository
        .directory
        .clone()
        .filter(|directory| !directory.is_empty())
        .or_else(|| repository_directory(&repository.url))
        .unwrap_or_else(|| repository.url.clone())
}

pub fn directories(repositories: &[Repository]) -> Vec<String> {
    repositories.iter().map(directory).collect()
}

pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn title(codespace: &Codespace) -> String {
    format!("{} · {}", codespace.workspace, codespace.branch)
}

pub fn format_memory(mib: u64) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

pub fn parse_memory(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "m" | "mb" | "mi" | "mib" => 1,
        "g" | "gb" | "gi" | "gib" => 1024,
        _ => return None,
    };
    number.checked_mul(factor).filter(|mib| *mib > 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCodespaceDefaults {
    pub preset: Option<String>,
    pub cpus: u32,
    pub memory_mib: u64,
}

pub fn new_codespace_defaults(codespaces: &[Codespace], workspace: &str) -> NewCodespaceDefaults {
    let last = codespaces
        .iter()
        .filter(|c| c.workspace == workspace)
        .max_by_key(|c| c.created_at);
    match last {
        Some(last) => NewCodespaceDefaults {
            preset: last.preset.clone(),
            cpus: last.cpus,
            memory_mib: last.memory_mib,
        },
        None => NewCodespaceDefaults {
            preset: None,
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
        },
    }
}

pub fn work_text(status: &RepositoryStatus) -> String {
    if let Some(error) = &status.error {
        return format!("unknown: {error}");
    }
    let mut parts = Vec::new();
    match status.changed_files {
        0 => {}
        1 => parts.push("1 changed file".to_owned()),
        n => parts.push(format!("{n} changed files")),
    }
    match status.unpushed {
        0 => {}
        1 => parts.push("1 commit that is not pushed".to_owned()),
        n => parts.push(format!("{n} commits that are not pushed")),
    }
    if parts.is_empty() {
        "no unsaved work".to_owned()
    } else {
        parts.join(", ")
    }
}

pub fn repository_of<'a>(path: &'a str, directories: &[String]) -> Option<&'a str> {
    if directories.len() < 2 {
        return None;
    }
    let (first, _) = path.split_once('/')?;
    directories.iter().any(|d| d == first).then_some(first)
}

pub fn session_state(name: &str) -> SessionState {
    match name {
        "thinking" => SessionState::Thinking,
        "running_tool" => SessionState::RunningTool,
        "waiting_approval" => SessionState::WaitingApproval,
        "responding" => SessionState::Responding,
        "completed" => SessionState::Completed,
        _ => SessionState::Idle,
    }
}

pub fn session_info(summary: &SessionSummary) -> SessionInfo {
    SessionInfo {
        id: SessionId(summary.id.clone()),
        model: summary.model.clone(),
        reasoning: ReasoningEffort::Medium,
        policy_default: PolicyAction::Ask,
        comment_guard: false,
        state: session_state(&summary.state),
        last_seq: 0,
        created_at: summary.created_at,
        title: summary.title.clone(),
        archived: summary.archived,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codespace(id: &str, workspace: &str, created_at: u64) -> Codespace {
        Codespace {
            id: id.into(),
            workspace: workspace.into(),
            host: "box".into(),
            preset: Some(format!("p-{id}")),
            state: CodespaceState::Ready,
            cpus: 4,
            memory_mib: 8192,
            instance: format!("loom-{id}"),
            branch: format!("loom/{id}"),
            base_branch: None,
            repositories: Vec::new(),
            agent: None,
            step: None,
            error: None,
            first_message: false,
            created_at,
            updated_at: created_at,
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
    fn only_closed_codespaces_with_a_saved_archive_have_an_archive() {
        let mut closed = codespace("a", "acme", 1);
        closed.state = CodespaceState::Closed;
        assert!(!has_archive(&closed));
        closed.archive = Some(loom_oracle_proto::CodespaceArchive {
            error: Some("too large".into()),
            ..Default::default()
        });
        assert!(!has_archive(&closed));
        closed.archive = Some(loom_oracle_proto::CodespaceArchive {
            size: 10,
            saved_at: Some(5),
            ..Default::default()
        });
        assert!(has_archive(&closed));
        let mut open = closed.clone();
        open.state = CodespaceState::Closing;
        assert!(!has_archive(&open));
    }

    #[test]
    fn memory_reads_and_shows_units() {
        assert_eq!(parse_memory("4 GiB"), Some(4096));
        assert_eq!(parse_memory("4g"), Some(4096));
        assert_eq!(parse_memory("512"), Some(512));
        assert_eq!(parse_memory("512 MiB"), Some(512));
        assert_eq!(parse_memory("0"), None);
        assert_eq!(parse_memory("x"), None);
        assert_eq!(parse_memory("2 TB"), None);
        assert_eq!(format_memory(4096), "4 GiB");
        assert_eq!(format_memory(1536), "1536 MiB");
    }

    #[test]
    fn a_new_codespace_starts_with_the_last_one_of_its_workspace() {
        let codespaces = vec![
            codespace("a", "acme", 1),
            codespace("b", "acme", 3),
            codespace("c", "other", 5),
        ];
        let defaults = new_codespace_defaults(&codespaces, "acme");
        assert_eq!(defaults.preset.as_deref(), Some("p-b"));
        assert_eq!((defaults.cpus, defaults.memory_mib), (4, 8192));
        let first = new_codespace_defaults(&codespaces, "new");
        assert_eq!(first.preset, None);
        assert_eq!((first.cpus, first.memory_mib), (2, 4096));
    }

    #[test]
    fn oracle_sessions_read_as_agent_sessions() {
        let summary = SessionSummary {
            id: "s1".into(),
            title: Some("Fix".into()),
            state: "waiting_approval".into(),
            archived: false,
            model: Some("openai/gpt".into()),
            created_at: 7,
            updated_at: 9,
        };
        let info = session_info(&summary);
        assert_eq!(info.id.0, "s1");
        assert_eq!(info.state, SessionState::WaitingApproval);
        assert_eq!(info.created_at, 7);
        assert_eq!(session_state("running_tool"), SessionState::RunningTool);
        assert_eq!(session_state("other"), SessionState::Idle);
    }

    #[test]
    fn states_show_their_step_or_error() {
        let mut c = codespace("a", "acme", 1);
        assert_eq!(state_label(&c), "ready");
        c.state = CodespaceState::Starting;
        c.step = Some("cloning app".into());
        assert_eq!(state_label(&c), "starting: cloning app");
        c.state = CodespaceState::Failed;
        c.error = Some("no disk".into());
        assert_eq!(state_label(&c), "failed: no disk");
        c.state = CodespaceState::Ready;
        c.step = Some("stopping".into());
        assert_eq!(state_label(&c), "ready: stopping");
        assert!(!can_stop(&c));
        c.step = None;
        assert!(can_stop(&c));
        c.state = CodespaceState::Stopped;
        assert_eq!(state_label(&c), "stopped: no disk");
        assert!(!can_stop(&c));
        assert!(is_starting(CodespaceState::Provisioning));
        assert!(!is_starting(CodespaceState::Ready));
        c.state = CodespaceState::Closed;
        assert!(!is_listed(&c));
    }

    #[test]
    fn paths_name_their_repository_when_there_are_several() {
        let dirs = vec!["app".to_owned(), "lib".to_owned()];
        assert_eq!(repository_of("app/src/a.rs", &dirs), Some("app"));
        assert_eq!(repository_of("other/a.rs", &dirs), None);
        assert_eq!(repository_of("app", &dirs), None);
        assert_eq!(repository_of("app/a.rs", &dirs[..1]), None);
        let repos = vec![
            Repository {
                url: "https://github.com/acme/server.git".into(),
                provider: None,
                directory: None,
                base_branch: None,
            },
            Repository {
                url: "https://github.com/acme/web".into(),
                provider: None,
                directory: Some("client".into()),
                base_branch: None,
            },
        ];
        assert_eq!(directories(&repos), ["server", "client"]);
    }

    #[test]
    fn the_guard_describes_the_work() {
        let mut status = RepositoryStatus {
            directory: "app".into(),
            path: "/workspaces/app".into(),
            url: "u".into(),
            provider: None,
            branch: "loom/a".into(),
            base_branch: None,
            changed_files: 0,
            ahead: 0,
            unpushed: 0,
            pushed: false,
            pull_request: None,
            error: None,
        };
        assert_eq!(work_text(&status), "no unsaved work");
        status.changed_files = 2;
        status.unpushed = 1;
        assert_eq!(
            work_text(&status),
            "2 changed files, 1 commit that is not pushed"
        );
        status.error = Some("gone".into());
        assert_eq!(work_text(&status), "unknown: gone");
    }
}
