//! The chat media gallery (opened from the profile's "Media" row), shown in the
//! right side pane: tabs for Photos / Videos / Documents / Links.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use libadwaita as adw;

use crate::backend::MediaEntry;
use crate::ui::thread::{doc_icon, fmt_bytes, scaled_texture};

/// Shared map of `media id → thumbnail Picture` for tiles awaiting an on-demand
/// download, so an incoming `InlineReady` can fill the right tile.
pub type TileMap = Rc<RefCell<HashMap<String, gtk::Picture>>>;

/// Builds the tabbed gallery widget for the side pane. `on_open(id, kind)` fires
/// when a photo/video/document is clicked; `on_link(url)` when a link is clicked.
/// `on_need(id)` requests an on-demand download for a photo tile with nothing to
/// show; such tiles are registered in `tiles` so the caller can fill them later.
#[allow(clippy::too_many_arguments)]
pub fn build(
    photos: Vec<MediaEntry>,
    videos: Vec<MediaEntry>,
    documents: Vec<MediaEntry>,
    links: Vec<crate::model::LinkItem>,
    on_open: Rc<dyn Fn(String, i32)>,
    on_link: Rc<dyn Fn(String)>,
    on_need: Rc<dyn Fn(String)>,
    tiles: TileMap,
) -> gtk::Widget {
    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(
        &media_flow(photos, 1, on_open.clone(), on_need.clone(), tiles.clone()),
        Some("photos"),
        "Foto",
        "image-x-generic-symbolic",
    );
    stack.add_titled_with_icon(
        &media_flow(videos, 2, on_open.clone(), on_need.clone(), tiles.clone()),
        Some("videos"),
        "Video",
        "video-x-generic-symbolic",
    );
    stack.add_titled_with_icon(
        &document_list(documents, on_open.clone()),
        Some("docs"),
        "Documenti",
        "text-x-generic-symbolic",
    );
    stack.add_titled_with_icon(
        &link_list(links, on_link),
        Some("links"),
        "Link",
        "web-browser-symbolic",
    );

    // Narrow policy: the side pane is ~340 px, a Wide switcher wouldn't fit.
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Narrow)
        .build();
    switcher.set_margin_top(4);
    switcher.set_margin_start(6);
    switcher.set_margin_end(6);

    stack.set_vexpand(true);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.append(&switcher);
    root.append(&stack);
    root.upcast()
}

/// A scrollable grid of photo/video thumbnails (kind 1 photo / 2 video).
fn media_flow(
    items: Vec<MediaEntry>,
    kind: i32,
    on_open: Rc<dyn Fn(String, i32)>,
    on_need: Rc<dyn Fn(String)>,
    tiles: TileMap,
) -> gtk::Widget {
    if items.is_empty() {
        return empty_placeholder("Nessun elemento");
    }
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .row_spacing(6)
        .column_spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .min_children_per_line(3)
        .max_children_per_line(8)
        .build();
    for it in items {
        let pic = gtk::Picture::new();
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.add_css_class("media-thumb");
        // Show the cached full image if we have it (downscaled to keep RAM low).
        let cached_tex = it.cached.as_deref().and_then(|p| scaled_texture(p, 240));
        match cached_tex {
            Some(tex) => pic.set_paintable(Some(&tex)),
            None => {
                // Not downloaded yet: show the embedded thumbnail as a placeholder if
                // present, otherwise a loading tile.
                if !it.thumb.is_empty() {
                    if let Ok(tex) =
                        gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(&it.thumb))
                    {
                        pic.set_paintable(Some(&tex));
                    }
                } else {
                    pic.add_css_class("media-loading");
                }
                // Request the real photo on demand (throttled backend-side) and register
                // the tile so InlineReady replaces the placeholder. Photos only — the
                // inline loader is image-only. This also covers OLDER photos that only
                // had a thumbnail and were never fully downloaded.
                if kind == 1 {
                    tiles.borrow_mut().insert(it.id.clone(), pic.clone());
                    on_need(it.id.clone());
                }
            }
        }

        // Fixed 4:5 portrait tile. The size_request lives on the AspectFrame (not the
        // Picture) so the cell width is a fixed ~128 px — otherwise the homogeneous
        // FlowBox sizes every cell to the 240 px thumbnail texture, giving 2 huge
        // tiles per row with big gaps.
        let aspect = gtk::AspectFrame::new(0.5, 0.5, 0.8, false);
        aspect.set_size_request(128, 160);
        aspect.set_child(Some(&pic));
        let frame = gtk::Overlay::new();
        frame.set_child(Some(&aspect));
        if it.kind == 2 {
            let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
            play.set_pixel_size(36);
            play.add_css_class("media-play-overlay");
            play.set_halign(gtk::Align::Center);
            play.set_valign(gtk::Align::Center);
            play.set_can_target(false);
            frame.add_overlay(&play);
        } else if it.kind == 6 {
            let chip = gtk::Label::new(Some("GIF"));
            chip.add_css_class("media-time");
            chip.set_halign(gtk::Align::Center);
            chip.set_valign(gtk::Align::Center);
            chip.set_can_target(false);
            frame.add_overlay(&chip);
        }
        // File size chip on video/GIF tiles (photos keep a clean look).
        if matches!(it.kind, 2 | 6) && it.size > 0 {
            let size = gtk::Label::new(Some(&fmt_bytes(it.size)));
            size.add_css_class("media-time");
            size.set_halign(gtk::Align::End);
            size.set_valign(gtk::Align::End);
            size.set_margin_end(4);
            size.set_margin_bottom(4);
            size.set_can_target(false);
            frame.add_overlay(&size);
        }
        frame.set_cursor_from_name(Some("pointer"));
        {
            let on_open = on_open.clone();
            let id = it.id.clone();
            let item_kind = it.kind;
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| on_open(id.clone(), item_kind));
            frame.add_controller(click);
        }
        flow.append(&frame);
    }
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&flow)
        .build()
        .upcast()
}

/// A list of documents (icon by type + name + size · date), each opening on click.
fn document_list(items: Vec<MediaEntry>, on_open: Rc<dyn Fn(String, i32)>) -> gtk::Widget {
    if items.is_empty() {
        return empty_placeholder("Nessun documento");
    }
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for it in items {
        let mut sub = fmt_bytes(it.size);
        if let Some(date) = fmt_date(it.ts) {
            if sub.is_empty() {
                sub = date;
            } else {
                sub = format!("{sub} · {date}");
            }
        }
        let row = adw::ActionRow::builder()
            .title(if it.name.is_empty() { "Documento" } else { &it.name })
            .subtitle(&sub)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(doc_icon(&it.mime)));
        {
            let on_open = on_open.clone();
            let id = it.id.clone();
            row.connect_activated(move |_| on_open(id.clone(), 4));
        }
        list.append(&row);
    }
    wrap_list(list)
}

/// The shared links, richest first: a thumbnail/globe + title (or URL) + host
/// card per link, each opening in the browser on click.
fn link_list(links: Vec<crate::model::LinkItem>, on_link: Rc<dyn Fn(String)>) -> gtk::Widget {
    if links.is_empty() {
        return empty_placeholder("Nessun link");
    }
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for l in links {
        let row = adw::ActionRow::builder()
            .title(if l.title.is_empty() { &l.url } else { &l.title })
            .subtitle(host_of(&l.url))
            .activatable(true)
            .build();
        row.set_title_lines(2);
        if !l.desc.is_empty() {
            row.set_tooltip_text(Some(&l.desc));
        }
        let thumb_tex = (!l.thumb.is_empty())
            .then(|| gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(&l.thumb)).ok())
            .flatten();
        match thumb_tex {
            Some(tex) => {
                let pic = gtk::Picture::new();
                pic.set_size_request(48, 48);
                pic.set_content_fit(gtk::ContentFit::Cover);
                pic.set_paintable(Some(&tex));
                pic.add_css_class("media-thumb");
                row.add_prefix(&pic);
            }
            None => row.add_prefix(&gtk::Image::from_icon_name("web-browser-symbolic")),
        }
        {
            let on_link = on_link.clone();
            let url = l.url.clone();
            row.connect_activated(move |_| on_link(url.clone()));
        }
        list.append(&row);
    }
    wrap_list(list)
}

/// The host part of a URL (`https://a.b/c` → `a.b`), for link subtitles.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// A short `gg/mm/aa` date for a unix timestamp (`None` when unknown).
fn fmt_date(ts: i64) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    gtk::glib::DateTime::from_unix_local(ts)
        .ok()
        .and_then(|d| d.format("%d/%m/%y").ok())
        .map(|s| s.to_string())
}

fn wrap_list(list: gtk::ListBox) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.set_margin_top(8);
    b.set_margin_bottom(8);
    b.set_margin_start(8);
    b.set_margin_end(8);
    b.append(&list);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&b)
        .build()
        .upcast()
}

fn empty_placeholder(text: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    label.set_vexpand(true);
    label.set_valign(gtk::Align::Center);
    label.upcast()
}
