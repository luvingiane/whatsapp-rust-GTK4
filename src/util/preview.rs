//! Text helpers shared by the backend and UI: a one-line preview of a message,
//! and a friendly rendering of a JID as a phone number.

use wacore::proto_helpers::MessageExt;
use whatsapp_rust::waproto::whatsapp as wa;

/// A short, human one-line preview of a message for the chat list (and, later,
/// notifications). Plain text is returned verbatim; media and other kinds get a
/// labelled placeholder (with the caption appended when present).
pub fn message_preview(msg: &wa::Message) -> String {
    if let Some(text) = msg.text_content() {
        if !text.is_empty() {
            return text.to_string();
        }
    }

    if let Some(img) = &msg.image_message {
        return labelled("📷 Foto", img.caption.as_deref());
    }
    if let Some(v) = &msg.video_message {
        let label = if v.gif_playback.unwrap_or(false) {
            "🎞️ GIF"
        } else {
            "🎥 Video"
        };
        return labelled(label, v.caption.as_deref());
    }
    if let Some(a) = &msg.audio_message {
        return if a.ptt.unwrap_or(false) {
            "🎤 Messaggio vocale".to_string()
        } else {
            "🎵 Audio".to_string()
        };
    }
    if msg.sticker_message.is_some() {
        return "🩷 Sticker".to_string();
    }
    if let Some(doc) = &msg.document_message {
        let name = doc
            .file_name
            .as_deref()
            .or(doc.title.as_deref())
            .filter(|s| !s.is_empty())
            .unwrap_or("Documento");
        return format!("📄 {name}");
    }
    if msg.contact_message.is_some() || msg.contacts_array_message.is_some() {
        return "👤 Contatto".to_string();
    }
    if msg.location_message.is_some() || msg.live_location_message.is_some() {
        return "📍 Posizione".to_string();
    }
    if msg.poll_creation_message.is_some()
        || msg.poll_creation_message_v2.is_some()
        || msg.poll_creation_message_v3.is_some()
    {
        return "📊 Sondaggio".to_string();
    }

    String::new()
}

/// Joins a media label with its caption, e.g. `📷 Foto: ciao`.
fn labelled(label: &str, caption: Option<&str>) -> String {
    match caption {
        Some(c) if !c.is_empty() => format!("{label}: {c}"),
        _ => label.to_string(),
    }
}

/// Turns a raw JID like `393284448052:6@s.whatsapp.net` into a friendlier
/// `+393284448052` by dropping the device suffix and server, for display only.
pub fn pretty_number(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    let user = user.split(':').next().unwrap_or(user);
    format!("+{user}")
}
