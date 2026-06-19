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
    /// Delivery status of the last message when `last_from_me` (0 none, 1 sent,
    /// 2 delivered, 3 read), for the ✓/✓✓ glyph in the preview.
    pub last_status: i32,
}

/// A single message in a conversation, as shown in the thread view. Produced by
/// the [`crate::store`] and sent to the UI over the bridge.
#[derive(Debug, Clone)]
pub struct MessageRow {
    /// WhatsApp message id (unique within a chat).
    pub id: String,
    /// The chat this message belongs to.
    pub chat_jid: String,
    /// Sender JID (used to label the author in group chats).
    pub sender_jid: String,
    /// Resolved sender display name (saved name / pushname); empty if unknown,
    /// in which case the UI falls back to the number.
    pub sender_name: String,
    /// Whether we sent this message.
    pub from_me: bool,
    /// Unix timestamp (seconds).
    pub ts: i64,
    /// Display text: the message text, or a media placeholder ("📷 Foto", …).
    pub body: String,
    /// Delivery status for our own messages: 0 none/incoming, 1 sent (✓),
    /// 2 delivered (✓✓), 3 read/played (✓✓ blue).
    pub status: i32,
}
