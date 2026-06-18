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

/// File name of OUR application state database (chat list, later messages).
/// Kept separate from `whatsapp.db` so we never interfere with whatsapp-rust's
/// own diesel-managed schema/migrations.
const APP_DB_FILE: &str = "app.db";

/// Returns the absolute path to the session database, creating its parent
/// directory if necessary. On Linux this resolves to
/// `~/.local/share/whatsapp-rust-gtk4/whatsapp.db`.
///
/// This database is owned by whatsapp-rust (`SqliteStore`) and holds the Signal
/// session, identity keys and device registration. It is the reason the app can
/// reconnect on restart without re-scanning the QR code.
pub fn session_db_path() -> anyhow::Result<PathBuf> {
    data_file(SESSION_DB_FILE)
}

/// Returns the absolute path to OUR application database
/// (`~/.local/share/whatsapp-rust-gtk4/app.db`), creating the parent dir.
pub fn app_db_path() -> anyhow::Result<PathBuf> {
    data_file(APP_DB_FILE)
}

/// Resolves `<XDG data>/whatsapp-rust-gtk4/<file>`, creating the directory.
fn data_file(file: &str) -> anyhow::Result<PathBuf> {
    let mut dir = glib::user_data_dir();
    dir.push(DATA_SUBDIR);
    std::fs::create_dir_all(&dir)?;
    dir.push(file);
    Ok(dir)
}

/// Returns the cache path for a contact/group avatar, creating the avatars cache
/// directory. On Linux this resolves to
/// `~/.cache/whatsapp-rust-gtk4/avatars/<sanitized-jid>.jpg`. Profile pictures
/// are downloaded once and re-read from here, so they survive restarts and don't
/// re-hit the network. The JID is sanitized to a safe filename.
pub fn avatar_cache_path(jid: &str) -> anyhow::Result<PathBuf> {
    let mut dir = glib::user_cache_dir();
    dir.push(DATA_SUBDIR);
    dir.push("avatars");
    std::fs::create_dir_all(&dir)?;
    dir.push(format!("{}.jpg", sanitize(jid)));
    Ok(dir)
}

/// Maps a JID to a safe filename stem: every non-alphanumeric byte becomes `_`.
fn sanitize(jid: &str) -> String {
    jid.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
