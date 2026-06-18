//! The conversation thread view: a scrolled column of message bubbles (sent on
//! the right, received on the left). Populated from a [`MessageRow`] history and
//! appended in real time. Media show as a labelled placeholder for now.

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::model::MessageRow;
use crate::util::preview;

#[derive(Clone)]
pub struct ThreadView {
    pub root: gtk::ScrolledWindow,
    list: gtk::Box,
    /// Whether the open chat is a group (drives the per-message sender label).
    is_group: Rc<Cell<bool>>,
}

impl ThreadView {
    pub fn new() -> Self {
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

        Self {
            root,
            list,
            is_group: Rc::new(Cell::new(false)),
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
        self.scroll_to_bottom();
    }

    /// Append a single live message and scroll to the bottom.
    pub fn append(&self, m: &MessageRow) {
        self.list.append(&self.bubble(m));
        self.scroll_to_bottom();
    }

    /// Remove all bubbles.
    pub fn clear(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
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

impl Default for ThreadView {
    fn default() -> Self {
        Self::new()
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
