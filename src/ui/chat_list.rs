//! The chat list sidebar: a virtualized `ListView` over a `ListStore` of
//! [`ChatObject`]s, rebuilt from a [`ChatSummary`] snapshot on each update.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use libadwaita as adw;

use super::chat_object::ChatObject;
use super::AvatarCache;
use crate::model::ChatSummary;

/// Key under which each row's avatar property-binding is stashed on its
/// `ListItem`, so it can be torn down when the row is recycled.
const AVATAR_BINDING_KEY: &str = "wrg-avatar-binding";

type NeedAvatarCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
type OpenArchivedCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;
/// `(jids, archived)`: archive (or unarchive) the selected chats in bulk.
type ArchiveCb = Rc<RefCell<Option<Box<dyn Fn(Vec<String>, bool)>>>>;
/// `(jid, pinned)`: pin or unpin a single chat.
type PinCb = Rc<RefCell<Option<Box<dyn Fn(String, bool)>>>>;

/// Key under which each row's right-click menu gesture is stashed on its
/// `ListItem`, so it can be removed when the row is recycled.
const MENU_GESTURE_KEY: &str = "wrg-menu-gesture";

/// Collects the JIDs of every selected row in a multi-selection.
fn collect_selected(sel: &gtk::MultiSelection) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..sel.n_items() {
        if sel.is_selected(i) {
            if let Some(o) = sel.item(i).and_downcast::<ChatObject>() {
                out.push(o.jid());
            }
        }
    }
    out
}

#[derive(Clone)]
pub struct ChatList {
    pub root: gtk::Box,
    store: gio::ListStore,
    list_view: gtk::ListView,
    /// Shared decoded-texture cache (filled by [`Self::set_avatar`]).
    avatars: AvatarCache,
    /// Invoked with a JID when a visible row still lacks its avatar.
    on_need_avatar: NeedAvatarCb,
    /// "Archiviate" entry shown above the list (only on the active list); its
    /// count label and click callback. `None` on the archived list itself.
    archived_entry: Option<(gtk::Button, gtk::Label, OpenArchivedCb)>,
    /// Bulk archive/unarchive callback (from the row right-click menu).
    on_archive: ArchiveCb,
    /// Pin/unpin callback (from the row right-click menu).
    on_pin: PinCb,
    /// The selection model (for clearing the selection externally).
    selection: gtk::MultiSelection,
    /// Whether selection mode is active.
    selecting: Rc<Cell<bool>>,
    /// Leaves selection mode (used by the Esc handler).
    exit_sel: Rc<dyn Fn()>,
}

impl ChatList {
    /// Builds a chat list. `with_archived_button` adds the "Archiviate" entry on
    /// top (used for the active list; the archived view passes `false`).
    pub fn new(avatars: &AvatarCache, with_archived_button: bool) -> Self {
        let store = gio::ListStore::new::<ChatObject>();
        let avatars = avatars.clone();
        let on_need_avatar: NeedAvatarCb = Rc::new(RefCell::new(None));

        // Live name/number filter driven by the search entry below.
        let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let filter = {
            let query = query.clone();
            gtk::CustomFilter::new(move |obj| {
                let q = query.borrow();
                if q.is_empty() {
                    return true;
                }
                obj.downcast_ref::<ChatObject>().is_some_and(|c| {
                    c.name().to_lowercase().contains(q.as_str()) || c.jid().contains(q.as_str())
                })
            })
        };
        let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        // MultiSelection gives ctrl+click / ctrl+a / shift+click / shift+arrows for
        // free; a plain single click still activates (opens) via `single_click_activate`.
        let selection = gtk::MultiSelection::new(Some(filter_model));

        // Bulk archive + pin callbacks, invoked from each row's right-click menu.
        let on_archive: ArchiveCb = Rc::new(RefCell::new(None));
        let on_pin: PinCb = Rc::new(RefCell::new(None));
        // Active list archives (true); the archived view unarchives (false).
        let archived_view = !with_archived_button;
        // Selection-mode state: when on, clicks select instead of opening chats.
        let selecting = Rc::new(Cell::new(false));
        // Late-bound "enter selection mode for this jid", filled once the list view
        // and action bar exist (the row menu in `connect_bind` calls it).
        type EnterSelCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
        let enter_sel: EnterSelCb = Rc::new(RefCell::new(None));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&build_row()));
            }
        });
        {
            let on_need_avatar = on_need_avatar.clone();
            let selection_m = selection.clone();
            let on_archive = on_archive.clone();
            let on_pin = on_pin.clone();
            let enter_sel = enter_sel.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(obj) = item.item().and_downcast::<ChatObject>() else {
                    return;
                };
                let Some(row) = item.child().and_downcast::<gtk::Box>() else {
                    return;
                };
                bind_row(&row, &obj);

                // Drive the Avatar's image from the object's `avatar` property so
                // a late-arriving download updates this (recycled) row live.
                if let Some((avatar, ..)) = row_widgets(&row) {
                    let binding = obj
                        .bind_property("avatar", &avatar, "custom-image")
                        .sync_create()
                        .build();
                    unsafe { item.set_data(AVATAR_BINDING_KEY, binding) };
                }
                // No picture yet → ask the backend to fetch it.
                if obj.avatar().is_none() {
                    if let Some(cb) = on_need_avatar.borrow().as_ref() {
                        cb(obj.jid());
                    }
                }

                // Right-click context menu (Archivia / preferiti). Acts on the whole
                // selection if the clicked row is part of it, else on this row alone.
                let menu = gtk::GestureClick::new();
                menu.set_button(gtk::gdk::BUTTON_SECONDARY);
                {
                    let selection_m = selection_m.clone();
                    let on_archive = on_archive.clone();
                    let on_pin = on_pin.clone();
                    let enter_sel = enter_sel.clone();
                    let row = row.clone();
                    let jid = obj.jid();
                    let pinned = obj.pinned();
                    menu.connect_pressed(move |g, _, x, y| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let mut targets = collect_selected(&selection_m);
                        if !targets.iter().any(|j| j == &jid) {
                            targets = vec![jid.clone()];
                        }
                        let pop = gtk::Popover::new();
                        pop.set_has_arrow(false);
                        pop.set_parent(&row);
                        pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                            x as i32, y as i32, 1, 1,
                        )));
                        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
                        let archive_lbl = if archived_view {
                            "Sposta dalle archiviate"
                        } else if targets.len() > 1 {
                            "Archivia selezionate"
                        } else {
                            "Archivia"
                        };
                        let arch = gtk::Button::with_label(archive_lbl);
                        arch.add_css_class("flat");
                        arch.set_halign(gtk::Align::Start);
                        {
                            let on_archive = on_archive.clone();
                            let pop = pop.clone();
                            let targets = targets.clone();
                            arch.connect_clicked(move |_| {
                                if let Some(cb) = on_archive.borrow().as_ref() {
                                    cb(targets.clone(), !archived_view);
                                }
                                pop.popdown();
                            });
                        }
                        bx.append(&arch);
                        let pin = gtk::Button::with_label(if pinned {
                            "Rimuovi dai preferiti"
                        } else {
                            "Aggiungi ai preferiti"
                        });
                        pin.add_css_class("flat");
                        pin.set_halign(gtk::Align::Start);
                        {
                            let on_pin = on_pin.clone();
                            let pop = pop.clone();
                            let jid = jid.clone();
                            pin.connect_clicked(move |_| {
                                if let Some(cb) = on_pin.borrow().as_ref() {
                                    cb(jid.clone(), !pinned);
                                }
                                pop.popdown();
                            });
                        }
                        bx.append(&pin);
                        let select = gtk::Button::with_label("Seleziona chat");
                        select.add_css_class("flat");
                        select.set_halign(gtk::Align::Start);
                        {
                            let enter_sel = enter_sel.clone();
                            let pop = pop.clone();
                            let jid = jid.clone();
                            select.connect_clicked(move |_| {
                                if let Some(f) = enter_sel.borrow().as_ref() {
                                    f(jid.clone());
                                }
                                pop.popdown();
                            });
                        }
                        bx.append(&select);
                        pop.set_child(Some(&bx));
                        pop.connect_closed(|p| p.unparent());
                        pop.popup();
                    });
                }
                row.add_controller(menu.clone());
                unsafe { item.set_data(MENU_GESTURE_KEY, menu) };
            });
        }
        factory.connect_unbind(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let Some(binding) = unsafe { item.steal_data::<glib::Binding>(AVATAR_BINDING_KEY) } {
                binding.unbind();
            }
            if let Some(menu) = unsafe { item.steal_data::<gtk::GestureClick>(MENU_GESTURE_KEY) } {
                if let Some(row) = item.child() {
                    row.remove_controller(&menu);
                }
            }
        });

        let list_view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .single_click_activate(true)
            .build();
        list_view.add_css_class("navigation-sidebar");

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_view)
            .build();

        // Search bar above the list: filters chats by name or number.
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Cerca chat")
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        search.connect_search_changed(move |entry| {
            *query.borrow_mut() = entry.text().to_lowercase();
            filter.changed(gtk::FilterChange::Different);
        });

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&search);

        // Selection-mode action bar: shown after "Seleziona chat" (hidden otherwise).
        let sel_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(8)
            .margin_end(8)
            .build();
        sel_bar.set_visible(false);
        let sel_count = gtk::Label::new(Some("0 selezionate"));
        sel_count.set_hexpand(true);
        sel_count.set_xalign(0.0);
        let sel_archive = gtk::Button::with_label(if archived_view {
            "Sposta"
        } else {
            "Archivia"
        });
        sel_archive.add_css_class("flat");
        let sel_cancel = gtk::Button::from_icon_name("window-close-symbolic");
        sel_cancel.add_css_class("flat");
        sel_bar.append(&sel_count);
        sel_bar.append(&sel_archive);
        sel_bar.append(&sel_cancel);
        root.append(&sel_bar);

        // Leaving selection mode: back to click-to-open, clear + hide.
        let exit_sel: Rc<dyn Fn()> = {
            let selecting = selecting.clone();
            let list_view = list_view.clone();
            let selection = selection.clone();
            let sel_bar = sel_bar.clone();
            Rc::new(move || {
                selecting.set(false);
                list_view.set_single_click_activate(true);
                selection.unselect_all();
                sel_bar.set_visible(false);
            })
        };
        // Live selection count in the bar.
        {
            let sel_count = sel_count.clone();
            let selecting = selecting.clone();
            let selection2 = selection.clone();
            selection.connect_selection_changed(move |_, _, _| {
                if selecting.get() {
                    sel_count
                        .set_label(&format!("{} selezionate", collect_selected(&selection2).len()));
                }
            });
        }
        // Fill the "enter selection" hook now that the widgets exist.
        {
            let selecting = selecting.clone();
            let list_view = list_view.clone();
            let selection = selection.clone();
            let sel_bar = sel_bar.clone();
            let sel_count = sel_count.clone();
            *enter_sel.borrow_mut() = Some(Box::new(move |jid: String| {
                selecting.set(true);
                list_view.set_single_click_activate(false);
                sel_bar.set_visible(true);
                for i in 0..selection.n_items() {
                    if let Some(o) = selection.item(i).and_downcast::<ChatObject>() {
                        if o.jid() == jid {
                            selection.select_item(i, false);
                            break;
                        }
                    }
                }
                sel_count
                    .set_label(&format!("{} selezionate", collect_selected(&selection).len()));
            }));
        }
        // Bar buttons: archive the selection, or cancel.
        {
            let on_archive = on_archive.clone();
            let selection = selection.clone();
            let exit_sel = exit_sel.clone();
            sel_archive.connect_clicked(move |_| {
                let jids = collect_selected(&selection);
                if !jids.is_empty() {
                    if let Some(cb) = on_archive.borrow().as_ref() {
                        cb(jids, !archived_view);
                    }
                }
                exit_sel();
            });
        }
        {
            let exit_sel = exit_sel.clone();
            sel_cancel.connect_clicked(move |_| exit_sel());
        }

        // Optional "Archiviate" entry above the list (WhatsApp-Web style): an
        // archive icon, the label, and a right-aligned count. Hidden until there
        // is at least one archived chat. Clicking it opens the archived view.
        let archived_entry = if with_archived_button {
            let on_open_archived: OpenArchivedCb = Rc::new(RefCell::new(None));
            let inner = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .margin_top(6)
                .margin_bottom(6)
                .margin_start(6)
                .margin_end(6)
                .build();
            let icon = gtk::Image::from_icon_name("user-trash-symbolic");
            // A box the size of the list avatars so the label aligns with chat rows.
            let icon_slot = gtk::Box::builder()
                .width_request(40)
                .halign(gtk::Align::Center)
                .build();
            icon_slot.append(&icon);
            let label = gtk::Label::builder()
                .label("Archiviate")
                .xalign(0.0)
                .hexpand(true)
                .build();
            label.add_css_class("heading");
            // Notification badge: number of UNREAD archived chats (not the total).
            let count = gtk::Label::builder().halign(gtk::Align::End).build();
            count.add_css_class("badge");
            count.set_visible(false);
            inner.append(&icon_slot);
            inner.append(&label);
            inner.append(&count);

            let button = gtk::Button::builder().child(&inner).build();
            button.add_css_class("flat");
            button.set_visible(false);
            {
                let cb = on_open_archived.clone();
                button.connect_clicked(move |_| {
                    if let Some(f) = cb.borrow().as_ref() {
                        f();
                    }
                });
            }
            root.append(&button);
            root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            Some((button, count, on_open_archived))
        } else {
            None
        };

        root.append(&scrolled);

        Self {
            root,
            store,
            list_view,
            avatars,
            on_need_avatar,
            archived_entry,
            on_archive,
            on_pin,
            selection,
            selecting,
            exit_sel,
        }
    }

    /// Whether the list is in multi-selection mode.
    pub fn is_selecting(&self) -> bool {
        self.selecting.get()
    }

    /// Leaves selection mode (back to click-to-open).
    pub fn cancel_selection(&self) {
        (self.exit_sel)();
    }

    /// Clears the current selection without leaving selection mode.
    pub fn clear_selection(&self) {
        self.selection.unselect_all();
    }

    /// Registers the bulk archive/unarchive callback `(jids, archived)`.
    pub fn connect_archive<F: Fn(Vec<String>, bool) + 'static>(&self, f: F) {
        *self.on_archive.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the pin/unpin callback `(jid, pinned)`.
    pub fn connect_pin<F: Fn(String, bool) + 'static>(&self, f: F) {
        *self.on_pin.borrow_mut() = Some(Box::new(f));
    }

    /// Updates the "Archiviate" entry: the entry is shown while there are any
    /// archived chats (`total`), and the badge shows the number of UNREAD archived
    /// chats (`unread`, hidden when zero). No-op on the archived list itself.
    pub fn set_archived_count(&self, total: usize, unread: usize) {
        if let Some((button, count, _)) = &self.archived_entry {
            button.set_visible(total > 0);
            if unread > 0 {
                count.set_label(&unread.to_string());
                count.set_visible(true);
            } else {
                count.set_label("");
                count.set_visible(false);
            }
        }
    }

    /// Registers the callback invoked when the "Archiviate" entry is clicked.
    pub fn connect_open_archived<F: Fn() + 'static>(&self, f: F) {
        if let Some((_, _, cb)) = &self.archived_entry {
            *cb.borrow_mut() = Some(Box::new(f));
        }
    }

    /// Replaces the whole list with a fresh, ordered snapshot. Cached avatars are
    /// applied to the new objects so already-downloaded pictures show immediately.
    pub fn update(&self, chats: &[ChatSummary]) {
        let cache = self.avatars.borrow();
        let objs: Vec<glib::Object> = chats
            .iter()
            .map(|c| {
                let obj = ChatObject::new(c);
                if let Some(tex) = cache.get(&c.jid) {
                    let p = tex.clone().upcast::<gtk::gdk::Paintable>();
                    obj.set_property("avatar", &p);
                }
                obj.upcast::<glib::Object>()
            })
            .collect();
        drop(cache);
        self.store.remove_all();
        self.store.splice(0, 0, &objs);
    }

    /// Registers the callback invoked (with a JID) when a visible row needs its
    /// avatar fetched.
    pub fn connect_need_avatar<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_need_avatar.borrow_mut() = Some(Box::new(f));
    }

    /// Caches a freshly downloaded texture and applies it to the matching row
    /// (the property binding updates the visible Avatar).
    pub fn set_avatar(&self, jid: &str, tex: &gtk::gdk::Texture) {
        self.avatars
            .borrow_mut()
            .insert(jid.to_string(), tex.clone());
        let paintable = tex.clone().upcast::<gtk::gdk::Paintable>();
        let n = self.store.n_items();
        for i in 0..n {
            if let Some(obj) = self.store.item(i).and_downcast::<ChatObject>() {
                if obj.jid() == jid {
                    obj.set_property("avatar", &paintable);
                    break;
                }
            }
        }
    }

    /// Calls `f(jid, name)` when a chat row is activated (single click / Enter).
    pub fn connect_open<F: Fn(String, String) + 'static>(&self, f: F) {
        self.list_view.connect_activate(move |lv, pos| {
            let Some(model) = lv.model() else {
                return;
            };
            let Some(obj) = model.item(pos).and_downcast::<ChatObject>() else {
                return;
            };
            f(obj.jid(), obj.name());
        });
    }
}

/// Builds an empty row widget. Sub-widgets are looked up positionally in
/// [`bind_row`]; keep the structure in sync between the two.
fn build_row() -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    let avatar = adw::Avatar::new(40, None, true);

    let center = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let name = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    name.add_css_class("heading");
    let msg = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    msg.add_css_class("caption");
    msg.add_css_class("dim-label");
    center.append(&name);
    center.append(&msg);

    let right = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .spacing(4)
        .build();
    let time = gtk::Label::builder().xalign(1.0).build();
    time.add_css_class("caption");
    time.add_css_class("dim-label");
    let badge = gtk::Label::builder().halign(gtk::Align::End).build();
    badge.add_css_class("badge");
    // Pin marker, shown only for pinned chats.
    let pin = gtk::Image::from_icon_name("view-pin-symbolic");
    pin.set_halign(gtk::Align::End);
    pin.add_css_class("dim-label");
    pin.set_visible(false);
    right.append(&time);
    right.append(&badge);
    right.append(&pin);

    row.append(&avatar);
    row.append(&center);
    row.append(&right);
    row
}

/// Fills a row built by [`build_row`] with the values from `obj`.
fn bind_row(row: &gtk::Box, obj: &ChatObject) {
    let Some((avatar, name, msg, time, badge, pin)) = row_widgets(row) else {
        return;
    };
    let display = obj.name();
    avatar.set_text(Some(&display));
    name.set_label(&display);
    set_preview(&msg, obj);
    time.set_label(&obj.timestamp());
    pin.set_visible(obj.pinned());

    let unread = obj.unread();
    if unread > 0 {
        badge.set_label(&unread.to_string());
        badge.set_visible(true);
    } else {
        badge.set_label("");
        badge.set_visible(false);
    }
}

/// Sets the preview label, prefixing a ✓/✓✓ delivery glyph when the last message
/// was ours (blue when read), like the wrapper. Uses Pango markup to colour just
/// the glyph; the body is escaped.
fn set_preview(msg: &gtk::Label, obj: &ChatObject) {
    // Collapse any newlines/tabs so a multi-line message renders on a single,
    // ellipsized line (otherwise the row grows taller than its neighbours).
    let body = obj.last_message();
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let glyph = if obj.last_from_me() {
        match obj.last_status() {
            1 => "✓",
            2 | 3 => "✓✓",
            _ => "",
        }
    } else {
        ""
    };
    if glyph.is_empty() {
        msg.set_text(&body);
        return;
    }
    let esc = glib::markup_escape_text(&body);
    if obj.last_status() >= 3 && !crate::config::HIDE_READ_RECEIPTS {
        msg.set_markup(&format!("<span foreground='#53bdeb'>{glyph}</span> {esc}"));
    } else {
        msg.set_markup(&format!("{glyph} {esc}"));
    }
}

/// Positional lookup of the widgets created in [`build_row`].
#[allow(clippy::type_complexity)]
fn row_widgets(
    row: &gtk::Box,
) -> Option<(
    adw::Avatar,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Image,
)> {
    let avatar = row.first_child()?.downcast::<adw::Avatar>().ok()?;
    let center = avatar.next_sibling()?.downcast::<gtk::Box>().ok()?;
    let name = center.first_child()?.downcast::<gtk::Label>().ok()?;
    let msg = name.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let right = center.next_sibling()?.downcast::<gtk::Box>().ok()?;
    let time = right.first_child()?.downcast::<gtk::Label>().ok()?;
    let badge = time.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let pin = badge.next_sibling()?.downcast::<gtk::Image>().ok()?;
    Some((avatar, name, msg, time, badge, pin))
}
