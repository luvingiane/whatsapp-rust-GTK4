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
use crate::audio::Recorder;
use crate::model::MessageRow;
use crate::util::preview;

/// Scroll position (px from top) under which we ask for the previous page.
const BACKFILL_THRESHOLD: f64 = 40.0;

type LoadOlderCb = Box<dyn Fn(i64, String)>;
type NeedAvatarCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
type SendCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
type SendAudioCb = Rc<RefCell<Option<Box<dyn Fn(Vec<u8>, u32)>>>>;
type PlayCb = Rc<RefCell<Option<Box<dyn Fn(String, String)>>>>;

#[derive(Clone)]
pub struct ThreadView {
    /// Outer container: the scrolled message list above a composer bar.
    pub root: gtk::Box,
    /// The scrolled message list (drives backfill via its vadjustment).
    scrolled: gtk::ScrolledWindow,
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
    /// Our sent message id → its ✓/✓✓ status label, so a later receipt updates
    /// the glyph in place. Cleared when the thread is cleared.
    ticks: Rc<RefCell<HashMap<String, gtk::Label>>>,
    /// Invoked with the composed text when the user sends a message.
    on_send: SendCb,
    /// Invoked with `(ogg_bytes, duration_secs)` when a voice note is recorded.
    on_send_audio: SendAudioCb,
    /// Invoked with `(chat_jid, id)` when a voice note's play button is pressed.
    on_play: PlayCb,
    /// The currently playing voice note, kept alive while it plays.
    player: Rc<RefCell<Option<gtk::MediaFile>>>,
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

        let scrolled = gtk::ScrolledWindow::builder()
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
            scrolled.vadjustment().connect_value_changed(move |adj| {
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

        // --- composer: text entry + mic + send --------------------------------
        let on_send: SendCb = Rc::new(RefCell::new(None));
        let on_send_audio: SendAudioCb = Rc::new(RefCell::new(None));
        let recorder: Rc<RefCell<Option<Recorder>>> = Rc::new(RefCell::new(None));

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Scrivi un messaggio")
            .build();
        let mic = gtk::Button::from_icon_name("audio-input-microphone-symbolic");
        mic.add_css_class("flat");
        let send = gtk::Button::from_icon_name("document-send-symbolic");
        send.add_css_class("suggested-action");

        // Send on Enter or on the Send button; ignore blank input.
        let do_send = {
            let entry = entry.clone();
            let on_send = on_send.clone();
            move || {
                let text = entry.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                if let Some(cb) = on_send.borrow().as_ref() {
                    cb(text);
                }
                entry.set_text("");
            }
        };
        {
            let do_send = do_send.clone();
            entry.connect_activate(move |_| do_send());
        }
        {
            let do_send = do_send.clone();
            send.connect_clicked(move |_| do_send());
        }

        // Mic button toggles a voice-note recording; the second tap stops & sends.
        {
            let recorder = recorder.clone();
            let on_send_audio = on_send_audio.clone();
            let mic_btn = mic.clone();
            mic.connect_clicked(move |_| {
                let recording = recorder.borrow().is_some();
                if recording {
                    let rec = recorder.borrow_mut().take();
                    mic_btn.remove_css_class("destructive-action");
                    mic_btn.set_icon_name("audio-input-microphone-symbolic");
                    if let Some(rec) = rec {
                        match rec.stop() {
                            Ok((ogg, secs)) => {
                                if let Some(cb) = on_send_audio.borrow().as_ref() {
                                    cb(ogg, secs);
                                }
                            }
                            Err(e) => log::warn!("voice note failed: {e:?}"),
                        }
                    }
                } else {
                    match Recorder::start() {
                        Ok(rec) => {
                            *recorder.borrow_mut() = Some(rec);
                            mic_btn.add_css_class("destructive-action");
                            mic_btn.set_icon_name("media-playback-stop-symbolic");
                        }
                        Err(e) => log::warn!("cannot start recording: {e:?}"),
                    }
                }
            });
        }

        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        composer.append(&entry);
        composer.append(&mic);
        composer.append(&send);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&scrolled);
        root.append(&composer);

        Self {
            root,
            scrolled,
            list,
            is_group: Rc::new(Cell::new(false)),
            oldest,
            loading_older,
            exhausted,
            on_load_older,
            avatars: avatars.clone(),
            senders: Rc::new(RefCell::new(HashMap::new())),
            on_need_avatar: Rc::new(RefCell::new(None)),
            ticks: Rc::new(RefCell::new(HashMap::new())),
            on_send,
            on_send_audio,
            on_play: Rc::new(RefCell::new(None)),
            player: Rc::new(RefCell::new(None)),
        }
    }

    /// Registers the callback invoked with the composed text when the user sends.
    pub fn connect_send<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_send.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(ogg_bytes, duration_secs)` for a
    /// recorded voice note.
    pub fn connect_send_audio<F: Fn(Vec<u8>, u32) + 'static>(&self, f: F) {
        *self.on_send_audio.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(chat_jid, id)` when a voice note's
    /// play button is pressed.
    pub fn connect_play<F: Fn(String, String) + 'static>(&self, f: F) {
        *self.on_play.borrow_mut() = Some(Box::new(f));
    }

    /// Plays a downloaded voice note from `path`, stopping any current playback.
    pub fn play_audio(&self, path: &str) {
        if let Some(prev) = self.player.borrow_mut().take() {
            prev.pause();
        }
        let media = gtk::MediaFile::for_filename(path);
        media.play();
        *self.player.borrow_mut() = Some(media);
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
        let vadj = self.scrolled.vadjustment();
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
        self.ticks.borrow_mut().clear();
    }

    /// Advances the ✓/✓✓ glyph of one of our sent bubbles when a receipt lands
    /// (no-op if that message isn't currently on screen).
    pub fn set_status(&self, id: &str, status: i32) {
        if let Some(tick) = self.ticks.borrow().get(id) {
            tick.set_label(status_glyph(status));
            if status >= 3 {
                tick.add_css_class("tick-read");
            } else {
                tick.remove_css_class("tick-read");
            }
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

        if m.audio {
            // Voice note: a play button that requests download+playback, plus the
            // "🎤 Messaggio vocale" label.
            let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
            play.add_css_class("circular");
            play.set_valign(gtk::Align::Center);
            {
                let on_play = self.on_play.clone();
                let chat = m.chat_jid.clone();
                let id = m.id.clone();
                play.connect_clicked(move |_| {
                    if let Some(cb) = on_play.borrow().as_ref() {
                        cb(chat.clone(), id.clone());
                    }
                });
            }
            let label = gtk::Label::builder().label(&m.body).xalign(0.0).build();
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            row.append(&play);
            row.append(&label);
            bubble.append(&row);
        } else {
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
        }

        let time = gtk::Label::builder()
            .label(format_time(m.ts))
            .xalign(1.0)
            .build();
        time.add_css_class("caption");
        time.add_css_class("dim-label");

        if m.from_me {
            // Time + delivery ticks on one right-aligned row; the tick label is
            // tracked by message id so a later receipt can update it in place.
            let tick = gtk::Label::new(Some(status_glyph(m.status)));
            tick.add_css_class("tick");
            if m.status >= 3 {
                tick.add_css_class("tick-read");
            }
            self.ticks.borrow_mut().insert(m.id.clone(), tick.clone());
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .halign(gtk::Align::End)
                .build();
            row.append(&time);
            row.append(&tick);
            bubble.append(&row);
        } else {
            bubble.append(&time);
        }

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
        let vadj = self.scrolled.vadjustment();
        glib::idle_add_local_once(move || {
            vadj.set_value(vadj.upper() - vadj.page_size());
        });
    }
}

/// The check glyph for an outgoing message's delivery status: 1 sent (✓), 2
/// delivered and 3 read (✓✓ — colour distinguishes read). 0/other → none.
fn status_glyph(status: i32) -> &'static str {
    match status {
        1 => "✓",
        2 | 3 => "✓✓",
        _ => "",
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
