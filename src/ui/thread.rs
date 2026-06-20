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
type SendAudioCb = Rc<RefCell<Option<Box<dyn Fn(Vec<u8>, u32, Vec<u8>)>>>>;
type PlayCb = Rc<RefCell<Option<Box<dyn Fn(String, String)>>>>;

/// On-screen pieces of a voice-note bubble, so playback can drive its waveform
/// fill, time label and play/pause icon.
#[derive(Clone)]
struct AudioBubble {
    button: gtk::Button,
    area: gtk::DrawingArea,
    time: gtk::Label,
    /// Playback fraction 0..1, read by the waveform draw func.
    progress: Rc<Cell<f64>>,
    secs: u32,
}

/// Thin slot (bar + gap) width in px — keeps bars slim and theme-agnostic.
const BAR_W: f64 = 2.0;
const SLOT_W: f64 = 4.0;

/// Number of thin bars that fit in `w` px.
fn bar_count(w: f64) -> usize {
    ((w / SLOT_W).floor() as usize).max(1)
}

/// Amplitude (0..1) for bar `i` of `n`, sampled from `waveform` (0..100).
fn wf_amp(waveform: &[u8], i: usize, n: usize) -> f64 {
    if waveform.is_empty() {
        return 0.12;
    }
    let idx = (i * waveform.len() / n).min(waveform.len() - 1);
    (waveform[idx] as f64 / 100.0).clamp(0.0, 1.0)
}

/// Draws thin amplitude bars (`waveform`, 0..100) using the widget's own
/// foreground color (so it contrasts the bubble in any theme); bars before
/// `progress` (0..1) are solid, the rest dimmed.
fn draw_waveform(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    w: i32,
    h: i32,
    waveform: &[u8],
    progress: f64,
) {
    let c = area.color();
    let w = w as f64;
    let h = h as f64;
    let n = bar_count(w);
    for i in 0..n {
        let amp = wf_amp(waveform, i, n);
        let bh = (amp * h).max(2.0);
        let x = i as f64 * SLOT_W;
        let y = (h - bh) / 2.0;
        let played = (i as f64 + 0.5) / n as f64 <= progress;
        let alpha = if played { 1.0 } else { 0.35 } * c.alpha() as f64;
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, alpha);
        cr.rectangle(x, y, BAR_W, bh);
        let _ = cr.fill();
    }
}

/// Formats seconds as `m:ss`.
fn fmt_secs(secs: f64) -> String {
    let s = secs.max(0.0) as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Draws a live recording envelope: the most recent amplitudes (0..1) as thin
/// bars in the widget's foreground color, left-to-right.
fn draw_live(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32, levels: &[f64]) {
    let c = area.color();
    let w = w as f64;
    let h = h as f64;
    let n = bar_count(w);
    let slice = &levels[levels.len().saturating_sub(n)..];
    // Raw RMS amplitudes are small for speech; auto-gain to the loudest sample of
    // the whole take (low floor) and boost perceptually so the bars fill and adapt.
    let max = levels.iter().cloned().fold(0.0f64, f64::max).max(0.05);
    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
    for i in 0..n {
        let raw = slice.get(i).copied().unwrap_or(0.0);
        let amp = (raw / max).clamp(0.0, 1.0).sqrt();
        let bh = (amp * h).max(1.0);
        let x = i as f64 * SLOT_W;
        let y = (h - bh) / 2.0;
        cr.rectangle(x, y, BAR_W, bh);
    }
    let _ = cr.fill();
}

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
    /// The composer text entry, so a freshly opened chat can focus it.
    entry: gtk::Entry,
    /// Voice-note id currently playing (drives toggle + progress).
    playing_id: Rc<RefCell<Option<String>>>,
    /// Audio bubbles by message id, to update their waveform/time/icon on play.
    audio_widgets: Rc<RefCell<HashMap<String, AudioBubble>>>,
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

        // Live recording waveform, shown in place of the entry while recording.
        let rec_levels: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
        let rec_area = gtk::DrawingArea::builder()
            .height_request(28)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        rec_area.set_visible(false);
        {
            let rec_levels = rec_levels.clone();
            rec_area.set_draw_func(move |area, cr, w, h| {
                draw_live(area, cr, w, h, &rec_levels.borrow())
            });
        }
        let rec_time = gtk::Label::new(Some("0:00"));
        rec_time.add_css_class("caption");
        rec_time.add_css_class("dim-label");
        rec_time.set_visible(false);

        // Mic button toggles a voice-note recording; the second tap stops & sends.
        {
            let recorder = recorder.clone();
            let on_send_audio = on_send_audio.clone();
            let mic_btn = mic.clone();
            let entry_w = entry.clone();
            let rec_area_w = rec_area.clone();
            let rec_time_w = rec_time.clone();
            let rec_levels = rec_levels.clone();
            mic.connect_clicked(move |_| {
                let recording = recorder.borrow().is_some();
                if recording {
                    let rec = recorder.borrow_mut().take();
                    mic_btn.remove_css_class("destructive-action");
                    mic_btn.set_icon_name("audio-input-microphone-symbolic");
                    rec_area_w.set_visible(false);
                    rec_time_w.set_visible(false);
                    entry_w.set_visible(true);
                    if let Some(rec) = rec {
                        match rec.stop() {
                            Ok((ogg, secs, waveform)) => {
                                if let Some(cb) = on_send_audio.borrow().as_ref() {
                                    cb(ogg, secs, waveform);
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
                            rec_levels.borrow_mut().clear();
                            entry_w.set_visible(false);
                            rec_area_w.set_visible(true);
                            rec_time_w.set_visible(true);
                            rec_time_w.set_label("0:00");
                            // Poll mic levels, update the live waveform + timer.
                            let recorder = recorder.clone();
                            let rec_levels = rec_levels.clone();
                            let rec_area = rec_area_w.clone();
                            let rec_time = rec_time_w.clone();
                            gtk::glib::timeout_add_local(
                                std::time::Duration::from_millis(60),
                                move || {
                                    match recorder.borrow().as_ref() {
                                        Some(rec) => {
                                            rec.poll_levels();
                                            *rec_levels.borrow_mut() = rec.levels();
                                            rec_area.queue_draw();
                                            rec_time.set_label(&fmt_secs(rec.elapsed_secs() as f64));
                                            gtk::glib::ControlFlow::Continue
                                        }
                                        None => gtk::glib::ControlFlow::Break,
                                    }
                                },
                            );
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
        composer.append(&rec_area);
        composer.append(&rec_time);
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
            entry,
            playing_id: Rc::new(RefCell::new(None)),
            audio_widgets: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Puts the keyboard focus in the composer so the user can type right away.
    pub fn focus_composer(&self) {
        self.entry.grab_focus();
    }

    /// Registers the callback invoked with the composed text when the user sends.
    pub fn connect_send<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_send.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(ogg_bytes, duration_secs)` for a
    /// recorded voice note.
    pub fn connect_send_audio<F: Fn(Vec<u8>, u32, Vec<u8>) + 'static>(&self, f: F) {
        *self.on_send_audio.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(chat_jid, id)` when a voice note's
    /// play button is pressed.
    pub fn connect_play<F: Fn(String, String) + 'static>(&self, f: F) {
        *self.on_play.borrow_mut() = Some(Box::new(f));
    }

    /// Plays a downloaded voice note (`id` → `path`), stopping any current one and
    /// driving the matching bubble's waveform fill / time / play-pause icon.
    pub fn play_audio(&self, id: &str, path: &str) {
        // Stop and reset the previously playing bubble.
        if let Some(prev) = self.player.borrow_mut().take() {
            prev.pause();
        }
        if let Some(prev_id) = self.playing_id.borrow_mut().take() {
            if let Some(w) = self.audio_widgets.borrow().get(&prev_id) {
                w.progress.set(0.0);
                w.area.queue_draw();
                w.button.set_icon_name("media-playback-start-symbolic");
                w.time.set_label(&fmt_secs(w.secs as f64));
            }
        }

        let media = gtk::MediaFile::for_filename(path);
        *self.playing_id.borrow_mut() = Some(id.to_string());
        if let Some(w) = self.audio_widgets.borrow().get(id) {
            w.button.set_icon_name("media-playback-pause-symbolic");
        }
        // Update the bubble as playback advances; reset at the end.
        {
            let widgets = self.audio_widgets.clone();
            let playing_id = self.playing_id.clone();
            let id = id.to_string();
            media.connect_timestamp_notify(move |m| {
                let dur = m.duration();
                let ts = m.timestamp();
                let frac = if dur > 0 { (ts as f64 / dur as f64).clamp(0.0, 1.0) } else { 0.0 };
                let widgets = widgets.borrow();
                let Some(w) = widgets.get(&id) else { return };
                if m.is_ended() || (dur > 0 && ts >= dur) {
                    w.progress.set(0.0);
                    w.button.set_icon_name("media-playback-start-symbolic");
                    w.time.set_label(&fmt_secs(w.secs as f64));
                    if playing_id.borrow().as_deref() == Some(id.as_str()) {
                        *playing_id.borrow_mut() = None;
                    }
                } else {
                    w.progress.set(frac);
                    let pos = ts as f64 / 1_000_000.0;
                    w.time.set_label(&fmt_secs(pos));
                }
                w.area.queue_draw();
            });
        }
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
        self.audio_widgets.borrow_mut().clear();
        if let Some(p) = self.player.borrow_mut().take() {
            p.pause();
        }
        *self.playing_id.borrow_mut() = None;
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
            // Voice-note player: play/pause + waveform with progress + duration.
            let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
            play.add_css_class("circular");
            play.set_valign(gtk::Align::Center);

            let progress = Rc::new(Cell::new(0.0));
            let area = gtk::DrawingArea::builder()
                .height_request(28)
                .width_request(140)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            {
                let progress = progress.clone();
                let wf = m.audio_waveform.clone();
                area.set_draw_func(move |area, cr, w, h| {
                    draw_waveform(area, cr, w, h, &wf, progress.get())
                });
            }
            let dur_label = gtk::Label::new(Some(&fmt_secs(m.audio_secs as f64)));
            dur_label.add_css_class("caption");
            dur_label.add_css_class("dim-label");

            // Click: toggle if this note is the active one, else request playback.
            {
                let on_play = self.on_play.clone();
                let chat = m.chat_jid.clone();
                let id = m.id.clone();
                let player = self.player.clone();
                let playing_id = self.playing_id.clone();
                let btn = play.clone();
                play.connect_clicked(move |_| {
                    if playing_id.borrow().as_deref() == Some(id.as_str()) {
                        if let Some(media) = player.borrow().as_ref() {
                            if media.is_playing() {
                                media.pause();
                                btn.set_icon_name("media-playback-start-symbolic");
                            } else {
                                media.play();
                                btn.set_icon_name("media-playback-pause-symbolic");
                            }
                            return;
                        }
                    }
                    if let Some(cb) = on_play.borrow().as_ref() {
                        cb(chat.clone(), id.clone());
                    }
                });
            }

            self.audio_widgets.borrow_mut().insert(
                m.id.clone(),
                AudioBubble {
                    button: play.clone(),
                    area: area.clone(),
                    time: dur_label.clone(),
                    progress,
                    secs: m.audio_secs,
                },
            );

            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            row.append(&play);
            row.append(&area);
            row.append(&dur_label);
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
                // Render URLs as clickable links (markup), text stays selectable.
                .use_markup(true)
                .build();
            text.set_markup(&crate::util::text::linkify(&m.body));
            // Open links in the default browser (no hardcoded launcher command).
            text.connect_activate_link(|label, uri| {
                let ctx = label.display().app_launch_context();
                if let Err(e) =
                    gtk::gio::AppInfo::launch_default_for_uri(uri, Some(&ctx))
                {
                    log::warn!("open link failed: {e}");
                }
                glib::Propagation::Stop
            });
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
