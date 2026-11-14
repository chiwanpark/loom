use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use loom_proto::{
    EventWindow, History, SequencedEvent, SessionEvent, SessionId, SessionState, UsageSoFar,
};

use crate::observer::SessionObserver;
use crate::store::EventStore;
use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 4096;

/// The most bytes of events one [`EventHub::history`] page holds, unless its
/// first item alone is larger.
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct Log {
    next_seq: u64,
    events: Vec<SequencedEvent>,
    state: SessionState,
}

pub struct EventHub {
    log: Mutex<Log>,
    tx: broadcast::Sender<SequencedEvent>,
    store: Option<(EventStore, SessionId)>,
    observer: OnceLock<(SessionId, Arc<dyn SessionObserver>)>,
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub").field("log", &self.log).finish()
    }
}

#[derive(Debug)]
pub struct Subscription {
    pub backlog: Vec<SequencedEvent>,
    /// Set when the backlog starts a new window of the session instead of
    /// continuing after the `since_seq` the subscriber asked for.
    pub window: Option<EventWindow>,
    pub live: broadcast::Receiver<SequencedEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        Self::restore(None, Vec::new())
    }

    pub fn restore(store: Option<(EventStore, SessionId)>, events: Vec<SequencedEvent>) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let next_seq = events.last().map(|e| e.seq + 1).unwrap_or(1);
        Self {
            log: Mutex::new(Log {
                next_seq,
                events,
                state: SessionState::Idle,
            }),
            tx,
            store,
            observer: OnceLock::new(),
        }
    }

    /// Reports every later event to `observer`. Only the first call counts.
    pub(crate) fn observe(&self, session: SessionId, observer: Arc<dyn SessionObserver>) {
        let _ = self.observer.set((session, observer));
    }

    pub fn events(&self) -> Vec<SequencedEvent> {
        self.lock().events.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn emit(&self, event: SessionEvent) -> u64 {
        let mut log = self.lock();
        let seq = log.next_seq;
        log.next_seq += 1;
        match &event {
            SessionEvent::AssistantMessage { .. } => log.events.retain(|e| {
                !matches!(
                    e.event,
                    SessionEvent::AssistantDelta { .. } | SessionEvent::ReasoningDelta { .. }
                )
            }),
            SessionEvent::ToolCallFinished { call_id, .. } => log.events.retain(|e| {
                !matches!(&e.event, SessionEvent::ToolOutput { call_id: id, .. } if id == call_id)
            }),
            SessionEvent::StateChanged { state } => log.state = *state,
            // A cleared session starts its log over with this event.
            SessionEvent::Cleared => {
                log.events.clear();
                if let Some((store, session)) = &self.store {
                    store.truncate(session, seq);
                }
            }
            _ => {}
        }
        let record = SequencedEvent { seq, event };
        if let Some((store, session)) = &self.store
            && !record.event.is_ephemeral()
        {
            store.append(session, &record);
        }
        log.events.push(record.clone());
        let observed = self.observer.get().map(|_| record.event.clone());
        let _ = self.tx.send(record);
        drop(log);
        if let (Some((session, observer)), Some(event)) = (self.observer.get(), observed) {
            observer.event(session, &event);
        }
        seq
    }

    pub fn set_state(&self, state: SessionState) {
        if self.state() != state {
            self.emit(SessionEvent::StateChanged { state });
        }
    }

    pub fn state(&self) -> SessionState {
        self.lock().state
    }

    pub fn last_seq(&self) -> u64 {
        self.lock().next_seq - 1
    }

    pub fn subscribe(&self, since_seq: Option<u64>) -> Subscription {
        self.subscribe_tail(since_seq, None)
    }

    /// Like [`EventHub::subscribe`], but a new window holds only the events
    /// of about the last `tail` conversation items. A subscriber that missed
    /// more than `tail` items since `since_seq` gets a new window too.
    pub fn subscribe_tail(&self, since_seq: Option<u64>, tail: Option<u32>) -> Subscription {
        let log = self.lock();
        let events = &log.events;
        let catch_up = since_seq
            .filter(|since| *since < log.next_seq)
            .map(|since| events.partition_point(|e| e.seq <= since))
            .filter(|from| match tail {
                Some(tail) => count_items(&events[*from..]) <= tail as usize,
                None => true,
            });
        let (backlog, window) = match catch_up {
            Some(from) => (events[from..].to_vec(), None),
            None => {
                let start = match tail {
                    Some(tail) => window_start(events, tail as usize, None),
                    None => 0,
                };
                let window = EventWindow {
                    has_older: start > 0,
                    usage: usage_of(&events[..start]),
                };
                (events[start..].to_vec(), Some(window))
            }
        };
        let live = self.tx.subscribe();
        drop(log);
        Subscription {
            backlog,
            window,
            live,
        }
    }

    /// The events of about `items` conversation items before `before_seq`.
    pub fn history(&self, before_seq: u64, items: u32) -> History {
        let log = self.lock();
        let end = log.events.partition_point(|e| e.seq < before_seq);
        let events = &log.events[..end];
        let start = window_start(events, items as usize, Some(MAX_HISTORY_BYTES));
        History {
            events: events[start..].to_vec(),
            has_older: start > 0,
            usage: usage_of(&events[..start]),
        }
    }
}

/// Whether a client shows `event` as a new conversation item. A window may
/// start at any of these: what follows one does not need what came before.
fn starts_item(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::UserMessage { .. }
            | SessionEvent::AssistantMessage { .. }
            | SessionEvent::ToolCallRequested { .. }
            | SessionEvent::Warning { .. }
            | SessionEvent::Error { .. }
            | SessionEvent::ContextCompacted { .. }
            | SessionEvent::ModelChanged { .. }
            | SessionEvent::Cleared
    )
}

fn count_items(events: &[SequencedEvent]) -> usize {
    events.iter().filter(|e| starts_item(&e.event)).count()
}

/// Where a window of about the last `items` items of `events` starts. It
/// holds at least one item, stops before `max_bytes` when given, and starts
/// early enough to hold the request of every tool call still in progress.
fn window_start(events: &[SequencedEvent], items: usize, max_bytes: Option<usize>) -> usize {
    let items = items.max(1);
    let mut start = 0;
    let mut count = 0;
    let mut bytes = 0;
    for (index, event) in events.iter().enumerate().rev() {
        if let Some(max) = max_bytes {
            bytes += loom_proto::encode(event).map_or(0, |b| b.len());
            if bytes > max && count > 0 {
                break;
            }
        }
        if starts_item(&event.event) {
            count += 1;
            start = index;
            if count == items {
                break;
            }
        }
    }
    if count < items && max_bytes.is_none_or(|max| bytes <= max) {
        return 0;
    }
    // A tool call that has not finished yet still changes; keep its request.
    let mut open = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        match &event.event {
            SessionEvent::ToolCallRequested { call } => {
                open.insert(call.call_id.as_str(), index);
            }
            SessionEvent::ToolCallFinished { call_id, .. } => {
                open.remove(call_id.as_str());
            }
            // A finished turn settles the calls it left open.
            SessionEvent::TurnFinished { .. } | SessionEvent::Cleared => open.clear(),
            _ => {}
        }
    }
    open.into_values().fold(start, usize::min)
}

fn usage_of(events: &[SequencedEvent]) -> UsageSoFar {
    let mut usage = UsageSoFar::default();
    for event in events {
        usage.apply(&event.event);
    }
    usage
}

#[cfg(test)]
mod tests {
    use loom_proto::{OutputChunk, OutputStream, ToolResult};

    use super::*;

    #[test]
    fn subscription_has_no_gaps() {
        let hub = EventHub::new();
        hub.emit(SessionEvent::Warning {
            message: "a".into(),
        });
        let mut sub = hub.subscribe(None);
        hub.emit(SessionEvent::Warning {
            message: "b".into(),
        });
        assert_eq!(sub.backlog.len(), 1);
        assert_eq!(sub.backlog[0].seq, 1);
        assert_eq!(sub.live.try_recv().unwrap().seq, 2);
        assert_eq!(hub.subscribe(Some(1)).backlog.len(), 1);
        assert_eq!(hub.last_seq(), 2);
    }

    #[test]
    fn compacts_ephemeral_events() {
        let hub = EventHub::new();
        hub.emit(SessionEvent::AssistantDelta { text: "a".into() });
        hub.emit(SessionEvent::ReasoningDelta { text: "r".into() });
        hub.emit(SessionEvent::AssistantMessage {
            text: "a".into(),
            reasoning: None,
            model: "m".into(),
        });
        hub.emit(SessionEvent::ToolOutput {
            call_id: "c".into(),
            chunk: OutputChunk {
                stream: OutputStream::Stdout,
                data: b"x".to_vec(),
            },
        });
        hub.emit(SessionEvent::ToolCallFinished {
            call_id: "c".into(),
            result: ToolResult {
                content: "x".into(),
                is_error: false,
                details: None,
            },
        });
        let seqs: Vec<u64> = hub.subscribe(None).backlog.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [3, 5]);
    }

    #[test]
    fn tracks_state() {
        let hub = EventHub::new();
        hub.set_state(SessionState::Idle);
        assert_eq!(hub.last_seq(), 0);
        hub.set_state(SessionState::Thinking);
        assert_eq!(hub.state(), SessionState::Thinking);
        assert_eq!(hub.last_seq(), 1);
    }

    fn user(hub: &EventHub, text: &str) {
        hub.emit(SessionEvent::UserMessage {
            content: vec![loom_proto::ContentPart::text(text)],
        });
    }

    fn usage(hub: &EventHub, input: u64) {
        hub.emit(SessionEvent::Usage {
            usage: loom_proto::Usage {
                input,
                ..Default::default()
            },
        });
    }

    fn tool(hub: &EventHub, id: &str) {
        hub.emit(SessionEvent::ToolCallRequested {
            call: loom_proto::ToolCall {
                call_id: id.into(),
                name: "shell".into(),
                arguments: "{}".into(),
                risk: None,
            },
        });
    }

    fn finish(hub: &EventHub, id: &str) {
        hub.emit(SessionEvent::ToolCallFinished {
            call_id: id.into(),
            result: ToolResult {
                content: "ok".into(),
                is_error: false,
                details: None,
            },
        });
    }

    fn items(events: &[SequencedEvent]) -> usize {
        count_items(events)
    }

    /// Ten turns of a prompt, a usage report and a state change.
    fn ten_turns() -> EventHub {
        let hub = EventHub::new();
        for turn in 0..10 {
            user(&hub, &format!("turn {turn}"));
            usage(&hub, 1);
            hub.set_state(if turn % 2 == 0 {
                SessionState::Thinking
            } else {
                SessionState::Idle
            });
        }
        hub
    }

    #[test]
    fn a_tail_holds_the_latest_items_and_the_usage_before_them() {
        let hub = ten_turns();
        let sub = hub.subscribe_tail(None, Some(3));
        assert_eq!(items(&sub.backlog), 3);
        assert!(matches!(
            &sub.backlog[0].event,
            SessionEvent::UserMessage { content } if content[0] == loom_proto::ContentPart::text("turn 7")
        ));
        let window = sub.window.unwrap();
        assert!(window.has_older);
        assert_eq!(window.usage.total.input, 7);
        assert_eq!(sub.backlog.last().unwrap().seq, hub.last_seq());

        // A tail longer than the session holds all of it.
        let all = hub.subscribe_tail(None, Some(50));
        assert_eq!(all.backlog, hub.events());
        assert!(!all.window.unwrap().has_older);
        // So does no tail at all.
        let all = hub.subscribe(None);
        assert_eq!(all.backlog, hub.events());
        assert_eq!(all.window.unwrap().usage, UsageSoFar::default());
    }

    #[test]
    fn a_short_gap_is_caught_up_and_a_long_one_starts_a_new_window() {
        let hub = ten_turns();
        let since = hub.last_seq();
        user(&hub, "more");
        usage(&hub, 1);
        let sub = hub.subscribe_tail(Some(since), Some(3));
        assert!(sub.window.is_none());
        assert_eq!(sub.backlog.len(), 2);
        assert_eq!(sub.backlog[0].seq, since + 1);

        let sub = hub.subscribe_tail(Some(3), Some(3));
        assert!(sub.window.unwrap().has_older);
        assert_eq!(items(&sub.backlog), 3);

        // Without a tail, any gap is caught up.
        let sub = hub.subscribe_tail(Some(3), None);
        assert!(sub.window.is_none());
        assert_eq!(sub.backlog[0].seq, 4);

        // A subscriber ahead of the log, as after the agent lost unsaved
        // events in a restart, starts over.
        let sub = hub.subscribe_tail(Some(hub.last_seq() + 5), Some(3));
        assert!(sub.window.is_some());
    }

    #[test]
    fn a_tail_keeps_the_requests_of_unfinished_tool_calls() {
        let hub = EventHub::new();
        user(&hub, "go");
        tool(&hub, "slow");
        for n in 0..5 {
            let id = format!("fast{n}");
            tool(&hub, &id);
            finish(&hub, &id);
        }
        let sub = hub.subscribe_tail(None, Some(2));
        assert!(matches!(
            &sub.backlog[0].event,
            SessionEvent::ToolCallRequested { call } if call.call_id == "slow"
        ));
        assert!(sub.window.unwrap().has_older);

        // Once the turn is over, the call is settled and the tail is short.
        hub.emit(SessionEvent::TurnFinished {
            outcome: loom_proto::TurnOutcome::Interrupted,
        });
        user(&hub, "next");
        let sub = hub.subscribe_tail(None, Some(2));
        assert_eq!(items(&sub.backlog), 2);
    }

    #[test]
    fn history_pages_back_to_the_start_without_gaps() {
        let hub = ten_turns();
        let sub = hub.subscribe_tail(None, Some(4));
        let mut seen = sub.backlog.clone();
        let mut has_older = sub.window.unwrap().has_older;
        while has_older {
            let page = hub.history(seen[0].seq, 4);
            assert!(!page.events.is_empty());
            assert_eq!(
                usage_of(&page.events).total.input + page.usage.total.input,
                {
                    let before: Vec<_> = hub
                        .events()
                        .into_iter()
                        .filter(|e| e.seq < seen[0].seq)
                        .collect();
                    usage_of(&before).total.input
                }
            );
            has_older = page.has_older;
            seen.splice(0..0, page.events);
        }
        assert_eq!(seen, hub.events());
    }

    #[test]
    fn a_history_page_stops_at_its_byte_budget() {
        let hub = EventHub::new();
        let big = "x".repeat(MAX_HISTORY_BYTES / 4 - 64);
        for _ in 0..6 {
            user(&hub, &big);
        }
        let page = hub.history(hub.last_seq() + 1, 50);
        assert_eq!(page.events.len(), 4);
        assert!(page.has_older);
        // One item larger than the budget still makes a page.
        let huge = "x".repeat(MAX_HISTORY_BYTES + 1);
        user(&hub, &huge);
        let page = hub.history(hub.last_seq() + 1, 50);
        assert_eq!(page.events.len(), 1);
    }
}
