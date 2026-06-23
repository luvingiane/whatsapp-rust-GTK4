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

/// A message being quoted in a reply: the quoted stanza id, its author JID (the
/// `participant`, empty for 1:1), and the quoted preview text.
#[derive(Debug, Clone)]
pub struct ReplyQuote {
    pub id: String,
    pub sender: String,
    pub body: String,
}

/// One media item for the profile gallery: `cached` is the on-disk path when the
/// file was already downloaded (else the UI shows `thumb` or a placeholder).
#[derive(Debug, Clone)]
pub struct MediaEntry {
    pub id: String,
    pub name: String,
    pub size: i64,
    pub thumb: Vec<u8>,
    pub cached: Option<String>,
}

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
    /// A voice note (message `id`) has been downloaded + decrypted to `path` (OGG)
    /// and is ready to play (requested via [`WaCommand::PlayAudio`]).
    AudioReady { id: String, path: String },
    /// A media file (message `id`, `kind` 1 image/2 video/4 document) has been
    /// downloaded + decrypted to `path` and is ready to open (via
    /// [`WaCommand::DownloadMedia`]).
    MediaReady { kind: i32, path: String },
    /// A photo (message `id`) downloaded for inline display is on disk at `path`.
    InlineReady { id: String, path: String },
    /// Profile info for the panel (requested via [`WaCommand::FetchProfile`]).
    /// `rows` is the common-groups list (1:1) or the participants list (group),
    /// each as `(jid, name, subtitle)` so rows are clickable. `status` is the user's
    /// about text (1:1) or the group description; `blocked` is the 1:1 block state.
    Profile {
        is_group: bool,
        jid: String,
        title: String,
        subtitle: String,
        status: String,
        pic_path: Option<String>,
        blocked: bool,
        rows: Vec<(String, String, String)>,
        /// Number of media items (photos/videos/documents) in the chat.
        media_count: usize,
    },
    /// The chat's media for the profile gallery (requested via
    /// [`WaCommand::FetchChatMedia`]), grouped by kind, plus extracted links.
    ChatMedia {
        jid: String,
        photos: Vec<MediaEntry>,
        videos: Vec<MediaEntry>,
        documents: Vec<MediaEntry>,
        links: Vec<String>,
    },
    /// Online presence for the open chat, shown under the header name. For a 1:1,
    /// `online_names` is `["online"]` when the contact is online (empty otherwise);
    /// for a group it lists the currently-online members (`total` = participant count).
    PresenceInfo {
        jid: String,
        is_group: bool,
        online_names: Vec<String>,
        total: usize,
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
    /// Announce our presence to WhatsApp: `true` = available (window focused/active),
    /// `false` = unavailable (unfocused or idle). When we never go unavailable the
    /// phone treats this device as active and withholds its own notifications.
    SetPresence { available: bool },
    /// Send a text message to `jid`, optionally quoting another message (`quote`).
    /// The backend inserts it optimistically and replies with [`WaEvent::NewMessage`].
    SendText {
        jid: String,
        text: String,
        quote: Option<ReplyQuote>,
    },
    /// Send a voice note (OGG/Opus bytes, `duration` seconds) to `jid`. The backend
    /// uploads it, sends an audio message (ptt), and replies with a NewMessage.
    SendAudio {
        jid: String,
        ogg: Vec<u8>,
        duration: u32,
        /// Amplitude waveform (0..100 per bar) computed while recording.
        waveform: Vec<u8>,
        /// Optional message being quoted (reply), like [`Self::SendText`].
        quote: Option<ReplyQuote>,
    },
    /// Download + decrypt a stored voice note for playback; the backend replies
    /// with [`WaEvent::AudioReady`] once the OGG is on disk.
    PlayAudio { chat_jid: String, id: String },
    /// Download + decrypt a stored media message (photo/video/document); the backend
    /// replies with [`WaEvent::MediaReady`] once the file is on disk.
    DownloadMedia { chat_jid: String, id: String },
    /// Lazily download a photo for inline display in its bubble (off the command
    /// loop, deduped + throttled); the backend replies with [`WaEvent::InlineReady`].
    LoadInline { chat_jid: String, id: String },
    /// Send an image to `jid` (raw bytes + MIME), optionally with a caption and a
    /// reply quote.
    SendImage {
        jid: String,
        data: Vec<u8>,
        mime: String,
        caption: Option<String>,
        quote: Option<ReplyQuote>,
    },
    /// Send a document (any non-image file) to `jid`.
    SendDocument {
        jid: String,
        data: Vec<u8>,
        mime: String,
        file_name: String,
        quote: Option<ReplyQuote>,
    },
    /// Fetch profile/group info for the header panel; the backend replies with
    /// [`WaEvent::Profile`].
    FetchProfile(String),
    /// Fetch the chat's media gallery (photos/videos/documents/links); the backend
    /// replies with [`WaEvent::ChatMedia`].
    FetchChatMedia(String),
    /// Archive (or unarchive) one or more chats (bulk selection). The backend sends
    /// the app-state mutation per chat and updates the local store.
    SetArchived { jids: Vec<String>, archived: bool },
    /// Pin (or unpin) a chat, syncing the app-state flag to WhatsApp.
    SetPinned { jid: String, pinned: bool },
    /// Block (or unblock) a contact.
    SetBlocked { jid: String, blocked: bool },
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
