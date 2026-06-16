//! The Tokio <-> GTK bridge: typed messages and the channels carrying them.
//!
//! whatsapp-rust delivers everything inside async callbacks running on the Tokio
//! runtime, while GTK requires all UI mutation to happen on the GLib main loop.
//! We therefore never touch widgets from the backend; instead we translate
//! whatsapp-rust `Event`s into the small [`WaEvent`] enum and push them across an
//! `async-channel`. The GTK side drains that channel with
//! `glib::spawn_future_local`, so UI updates always run on the main thread.

use async_channel::{Receiver, Sender};

use crate::model::ChatSummary;

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
}

/// Commands flowing **from** the GTK UI **to** the WhatsApp backend.
///
/// Only [`WaCommand::Shutdown`] exists today; sending messages, marking reads,
/// etc. will be added by later modules. The plumbing is wired now so those
/// modules only need to add variants and a match arm.
#[derive(Debug, Clone)]
pub enum WaCommand {
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
