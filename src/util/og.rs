//! Open Graph metadata fetch for outgoing link previews. Backend-side (no GTK
//! widgets): plain string scanning over capped HTML, no HTML-parser dependency.
//! gdk-pixbuf is only used for pure image decode/re-encode, which is safe off
//! the GTK main thread.

use std::time::Duration;

/// Caps hostile/huge pages: how much HTML we read looking for meta tags, and
/// the largest preview image we download.
const HTML_CAP: u64 = 512 * 1024;
const IMAGE_CAP: u64 = 1024 * 1024;
/// Long edge of the re-encoded preview thumbnail (small enough to embed in the
/// message like the official clients do).
const THUMB_MAX: i32 = 200;

pub struct OgData {
    pub title: String,
    pub description: String,
    /// Re-encoded JPEG thumbnail (empty if the page has no usable image).
    pub thumb_jpeg: Vec<u8>,
}

/// Fetches `url` and extracts Open Graph title/description/image (with `<title>`
/// as fallback). Returns `None` when the page yields neither a title nor a
/// description — a preview with only a URL is not worth sending. Blocking
/// (ureq): call from `spawn_blocking`.
pub fn fetch_og(url: &str) -> Option<OgData> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let agent = agent();
    let html_bytes = get_limited(&agent, url, HTML_CAP)?;
    let html = String::from_utf8_lossy(&html_bytes).into_owned();
    let lower = html.to_ascii_lowercase();

    let title = meta_content(&html, &lower, "og:title")
        .or_else(|| html_title(&html, &lower))
        .unwrap_or_default();
    let description = meta_content(&html, &lower, "og:description")
        .or_else(|| meta_content(&html, &lower, "description"))
        .unwrap_or_default();
    if title.is_empty() && description.is_empty() {
        return None;
    }

    let thumb_jpeg = meta_content(&html, &lower, "og:image")
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .and_then(|img_url| get_limited(&agent, &img_url, IMAGE_CAP))
        .and_then(|bytes| thumb_from_bytes(&bytes))
        .unwrap_or_default();

    Some(OgData {
        title,
        description,
        thumb_jpeg,
    })
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build(),
    )
}

/// GET `url`, reading at most `cap` bytes of body. `None` on any failure.
fn get_limited(agent: &ureq::Agent, url: &str, cap: u64) -> Option<Vec<u8>> {
    let mut resp = agent.get(url).call().ok()?;
    resp.body_mut().with_config().limit(cap).read_to_vec().ok()
}

/// Decodes an image and re-encodes it as a small JPEG (long edge ≤ THUMB_MAX),
/// so only a tiny blob is embedded in the message / stored in the DB.
fn thumb_from_bytes(data: &[u8]) -> Option<Vec<u8>> {
    use gtk::gdk_pixbuf::prelude::*;
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader.connect_size_prepared(|l, w, h| {
        let scale = f64::from(THUMB_MAX) / f64::from(w.max(h).max(1));
        if scale < 1.0 {
            l.set_size(
                (f64::from(w) * scale).round() as i32,
                (f64::from(h) * scale).round() as i32,
            );
        }
    });
    if loader.write(data).is_err() {
        let _ = loader.close();
        return None;
    }
    loader.close().ok()?;
    let pb = loader.pixbuf()?;
    pb.save_to_bufferv("jpeg", &[("quality", "80")])
        .ok()
        .map(|b| b.to_vec())
}

/// The `content` of the first `<meta>` whose property/name equals `key`.
/// `lower` is the ASCII-lowercased copy of `html` (same byte offsets).
fn meta_content(html: &str, lower: &str, key: &str) -> Option<String> {
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<meta") {
        let start = pos + i;
        let end = start + lower[start..].find('>')?;
        let tag = &html[start..=end];
        let tag_l = &lower[start..=end];
        let is_key = [
            format!("property=\"{key}\""),
            format!("property='{key}'"),
            format!("name=\"{key}\""),
            format!("name='{key}'"),
        ]
        .iter()
        .any(|n| tag_l.contains(n.as_str()));
        if is_key {
            if let Some(v) = attr_value(tag, tag_l, "content") {
                let v = decode_entities(&v);
                if !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
        pos = end + 1;
    }
    None
}

/// A quoted attribute's value inside one tag.
fn attr_value(tag: &str, tag_l: &str, name: &str) -> Option<String> {
    let i = tag_l.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[i..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &rest[1..];
    Some(inner[..inner.find(quote)?].to_string())
}

/// The document `<title>` text.
fn html_title(html: &str, lower: &str) -> Option<String> {
    let start = lower.find("<title")?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</title")?;
    let t = decode_entities(html.get(open_end..close)?);
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Decodes the handful of entities that actually show up in titles/descriptions.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}
