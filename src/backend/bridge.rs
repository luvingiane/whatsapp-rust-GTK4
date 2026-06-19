//! The Tokio <-> GTK bridge: typed messages and the channels carrying them.
//!
//! whatsapp-rust delivers everything inside async callbacks running on the Tokio
//! runtime, while GTK requires all UI mutation to happen on the GLib main loop.
//! We therefore never touch widgets from the backend; instead we translate
//! whatsapp-rust `Event`s into the small [`WaEvent`] enum and push them across an
//! `async-channel`. The GTK side drains that channel with
//! `glib::spawn_future_local`, so UI updates always run on the main thread.

use async_channel::{Receiver, Sender};

use crate::model::{ChatSummary, MessageRow};

/// Events flowing **from** the WhatsApp backend (Tokio thread) **to** the GTK UI.
#[derive(Debug, Clone)]
pub enum WaEvent {
    /// A fresh QR pairing string to render. whatsapp-rust re-emits this on
    /// expiry, so each one simply replaces the previously shown code.
    QrCode(String),
    /// First-time pairing completed; carries our own JID for display.
    PairSuccess { jid: String },
    /// Connected or reconnected. `jid` is our own number if already known
    /// (it is `None` only in the brief window before the device is registered).
    Connected { jid: Option<String> },
    /// Socket dropped; whatsapp-rust will attempt to reconnect on its own.
    Disconnected,
    /// Session was invalidated server-side; the user must scan a new QR code.
    LoggedOut,
    /// A non-fatal backend problem worth surfacing to the user.
    Error(String),
    /// The full, ordered chat list (sidebar). Sent after history sync and,
    /// debounced, whenever the store changes.
    ChatsSnapshot(Vec<ChatSummary>),
    /// The full, ordered archived chat list. Sent alongside [`Self::ChatsSnapshot`]
    /// so the archived view and its count stay in sync with the active list.
    ArchivedChatsSnapshot(Vec<ChatSummary>),
    /// The message history for a chat the UI requested via [`WaCommand::OpenChat`],
    /// oldest-first.
    ChatHistory {
        jid: String,
        messages: Vec<MessageRow>,
    },
    /// A single new/live message persisted to the store. The UI appends it if the
    /// matching chat is currently open.
    NewMessage(MessageRow),
    /// An older page of history for a chat, requested via [`WaCommand::LoadOlder`],
    /// oldest-first. The UI prepends it (preserving scroll); an empty `messages`
    /// means the local history has been exhausted.
    OlderHistory {
        jid: String,
        messages: Vec<MessageRow>,
    },
    /// A profile picture is available on disk for `jid` (requested via
    /// [`WaCommand::FetchAvatar`]). The UI loads it into a `gdk::Texture`.
    Avatar { jid: String, path: String },
    /// A delivery/read receipt advanced the status of one or more of our sent
    /// messages (1 sent, 2 delivered, 3 read). The UI updates the ✓/✓✓ glyph on
    /// the matching bubbles if the chat is open.
    ReceiptUpdate {
        chat_jid: String,
        message_ids: Vec<String>,
        status: i32,
    },
}

/// Commands flowing **from** the GTK UI **to** the WhatsApp backend.
///
/// Only [`WaCommand::Shutdown`] exists today; sending messages, marking reads,
/// etc. will be added by later modules. The plumbing is wired now so those
/// modules only need to add variants and a match arm.
#[derive(Debug, Clone)]
pub enum WaCommand {
    /// Load a chat's message history; the backend replies with
    /// [`WaEvent::ChatHistory`].
    OpenChat(String),
    /// Load the page of messages older than the keyset cursor `(before_ts,
    /// before_id)` for `jid`; the backend replies with [`WaEvent::OlderHistory`].
    LoadOlder {
        jid: String,
        before_ts: i64,
        before_id: String,
        count: i64,
    },
    /// Fetch (download + disk-cache) the profile picture for `jid`; the backend
    /// replies with [`WaEvent::Avatar`] if one is available.
    FetchAvatar(String),
    /// Ask the backend to stop its run loop (sent when the window closes).
    Shutdown,
}

/// Both channel ends, created together so the wiring stays in one place.
pub struct Channels {
    pub event_tx: Sender<WaEvent>,
    pub event_rx: Receiver<WaEvent>,
    pub command_tx: Sender<WaCommand>,
    pub command_rx: Receiver<WaCommand>,
}

/// Creates the event (backend -> UI) and command (UI -> backend) channels.
pub fn channels() -> Channels {
    let (event_tx, event_rx) = async_channel::unbounded();
    let (command_tx, command_rx) = async_channel::unbounded();
    Channels {
        event_tx,
        event_rx,
        command_tx,
        command_rx,
    }
}
