//! The chat media gallery (opened from the profile's "Media" row): a tabbed
//! window with Photos / Videos / Documents / Links. One instance at a time.

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

/// Builds and presents the gallery over `parent`. `on_open(id, kind)` fires when a
/// photo/video/document is clicked; `on_link(url)` when a link is clicked.
/// `on_need(id)` requests an on-demand download for a photo tile with nothing to
/// show; such tiles are registered in `tiles` so the caller can fill them later.
#[allow(clippy::too_many_arguments)]
pub fn present(
    parent: &adw::ApplicationWindow,
    photos: Vec<MediaEntry>,
    videos: Vec<MediaEntry>,
    documents: Vec<MediaEntry>,
    links: Vec<String>,
    on_open: Rc<dyn Fn(String, i32)>,
    on_link: Rc<dyn Fn(String)>,
    on_need: Rc<dyn Fn(String)>,
    tiles: TileMap,
) -> adw::Window {
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

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));

    let tb = adw::ToolbarView::new();
    tb.add_top_bar(&header);
    tb.set_content(Some(&stack));

    let window = adw::Window::builder()
        .transient_for(parent)
        .default_width(640)
        .default_height(560)
        .title("Media")
        .content(&tb)
        .build();

    let keys = gtk::EventControllerKey::new();
    {
        let window = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(keys);

    window.present();
    window
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
        if kind == 2 {
            let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
            play.set_pixel_size(36);
            play.add_css_class("media-play-overlay");
            play.set_halign(gtk::Align::Center);
            play.set_valign(gtk::Align::Center);
            play.set_can_target(false);
            frame.add_overlay(&play);
        }
        frame.set_cursor_from_name(Some("pointer"));
        {
            let on_open = on_open.clone();
            let id = it.id.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| on_open(id.clone(), kind));
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

/// A list of documents (icon + name + size), each opening on click.
fn document_list(items: Vec<MediaEntry>, on_open: Rc<dyn Fn(String, i32)>) -> gtk::Widget {
    if items.is_empty() {
        return empty_placeholder("Nessun documento");
    }
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for it in items {
        let row = adw::ActionRow::builder()
            .title(if it.name.is_empty() { "Documento" } else { &it.name })
            .subtitle(&fmt_bytes(it.size))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(doc_icon("")));
        {
            let on_open = on_open.clone();
            let id = it.id.clone();
            row.connect_activated(move |_| on_open(id.clone(), 4));
        }
        list.append(&row);
    }
    wrap_list(list)
}

/// A list of links, each opening in the browser on click.
fn link_list(links: Vec<String>, on_link: Rc<dyn Fn(String)>) -> gtk::Widget {
    if links.is_empty() {
        return empty_placeholder("Nessun link");
    }
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for url in links {
        let row = adw::ActionRow::builder().title(&url).activatable(true).build();
        row.add_prefix(&gtk::Image::from_icon_name("web-browser-symbolic"));
        {
            let on_link = on_link.clone();
            let url = url.clone();
            row.connect_activated(move |_| on_link(url.clone()));
        }
        list.append(&row);
    }
    wrap_list(list)
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
