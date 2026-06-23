//! Application-wide constants and filesystem paths.

use std::path::PathBuf;

use gtk::glib;

/// Reverse-DNS application id (used by GTK/libadwaita and, later, the .desktop file).
pub const APP_ID: &str = "io.github.matt.WhatsAppRustGtk";

/// Human-readable application name shown in logs and the window title.
pub const APP_NAME: &str = "WhatsApp (Rust/GTK4)";

/// When `true`, never paint the blue (read) tick on **our own** sent messages —
/// they stop at ✓✓ (delivered). This mirrors having "read receipts" turned OFF in
/// WhatsApp's privacy settings: the official apps suppress the blue tick client-side
/// in that case, but the server still delivers the read receipt, so we must hide it
/// here too. Display-only: the stored status is untouched. Flip to `false` to show
/// blue read ticks again.
pub const HIDE_READ_RECEIPTS: bool = true;

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

/// File holding the persisted sidebar (chat list) width in pixels, so the
/// resizable split is restored across restarts.
const SIDEBAR_WIDTH_FILE: &str = "sidebar_width";

/// Reads the persisted sidebar width, or `None` if never set / unreadable.
pub fn read_sidebar_width() -> Option<i32> {
    let path = data_file(SIDEBAR_WIDTH_FILE).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Persists the sidebar width (best effort; errors are ignored).
pub fn write_sidebar_width(width: i32) {
    if let Ok(path) = data_file(SIDEBAR_WIDTH_FILE) {
        let _ = std::fs::write(path, width.to_string());
    }
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

/// Returns the cache path for a downloaded media file (photo/video/document),
/// creating the media cache directory
/// (`~/.cache/whatsapp-rust-gtk4/media/<sanitized-id>.<ext>`). Decrypted media is
/// cached so re-opening it doesn't re-hit the network.
pub fn media_cache_path(id: &str, ext: &str) -> anyhow::Result<PathBuf> {
    let mut dir = glib::user_cache_dir();
    dir.push(DATA_SUBDIR);
    dir.push("media");
    std::fs::create_dir_all(&dir)?;
    dir.push(format!("{}.{}", sanitize(id), ext));
    Ok(dir)
}

/// Returns the cache path for a downloaded voice note, creating the audio cache
/// directory (`~/.cache/whatsapp-rust-gtk4/audio/<sanitized-id>.ogg`). Decrypted
/// notes are cached so replaying one doesn't re-hit the network.
pub fn audio_cache_path(id: &str) -> anyhow::Result<PathBuf> {
    let mut dir = glib::user_cache_dir();
    dir.push(DATA_SUBDIR);
    dir.push("audio");
    std::fs::create_dir_all(&dir)?;
    dir.push(format!("{}.ogg", sanitize(id)));
    Ok(dir)
}

/// Returns the cache path for a full-res profile picture (separate from the small
/// avatar cache), creating the directory
/// (`~/.cache/whatsapp-rust-gtk4/profile/<sanitized-jid>.jpg`).
pub fn profile_pic_path(jid: &str) -> anyhow::Result<PathBuf> {
    let mut dir = glib::user_cache_dir();
    dir.push(DATA_SUBDIR);
    dir.push("profile");
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
