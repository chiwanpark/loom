//! The client-side model of an agent's sessions, shared by the desktop and
//! mobile clients: a conversation built from session events, the files a
//! session changed, the grouping of sessions by what they are doing, and the
//! labels for session settings. It knows nothing of any UI toolkit; a client
//! picks how it holds a conversation's images.

pub mod agent_list;
pub mod changes;
pub mod conversation;
pub mod labels;
