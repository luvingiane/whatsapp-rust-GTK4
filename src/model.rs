//! Plain domain types shared across the backend, store and UI layers.

/// A single row of the chat list, as shown in the sidebar. Produced by the
/// [`crate::store`] from the application DB and sent to the UI over the bridge.
#[derive(Debug, Clone)]
pub struct ChatSummary {
    /// Canonical chat JID (e.g. `393...@s.whatsapp.net` or `...@g.us`).
    pub jid: String,
    /// Display name (group subject / contact name / phone number fallback).
    pub name: String,
    /// Preview text of the most recent message.
    pub last_message: String,
    /// Unix timestamp (seconds) of the most recent message, for ordering.
    pub last_ts: i64,
    /// Whether the most recent message was sent by us.
    pub last_from_me: bool,
    /// Number of unread incoming messages.
    pub unread: u32,
    /// Whether this is a group chat.
    pub is_group: bool,
    /// Whether the chat is pinned (sorted above the rest).
    pub pinned: bool,
}
