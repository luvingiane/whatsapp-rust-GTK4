//! The conversation thread view: a scrolled column of message bubbles (sent on
//! the right, received on the left). Populated from a [`MessageRow`] history,
//! appended in real time, and backfilled with older pages when scrolled to the
//! top. Media show as a labelled placeholder for now.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use libadwaita as adw;

use super::AvatarCache;
use crate::model::MessageRow;
use crate::util::preview;

/// Scroll position (px from top) under which we ask for the previous page.
const BACKFILL_THRESHOLD: f64 = 40.0;

type LoadOlderCb = Box<dyn Fn(i64, String)>;
type NeedAvatarCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;

#[derive(Clone)]
pub struct ThreadView {
    pub root: gtk::ScrolledWindow,
    list: gtk::Box,
    /// Whether the open chat is a group (drives the per-message sender label).
    is_group: Rc<Cell<bool>>,
    /// Keyset cursor (ts, id) of the topmost loaded message, or `None` until a
    /// history is shown. Drives backfill requests.
    oldest: Rc<RefCell<Option<(i64, String)>>>,
    /// A backfill request is in flight (suppresses duplicate requests).
    loading_older: Rc<Cell<bool>>,
    /// Local history exhausted: stop asking for older pages.
    exhausted: Rc<Cell<bool>>,
    /// Called with the current `(ts, id)` cursor when the user scrolls to the top.
    on_load_older: Rc<RefCell<Option<LoadOlderCb>>>,
    /// Shared decoded-texture cache (shared with the chat list).
    avatars: AvatarCache,
    /// Group sender JID → its on-screen avatar widgets, so a late download
    /// updates already-rendered bubbles. Cleared when the thread is cleared.
    senders: Rc<RefCell<HashMap<String, Vec<adw::Avatar>>>>,
    /// Invoked with a sender JID when a group bubble still lacks its avatar.
    on_need_avatar: NeedAvatarCb,
}

impl ThreadView {
    pub fn new(avatars: &AvatarCache) -> Self {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let oldest: Rc<RefCell<Option<(i64, String)>>> = Rc::new(RefCell::new(None));
        let loading_older = Rc::new(Cell::new(false));
        let exhausted = Rc::new(Cell::new(false));
        let on_load_older: Rc<RefCell<Option<LoadOlderCb>>> = Rc::new(RefCell::new(None));

        // Backfill trigger: when scrolled near the top, ask for the previous page.
        {
            let oldest = oldest.clone();
            let loading_older = loading_older.clone();
            let exhausted = exhausted.clone();
            let on_load_older = on_load_older.clone();
            root.vadjustment().connect_value_changed(move |adj| {
                if adj.value() > BACKFILL_THRESHOLD || loading_older.get() || exhausted.get() {
                    return;
                }
                let cursor = oldest.borrow().clone();
                let Some((ts, id)) = cursor else { return };
                if let Some(cb) = on_load_older.borrow().as_ref() {
                    loading_older.set(true);
                    cb(ts, id);
                }
            });
        }

        Self {
            root,
            list,
            is_group: Rc::new(Cell::new(false)),
            oldest,
            loading_older,
            exhausted,
            on_load_older,
            avatars: avatars.clone(),
            senders: Rc::new(RefCell::new(HashMap::new())),
            on_need_avatar: Rc::new(RefCell::new(None)),
        }
    }

    /// Prepare for a freshly opened chat: clear bubbles and record group-ness.
    pub fn set_loading(&self, is_group: bool) {
        self.is_group.set(is_group);
        self.clear();
    }

    /// Render a full history (oldest-first) and scroll to the bottom.
    pub fn show_history(&self, messages: &[MessageRow]) {
        self.clear();
        for m in messages {
            self.list.append(&self.bubble(m));
        }
        // First message is the oldest → the backfill cursor starts there.
        *self.oldest.borrow_mut() = messages.first().map(|m| (m.ts, m.id.clone()));
        self.loading_older.set(false);
        self.exhausted.set(false);
        self.scroll_to_bottom();
    }

    /// Prepend an older page (oldest-first) while keeping the viewport on the same
    /// message. An empty page means the local history is exhausted.
    pub fn prepend_history(&self, messages: &[MessageRow]) {
        if messages.is_empty() {
            self.exhausted.set(true);
            self.loading_older.set(false);
            return;
        }

        // Record the scroll geometry before inserting, so we can compensate for
        // the height added above the current viewport.
        let vadj = self.root.vadjustment();
        let old_value = vadj.value();
        let old_upper = vadj.upper();

        // Prepend in reverse so the final order stays oldest-first at the top.
        for m in messages.iter().rev() {
            self.list.prepend(&self.bubble(m));
        }
        *self.oldest.borrow_mut() = messages.first().map(|m| (m.ts, m.id.clone()));

        // Restore the viewport after layout recomputes `upper`.
        glib::idle_add_local_once(move || {
            vadj.set_value(old_value + (vadj.upper() - old_upper));
        });
        self.loading_older.set(false);
    }

    /// Append a single live message and scroll to the bottom.
    pub fn append(&self, m: &MessageRow) {
        self.list.append(&self.bubble(m));
        self.scroll_to_bottom();
    }

    /// Registers the callback invoked (with the current `(ts, id)` cursor) when the
    /// user scrolls to the top and an older page should be loaded.
    pub fn connect_load_older<F: Fn(i64, String) + 'static>(&self, f: F) {
        *self.on_load_older.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked (with a sender JID) when a group bubble
    /// needs its avatar fetched.
    pub fn connect_need_avatar<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_need_avatar.borrow_mut() = Some(Box::new(f));
    }

    /// Caches a freshly downloaded texture and applies it to any on-screen group
    /// bubbles authored by `jid`.
    pub fn set_avatar(&self, jid: &str, tex: &gtk::gdk::Texture) {
        self.avatars
            .borrow_mut()
            .insert(jid.to_string(), tex.clone());
        if let Some(list) = self.senders.borrow().get(jid) {
            for a in list {
                a.set_custom_image(Some(tex));
            }
        }
    }

    /// Remove all bubbles and reset the backfill state.
    pub fn clear(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        *self.oldest.borrow_mut() = None;
        self.loading_older.set(false);
        self.exhausted.set(false);
        self.senders.borrow_mut().clear();
    }

    fn bubble(&self, m: &MessageRow) -> gtk::Box {
        let bubble = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(1)
            .halign(if m.from_me {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .build();
        bubble.add_css_class(if m.from_me { "bubble-out" } else { "bubble-in" });

        // Sender label for incoming group messages: resolved name, else number.
        if self.is_group.get() && !m.from_me && !m.sender_jid.is_empty() {
            let label = if m.sender_name.is_empty() {
                preview::pretty_number(&m.sender_jid)
            } else {
                m.sender_name.clone()
            };
            let sender = gtk::Label::builder().label(label).xalign(0.0).build();
            sender.add_css_class("caption-heading");
            bubble.append(&sender);
        }

        let text = gtk::Label::builder()
            .label(&m.body)
            .xalign(0.0)
            .wrap(true)
            // WordChar so a long unbreakable URL wraps by character instead of
            // forcing a huge minimum width on the bubble (and the window).
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(42)
            .selectable(true)
            .build();
        bubble.append(&text);

        let time = gtk::Label::builder()
            .label(format_time(m.ts))
            .xalign(1.0)
            .build();
        time.add_css_class("caption");
        time.add_css_class("dim-label");
        bubble.append(&time);

        // In groups, show the sender's avatar to the left of incoming bubbles.
        if self.is_group.get() && !m.from_me && !m.sender_jid.is_empty() {
            let initials = if m.sender_name.is_empty() {
                preview::pretty_number(&m.sender_jid)
            } else {
                m.sender_name.clone()
            };
            let avatar = adw::Avatar::new(28, Some(&initials), true);
            avatar.set_valign(gtk::Align::Start);
            if let Some(tex) = self.avatars.borrow().get(&m.sender_jid) {
                avatar.set_custom_image(Some(tex));
            } else {
                self.senders
                    .borrow_mut()
                    .entry(m.sender_jid.clone())
                    .or_default()
                    .push(avatar.clone());
                if let Some(cb) = self.on_need_avatar.borrow().as_ref() {
                    cb(m.sender_jid.clone());
                }
            }
            let wrap = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .halign(gtk::Align::Start)
                .build();
            wrap.append(&avatar);
            wrap.append(&bubble);
            return wrap;
        }

        bubble
    }

    fn scroll_to_bottom(&self) {
        // Defer until after layout so `upper` reflects the new content.
        let vadj = self.root.vadjustment();
        glib::idle_add_local_once(move || {
            vadj.set_value(vadj.upper() - vadj.page_size());
        });
    }
}

/// Formats a unix timestamp as `HH:MM` in local time (empty if unset).
fn format_time(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    glib::DateTime::from_unix_local(ts)
        .ok()
        .and_then(|dt| dt.format("%H:%M").ok())
        .map(|g| g.to_string())
        .unwrap_or_default()
}
