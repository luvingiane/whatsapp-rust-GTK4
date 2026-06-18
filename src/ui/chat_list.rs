//! The chat list sidebar: a virtualized `ListView` over a `ListStore` of
//! [`ChatObject`]s, rebuilt from a [`ChatSummary`] snapshot on each update.

use std::cell::RefCell;
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

#[derive(Clone)]
pub struct ChatList {
    pub root: gtk::Box,
    store: gio::ListStore,
    list_view: gtk::ListView,
    /// Shared decoded-texture cache (filled by [`Self::set_avatar`]).
    avatars: AvatarCache,
    /// Invoked with a JID when a visible row still lacks its avatar.
    on_need_avatar: NeedAvatarCb,
}

impl ChatList {
    pub fn new(avatars: &AvatarCache) -> Self {
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
        let selection = gtk::SingleSelection::builder()
            .model(&filter_model)
            .autoselect(false)
            .can_unselect(true)
            .build();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&build_row()));
            }
        });
        {
            let on_need_avatar = on_need_avatar.clone();
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
            });
        }
        factory.connect_unbind(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let Some(binding) = unsafe { item.steal_data::<glib::Binding>(AVATAR_BINDING_KEY) } {
                binding.unbind();
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
        root.append(&scrolled);

        Self {
            root,
            store,
            list_view,
            avatars,
            on_need_avatar,
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
    right.append(&time);
    right.append(&badge);

    row.append(&avatar);
    row.append(&center);
    row.append(&right);
    row
}

/// Fills a row built by [`build_row`] with the values from `obj`.
fn bind_row(row: &gtk::Box, obj: &ChatObject) {
    let Some((avatar, name, msg, time, badge)) = row_widgets(row) else {
        return;
    };
    let display = obj.name();
    avatar.set_text(Some(&display));
    name.set_label(&display);
    msg.set_label(&obj.last_message());
    time.set_label(&obj.timestamp());

    let unread = obj.unread();
    if unread > 0 {
        badge.set_label(&unread.to_string());
        badge.set_visible(true);
    } else {
        badge.set_label("");
        badge.set_visible(false);
    }
}

/// Positional lookup of the widgets created in [`build_row`].
#[allow(clippy::type_complexity)]
fn row_widgets(
    row: &gtk::Box,
) -> Option<(adw::Avatar, gtk::Label, gtk::Label, gtk::Label, gtk::Label)> {
    let avatar = row.first_child()?.downcast::<adw::Avatar>().ok()?;
    let center = avatar.next_sibling()?.downcast::<gtk::Box>().ok()?;
    let name = center.first_child()?.downcast::<gtk::Label>().ok()?;
    let msg = name.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let right = center.next_sibling()?.downcast::<gtk::Box>().ok()?;
    let time = right.first_child()?.downcast::<gtk::Label>().ok()?;
    let badge = time.next_sibling()?.downcast::<gtk::Label>().ok()?;
    Some((avatar, name, msg, time, badge))
}
