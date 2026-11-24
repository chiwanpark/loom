use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
};
use gpui_component::{ActiveTheme, v_flex};
use loom_client::oracle_proto::SessionSummary;
use loom_client::proto::{SequencedEvent, SessionEvent, SessionState};
use loom_view_model::codespaces::session_info;

use crate::conversation::{self, Conversation};
use crate::oracle::OracleLink;
use crate::ui::messages::MessageList;

enum Load {
    Loading,
    Failed(String),
    Loaded,
}

pub struct ArchiveView {
    codespace: String,
    session: String,
    title: SharedString,
    conversation: Entity<Conversation>,
    messages: Entity<MessageList>,
    load: Load,
    focus: FocusHandle,
}

pub fn finish(conversation: &mut Conversation, events: &[SequencedEvent]) {
    for event in events {
        conversation.apply(event);
    }
    if conversation.is_busy() {
        let seq = events.last().map_or(0, |event| event.seq + 1);
        conversation.apply(&SequencedEvent {
            seq,
            event: SessionEvent::StateChanged {
                state: SessionState::Idle,
            },
        });
    }
}

impl ArchiveView {
    pub fn new(
        link: Entity<OracleLink>,
        codespace: String,
        summary: &SessionSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut info = session_info(summary);
        info.state = SessionState::Idle;
        let title = summary
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Untitled session".into());
        let conversation = cx.new(|_| conversation::new(info));
        let messages = cx.new(|cx| MessageList::new(conversation.clone(), cx));
        let mut this = Self {
            codespace,
            session: summary.id.clone(),
            title: title.into(),
            conversation,
            messages,
            load: Load::Loading,
            focus: cx.focus_handle(),
        };
        this.load(link, window, cx);
        this
    }

    pub fn is(&self, codespace: &str, session: &str) -> bool {
        self.codespace == codespace && self.session == session
    }

    pub fn codespace(&self) -> &str {
        &self.codespace
    }

    pub fn conversation(&self) -> &Entity<Conversation> {
        &self.conversation
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self.load, Load::Loaded)
    }

    fn load(&mut self, link: Entity<OracleLink>, window: &mut Window, cx: &mut Context<Self>) {
        let read = link.read(cx);
        let Some(client) = read.client() else {
            self.load = Load::Failed("The client is not connected to the oracle.".into());
            return;
        };
        let (codespace, session) = (self.codespace.clone(), self.session.clone());
        let task = read
            .backend()
            .run(async move { client.archived_events(&codespace, &session).await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.load = match result {
                    Some(Ok(events)) => {
                        this.conversation.update(cx, |conversation, cx| {
                            finish(conversation, &events);
                            cx.notify();
                        });
                        Load::Loaded
                    }
                    Some(Err(err)) => Load::Failed(err.to_string()),
                    None => Load::Failed("The request was cancelled.".into()),
                };
                cx.notify();
            });
        })
        .detach();
    }
}

impl Focusable for ArchiveView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ArchiveView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = match &self.load {
            Load::Loading => div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Reading the archive…")
                .into_any_element(),
            Load::Failed(error) => div()
                .p_4()
                .text_sm()
                .text_color(theme.danger)
                .child(format!("Cannot read the archive: {error}"))
                .into_any_element(),
            Load::Loaded => self.messages.clone().into_any_element(),
        };
        v_flex()
            .id("archive-view")
            .debug_selector(|| "archive-view".into())
            .track_focus(&self.focus)
            .size_full()
            .child(div().flex_1().min_h_0().w_full().child(body))
    }
}

#[cfg(test)]
mod tests {
    use loom_client::proto::{ContentPart, SessionId, SessionInfo};

    use super::*;

    #[test]
    fn an_archived_turn_that_did_not_end_shows_as_idle() {
        let mut conversation = conversation::new(SessionInfo {
            id: SessionId("s1".into()),
            ..session_info(&SessionSummary {
                id: "s1".into(),
                title: None,
                state: "idle".into(),
                archived: false,
                model: None,
                created_at: 0,
                updated_at: 0,
            })
        });
        let events = [
            SequencedEvent {
                seq: 0,
                event: SessionEvent::UserMessage {
                    content: vec![ContentPart::text("hello")],
                },
            },
            SequencedEvent {
                seq: 1,
                event: SessionEvent::StateChanged {
                    state: SessionState::Thinking,
                },
            },
        ];
        finish(&mut conversation, &events);
        assert!(!conversation.is_busy());
        assert!(!conversation.item_keys().is_empty());
    }
}
