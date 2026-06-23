//! The contact / group profile panel, shown when the conversation header is
//! clicked: a large picture, name + status, a Media placeholder, a block button
//! (1:1), and the list of groups in common (1:1) or participants (group). Each
//! row is clickable to open that participant's / group's profile.

use std::rc::Rc;

use adw::prelude::*;
use libadwaita as adw;

use super::AvatarCache;

/// Everything the panel needs to render, mirroring [`crate::backend::WaEvent::Profile`].
pub struct ProfileData {
    pub is_group: bool,
    pub jid: String,
    pub title: String,
    pub subtitle: String,
    pub status: String,
    pub pic_path: Option<String>,
    pub blocked: bool,
    /// `(jid, name, subtitle)` for each participant (group) / common group (1:1).
    pub rows: Vec<(String, String, String)>,
    /// Number of media items (photos/videos/documents) in the chat.
    pub media_count: usize,
}

/// Builds and presents the profile window over `parent`, returning the window so
/// the caller can keep a single instance (closing any previous one).
///
/// `on_open(jid)` fires when a participant / common-group row is clicked;
/// `on_block(jid, want_blocked)` fires when the block button is toggled.
pub fn present(
    parent: &adw::ApplicationWindow,
    avatars: &AvatarCache,
    data: ProfileData,
    on_open: Rc<dyn Fn(String)>,
    on_block: Rc<dyn Fn(String, bool)>,
    on_media: Rc<dyn Fn(String)>,
) -> adw::Window {
    let avatar = adw::Avatar::new(128, Some(&data.title), true);
    avatar.set_halign(gtk::Align::Center);
    if let Some(path) = data.pic_path.as_deref() {
        if let Ok(tex) = gtk::gdk::Texture::from_filename(path) {
            avatar.set_custom_image(Some(&tex));
        }
    }

    let title_lbl = gtk::Label::new(Some(&data.title));
    title_lbl.add_css_class("title-2");
    title_lbl.set_wrap(true);
    title_lbl.set_justify(gtk::Justification::Center);
    let sub_lbl = gtk::Label::new(Some(&data.subtitle));
    sub_lbl.add_css_class("dim-label");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&avatar);
    content.append(&title_lbl);
    content.append(&sub_lbl);

    // Status / about (1:1) or group description, shown under the name.
    if !data.status.is_empty() {
        let status = gtk::Label::new(Some(&data.status));
        status.set_wrap(true);
        status.set_justify(gtk::Justification::Center);
        status.add_css_class("body");
        content.append(&status);
    }

    // Media: opens the chat's gallery (photos/videos/documents/links).
    let media = adw::ActionRow::builder()
        .title("Media")
        .subtitle(if data.media_count > 0 {
            format!("{} elementi", data.media_count)
        } else {
            "—".to_string()
        })
        .activatable(data.media_count > 0)
        .build();
    media.add_prefix(&gtk::Image::from_icon_name("image-x-generic-symbolic"));
    media.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let jid = data.jid.clone();
        let on_media = on_media.clone();
        media.connect_activated(move |_| on_media(jid.clone()));
    }
    let media_list = gtk::ListBox::new();
    media_list.add_css_class("boxed-list");
    media_list.set_selection_mode(gtk::SelectionMode::None);
    media_list.append(&media);
    content.append(&media_list);

    // Groups in common (1:1) or participants (group).
    let section = gtk::Label::new(Some(if data.is_group {
        "Partecipanti"
    } else {
        "Gruppi in comune"
    }));
    section.set_xalign(0.0);
    section.add_css_class("heading");
    section.set_margin_top(6);
    content.append(&section);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    if data.rows.is_empty() {
        let empty = adw::ActionRow::builder()
            .title(if data.is_group {
                "—"
            } else {
                "Nessun gruppo in comune"
            })
            .build();
        list.append(&empty);
    } else {
        let cache = avatars.borrow();
        for (jid, name, sub) in &data.rows {
            let row = adw::ActionRow::builder().title(name).activatable(true).build();
            if !sub.is_empty() {
                row.set_subtitle(sub);
            }
            let row_avatar = adw::Avatar::new(36, Some(name), true);
            if let Some(tex) = cache.get(jid) {
                row_avatar.set_custom_image(Some(tex));
            }
            row.add_prefix(&row_avatar);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                let jid = jid.clone();
                let on_open = on_open.clone();
                row.connect_activated(move |_| on_open(jid.clone()));
            }
            list.append(&row);
        }
    }
    content.append(&list);

    // Block / unblock (1:1 only): a small, strong-red button at the very bottom.
    // Blocking asks for confirmation first; unblocking is immediate.
    if !data.is_group {
        let blocked = Rc::new(std::cell::Cell::new(data.blocked));
        let btn = gtk::Button::new();
        btn.set_halign(gtk::Align::Center);
        btn.set_margin_top(18);
        btn.add_css_class("pill");
        btn.add_css_class("destructive-action");
        btn.add_css_class("block-btn");
        let relabel = |b: bool, btn: &gtk::Button| {
            btn.set_label(if b { "Sblocca" } else { "Blocca" });
        };
        relabel(data.blocked, &btn);
        {
            let jid = data.jid.clone();
            let name = data.title.clone();
            let on_block = on_block.clone();
            let btn_w = btn.clone();
            let blocked = blocked.clone();
            btn.connect_clicked(move |b| {
                // Unblock: act at once. Block: confirm with a destructive dialog.
                if blocked.get() {
                    blocked.set(false);
                    relabel(false, &btn_w);
                    on_block(jid.clone(), false);
                    return;
                }
                let parent = b.root().and_downcast::<gtk::Window>();
                let dialog = adw::MessageDialog::new(
                    parent.as_ref(),
                    Some(&format!("Bloccare {name}?")),
                    Some("Non potrà più inviarti messaggi né vedere i tuoi aggiornamenti."),
                );
                dialog.add_responses(&[("cancel", "Annulla"), ("block", "Blocca")]);
                dialog.set_response_appearance("block", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                {
                    let jid = jid.clone();
                    let on_block = on_block.clone();
                    let btn_w = btn_w.clone();
                    let blocked = blocked.clone();
                    dialog.connect_response(None, move |_, resp| {
                        if resp == "block" {
                            blocked.set(true);
                            btn_w.set_label("Sblocca");
                            on_block(jid.clone(), true);
                        }
                    });
                }
                dialog.present();
            });
        }
        content.append(&btn);
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();

    let tb = adw::ToolbarView::new();
    tb.add_top_bar(&adw::HeaderBar::new());
    tb.set_content(Some(&scrolled));

    let window = adw::Window::builder()
        // Non-modal so the media gallery / viewer opened from here stay interactive
        // alongside the profile (Esc still closes it).
        .modal(false)
        .transient_for(parent)
        .default_width(360)
        .default_height(560)
        .title(&data.title)
        .content(&tb)
        .build();

    // Esc closes the profile (a second Esc, handled by the main window, then
    // closes the chat).
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
