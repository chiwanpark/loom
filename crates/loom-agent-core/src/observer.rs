//! Hooks that see every session event as it happens, for exporters such as
//! OpenTelemetry.

use std::sync::Arc;

use loom_proto::{SessionEvent, SessionId};

/// Receives the events of every session of an agent.
///
/// The methods run synchronously on the thread that produced the event, after
/// the event is recorded and with no session lock held, so they must be quick
/// and must not block.
pub trait SessionObserver: Send + Sync {
    /// A client created a new session. Sessions restored when the agent starts
    /// are not reported.
    fn session_created(&self, _session: &SessionId) {}

    /// An event of a live session, in order. Events replayed from storage are
    /// not reported.
    fn event(&self, session: &SessionId, event: &SessionEvent);
}

/// Several observers that see the same events, in order.
pub(crate) struct Observers(pub Vec<Arc<dyn SessionObserver>>);

impl SessionObserver for Observers {
    fn session_created(&self, session: &SessionId) {
        for observer in &self.0 {
            observer.session_created(session);
        }
    }

    fn event(&self, session: &SessionId, event: &SessionEvent) {
        for observer in &self.0 {
            observer.event(session, event);
        }
    }
}
