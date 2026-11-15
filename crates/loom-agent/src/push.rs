//! Notifications to paired phones: when a tool call waits for approval and
//! when a turn completes or fails, the agent seals a notice for every
//! client of the host that registered a push token, and its relay sends it
//! through the client's push service.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use loom_agent_core::{Agent, SessionObserver};
use loom_proto::push::{PUSH_NOTICE_VERSION, PushKind, PushNotice, push_text};
use loom_proto::{AgentInfo, SessionEvent, SessionId, ToolCall, ToolCallId, TurnOutcome};
use loom_transport::{PushChannel, PushRequest};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::relay::LocalHostKeys;

/// Notifications that wait for the relay link, and events that wait to
/// become notifications; beyond these, new ones are dropped.
const QUEUE: usize = 64;

/// Something to tell the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEvent {
    pub session: SessionId,
    pub kind: PushKind,
    pub detail: Option<String>,
}

/// What a session's events said so far that a notification needs.
#[derive(Default)]
struct Track {
    /// The tool calls in progress, as the notification names them.
    tools: HashMap<ToolCallId, String>,
    answer: Option<String>,
    error: Option<String>,
}

/// Turns session events into [`PushEvent`]s.
pub struct PushObserver {
    events: mpsc::Sender<PushEvent>,
    tracks: Mutex<HashMap<SessionId, Track>>,
}

impl PushObserver {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<PushEvent>) {
        let (events, rx) = mpsc::channel(QUEUE);
        let observer = Arc::new(Self {
            events,
            tracks: Mutex::default(),
        });
        (observer, rx)
    }

    fn send(&self, event: PushEvent) {
        if self.events.try_send(event).is_err() {
            tracing::debug!("dropped a notification: too many wait");
        }
    }
}

impl SessionObserver for PushObserver {
    fn event(&self, session: &SessionId, event: &SessionEvent) {
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        let track = tracks.entry(session.clone()).or_default();
        let push = |kind, detail| PushEvent {
            session: session.clone(),
            kind,
            detail,
        };
        match event {
            SessionEvent::ToolCallRequested { call } => {
                track.tools.insert(call.call_id.clone(), tool_label(call));
            }
            SessionEvent::ToolCallFinished { call_id, .. } => {
                track.tools.remove(call_id);
            }
            SessionEvent::ApprovalRequired { call_id, .. } => {
                let detail = track.tools.get(call_id).cloned();
                self.send(push(PushKind::Approval, detail));
            }
            SessionEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => {
                track.answer = Some(text.clone());
            }
            SessionEvent::Error { message } => track.error = Some(message.clone()),
            SessionEvent::TurnFinished { outcome } => {
                let track = std::mem::take(track);
                match outcome {
                    TurnOutcome::Completed => self.send(push(PushKind::Finished, track.answer)),
                    TurnOutcome::Failed => self.send(push(PushKind::Failed, track.error)),
                    // The user stopped it.
                    TurnOutcome::Interrupted => {}
                }
            }
            SessionEvent::Cleared => {
                tracks.remove(session);
            }
            _ => {}
        }
    }
}

/// A tool call as a notification names it: the tool, and its command or
/// path when it has one.
fn tool_label(call: &ToolCall) -> String {
    let arguments: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let field = |name: &str| arguments.get(name).and_then(|v| v.as_str());
    match field("command").or_else(|| field("path")) {
        Some(what) => {
            let first = what.lines().next().unwrap_or_default();
            format!("{}: {first}", call.name)
        }
        None => call.name.clone(),
    }
}

/// The first line of an answer, which says the most in a notification.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// The notice for `event`.
fn notice(agent: &Agent, info: &AgentInfo, event: PushEvent) -> PushNotice {
    let title = agent
        .session(&event.session)
        .ok()
        .and_then(|s| s.info().title);
    let detail = event.detail.map(|detail| match event.kind {
        PushKind::Finished => push_text(first_line(&detail)),
        PushKind::Approval | PushKind::Failed => push_text(&detail),
    });
    PushNotice {
        version: PUSH_NOTICE_VERSION,
        instance: info.instance.clone(),
        workspace: push_text(&info.workspace.display().to_string()),
        session: event.session,
        title: title.as_deref().map(push_text),
        kind: event.kind,
        detail,
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
    }
}

/// The relay link's end of the notifications, and the task that seals them
/// for every client with a push token and forgets the tokens the push
/// service rejects.
pub fn start_push(
    agent: Agent,
    info: AgentInfo,
    keys: Arc<LocalHostKeys>,
    mut events: mpsc::Receiver<PushEvent>,
    shutdown: CancellationToken,
) -> PushChannel {
    let (outgoing, outgoing_rx) = mpsc::channel(QUEUE);
    let (rejected_tx, mut rejected) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                event = events.recv() => {
                    let Some(event) = event else { return };
                    for request in seal_for_clients(&agent, &info, &keys, event) {
                        if outgoing.try_send(request).is_err() {
                            tracing::debug!("dropped a notification: the relay link is behind");
                        }
                    }
                }
                Some(token) = rejected.recv() => {
                    match keys.clients.drop_push_token(&token) {
                        Ok(true) => tracing::info!("forgot a push token the push service rejected"),
                        Ok(false) => {}
                        Err(err) => tracing::warn!(error = %err, "could not forget a push token"),
                    }
                }
            }
        }
    });
    PushChannel {
        outgoing: outgoing_rx,
        rejected: rejected_tx,
    }
}

fn seal_for_clients(
    agent: &Agent,
    info: &AgentInfo,
    keys: &LocalHostKeys,
    event: PushEvent,
) -> Vec<PushRequest> {
    let clients = match keys.clients.list() {
        Ok(clients) => clients,
        Err(err) => {
            tracing::warn!(error = %err, "cannot read the authorized client list");
            return Vec::new();
        }
    };
    let targets: Vec<_> = clients
        .into_iter()
        .filter_map(|c| Some((c.pubkey, c.push_token?)))
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    let plaintext = match loom_proto::encode(&notice(agent, info, event)) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not encode a notification");
            return Vec::new();
        }
    };
    targets
        .into_iter()
        .filter_map(|(pubkey, token)| {
            match loom_crypto::seal_push(&keys.identity, &pubkey, &plaintext) {
                Ok(payload) => Some(PushRequest { token, payload }),
                Err(err) => {
                    tracing::warn!(error = %err, "could not seal a notification");
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_proto::Decision;

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            risk: None,
        }
    }

    #[test]
    fn approvals_and_ended_turns_become_notifications() {
        let (observer, mut events) = PushObserver::new();
        let session = SessionId("s".into());
        let event = |e: SessionEvent| observer.event(&session, &e);

        event(SessionEvent::ToolCallRequested {
            call: call("1", "shell", r#"{"command":"cargo test\n--all"}"#),
        });
        event(SessionEvent::ApprovalRequired {
            call_id: "1".into(),
            reason: "ask".into(),
        });
        event(SessionEvent::ApprovalResolved {
            call_id: "1".into(),
            decision: Decision::Allow,
        });
        event(SessionEvent::AssistantMessage {
            text: "\nAll tests pass.\nDetails follow.".into(),
            reasoning: None,
            model: "m".into(),
        });
        event(SessionEvent::TurnFinished {
            outcome: TurnOutcome::Completed,
        });
        event(SessionEvent::Error {
            message: "the provider is down".into(),
        });
        event(SessionEvent::TurnFinished {
            outcome: TurnOutcome::Failed,
        });
        event(SessionEvent::TurnFinished {
            outcome: TurnOutcome::Interrupted,
        });

        let mut got = Vec::new();
        while let Ok(event) = events.try_recv() {
            got.push((event.kind, event.detail));
        }
        assert_eq!(
            got,
            [
                (PushKind::Approval, Some("shell: cargo test".into())),
                (
                    PushKind::Finished,
                    Some("\nAll tests pass.\nDetails follow.".into())
                ),
                (PushKind::Failed, Some("the provider is down".into())),
            ]
        );
        assert_eq!(first_line("\n  All tests pass.\nmore"), "All tests pass.");
    }

    #[test]
    fn tools_are_named_by_their_command_or_path() {
        assert_eq!(
            tool_label(&call("1", "write", r#"{"path":"src/a.rs","content":"x"}"#)),
            "write: src/a.rs"
        );
        assert_eq!(tool_label(&call("1", "edit", r#"{"patch":"..."}"#)), "edit");
        assert_eq!(tool_label(&call("1", "odd", "not json")), "odd");
    }
}
