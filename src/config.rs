//! Application-wide constants and filesystem paths.

use std::path::PathBuf;

use gtk::glib;

/// Reverse-DNS application id (used by GTK/libadwaita and, later, the .desktop file).
pub const APP_ID: &str = "io.github.matt.WhatsAppRustGtk";

/// Human-readable application name shown in logs and the window title.
pub const APP_NAME: &str = "WhatsApp (Rust/GTK4)";

/// OS string advertised to WhatsApp in the device registration props. Combined
/// with `PlatformType::Chrome` (set in the backend) it makes this client appear
/// as "Google Chrome (Linux)" in the phone's Linked Devices list, instead of an
/// "unknown device". This is sent only at pairing time, so changing it requires
/// re-pairing to take effect. It is cosmetic — it does not make us a real browser.
pub const DEVICE_OS: &str = "Linux";

/// Subdirectory under the XDG data dir where we keep all app state.
const DATA_SUBDIR: &str = "whatsapp-rust-gtk4";

/// File name of the whatsapp-rust SQLite session/keys database.
const SESSION_DB_FILE: &str = "whatsapp.db";

/// Returns the absolute path to the session database, creating its parent
/// directory if necessary. On Linux this resolves to
/// `~/.local/share/whatsapp-rust-gtk4/whatsapp.db`.
///
/// This database is owned by whatsapp-rust (`SqliteStore`) and holds the Signal
/// session, identity keys and device registration. It is the reason the app can
/// reconnect on restart without re-scanning the QR code.
pub fn session_db_path() -> anyhow::Result<PathBuf> {
    let mut dir = glib::user_data_dir();
    dir.push(DATA_SUBDIR);
    std::fs::create_dir_all(&dir)?;
    dir.push(SESSION_DB_FILE);
    Ok(dir)
}
