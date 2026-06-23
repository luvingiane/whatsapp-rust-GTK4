//! Text helpers for the conversation view.

use gtk::glib;

/// Turns plain message text into Pango markup with clickable links. Non-URL text
/// is escaped; `http(s)://…` and `www.…` runs become `<a href>` anchors (a `www.`
/// link gets `https://` prepended in its href). Escaping is done per-span so URL
/// query separators (`&`) survive.
pub fn linkify(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    let mut plain_start = 0;
    // `i` always sits on a char boundary (we advance whole chars), so the byte
    // slices below are safe even with multibyte (accented/emoji) text.
    while i < text.len() {
        if let Some(end) = url_at(text, i) {
            // Flush the plain run before the URL.
            if plain_start < i {
                out.push_str(&glib::markup_escape_text(&text[plain_start..i]));
            }
            let url = &text[i..end];
            let esc = glib::markup_escape_text(url);
            let href = if url.starts_with("www.") {
                format!("https://{esc}")
            } else {
                esc.to_string()
            };
            out.push_str(&format!("<a href=\"{href}\">{esc}</a>"));
            i = end;
            plain_start = end;
        } else {
            i += text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
    }
    if plain_start < text.len() {
        out.push_str(&glib::markup_escape_text(&text[plain_start..]));
    }
    out
}

/// Extracts the URLs found in `text` (same recognition as [`linkify`]), in order.
pub fn find_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(end) = url_at(text, i) {
            out.push(text[i..end].to_string());
            i = end;
        } else {
            i += text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
    }
    out
}

/// If a URL starts at byte `start`, returns its exclusive end offset. Recognizes
/// `http://`, `https://` and `www.` prefixes; the URL runs until whitespace, with
/// common trailing punctuation trimmed.
fn url_at(text: &str, start: usize) -> Option<usize> {
    // Only start matching at a boundary (start of text or after whitespace).
    if start > 0 {
        let prev = text[..start].chars().next_back();
        if !matches!(prev, Some(c) if c.is_whitespace()) {
            return None;
        }
    }
    let rest = &text[start..];
    if !(rest.starts_with("http://") || rest.starts_with("https://") || rest.starts_with("www.")) {
        return None;
    }
    let mut end = start;
    for c in rest.chars() {
        if c.is_whitespace() {
            break;
        }
        end += c.len_utf8();
    }
    // Trim trailing punctuation that usually isn't part of the link.
    while end > start {
        let last = text[..end].chars().next_back().unwrap();
        if matches!(last, '.' | ',' | ')' | ']' | '}' | '!' | '?' | ':' | ';' | '"' | '\'') {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    // Need more than just the scheme.
    if &text[start..end] == "www." || end <= start + 4 {
        return None;
    }
    Some(end)
}
