//! The "Tokio side" of the app: everything that talks to whatsapp-rust.
//!
//! - [`bridge`]: the typed messages and channels between Tokio and GTK.
//! - [`runtime`]: the dedicated Tokio thread.
//! - [`client`]: building and driving the whatsapp-rust bot.

pub mod bridge;
pub mod client;
pub mod runtime;

pub use bridge::{channels, MediaEntry, ReplyQuote, WaCommand, WaEvent};
