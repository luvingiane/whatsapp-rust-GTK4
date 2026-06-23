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
/// Invoked with a participant JID when their avatar/name is clicked in a group.
type OpenProfileCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
/// Invoked with `(chat_jid, msg_id, kind)` when a media thumbnail is clicked.
type OpenMediaCb = Rc<RefCell<Option<Box<dyn Fn(String, String, i32)>>>>;
/// Invoked with `(chat_jid, msg_id)` to lazily download a photo for inline display.
type LoadInlineCb = Rc<RefCell<Option<Box<dyn Fn(String, String)>>>>;
/// Invoked with the pending attachments `(data, mime, name, is_image)`, an optional
/// caption, and an optional reply quote when the user sends them.
#[allow(clippy::type_complexity)]
type SendFilesCb = Rc<
    RefCell<
        Option<
            Box<
                dyn Fn(
                    Vec<(Vec<u8>, String, String, bool)>,
                    Option<String>,
                    Option<(String, String, String)>,
                ),
            >,
        >,
    >,
>;

/// A file the user has staged to send (image or document).
#[derive(Clone)]
struct PendingAttachment {
    data: Vec<u8>,
    mime: String,
    name: String,
    is_image: bool,
}
/// `(text, Option<(quoted_id, quoted_sender_jid, quoted_body)>)`.
type SendCb = Rc<RefCell<Option<Box<dyn Fn(String, Option<(String, String, String)>)>>>>;
/// Reply being composed: `(quoted_id, quoted_sender_jid, display_name, body)`.
type ReplyState = Rc<RefCell<Option<(String, String, String, String)>>>;
/// `(ogg_bytes, duration_secs, waveform, Option<(quoted_id, quoted_sender_jid, quoted_body)>)`.
type SendAudioCb =
    Rc<RefCell<Option<Box<dyn Fn(Vec<u8>, u32, Vec<u8>, Option<(String, String, String)>)>>>>;
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
    /// Whether the viewport is pinned to the bottom (drives live auto-scroll).
    stick_bottom: Rc<Cell<bool>>,
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
    /// Invoked with a participant JID when their avatar/name is clicked in a group.
    on_open_profile: OpenProfileCb,
    /// Invoked with `(chat_jid, id, kind)` when a media thumbnail is clicked.
    on_open_media: OpenMediaCb,
    /// Invoked with `(chat_jid, id)` to request a photo's inline download.
    on_load_inline: LoadInlineCb,
    /// Files staged to send, and a closure that rebuilds the preview strip from
    /// them (so chat switches / sends can reset the strip).
    pending: Rc<RefCell<Vec<PendingAttachment>>>,
    rebuild_attach: Rc<dyn Fn()>,
    on_send_files: SendFilesCb,
    /// Photo/video message id → its inline `Picture`, filled when the image
    /// downloads. Cleared when the thread is cleared.
    media_pics: Rc<RefCell<HashMap<String, gtk::Picture>>>,
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
    /// The message being quoted while composing a reply (if any).
    reply: ReplyState,
    /// The reply banner above the composer + its preview label (shown while replying).
    reply_banner: gtk::Box,
    reply_banner_label: gtk::Label,
    /// Day key (`day_key`) of the last appended message, so live appends know when
    /// to insert a new date separator. Reset on clear.
    last_day: Rc<RefCell<Option<i64>>>,
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

        // Floating "jump to last message" button, shown only when scrolled up.
        let to_bottom = gtk::Button::from_icon_name("go-down-symbolic");
        to_bottom.add_css_class("circular");
        to_bottom.add_css_class("osd");
        to_bottom.set_halign(gtk::Align::End);
        to_bottom.set_valign(gtk::Align::End);
        to_bottom.set_margin_end(12);
        to_bottom.set_margin_bottom(12);
        to_bottom.set_visible(false);
        {
            let vadj = scrolled.vadjustment();
            to_bottom.connect_clicked(move |_| {
                let vadj = vadj.clone();
                glib::idle_add_local_once(move || {
                    vadj.set_value(vadj.upper() - vadj.page_size());
                });
            });
        }
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scrolled));
        overlay.add_overlay(&to_bottom);

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

        // Auto-scroll "stick to bottom": track whether the user is at the bottom,
        // and when new content grows the list, follow it down only if they were
        // (so a live message doesn't yank someone who's reading history).
        // The follow is a short eased animation (not an instant jump) so a live/sent
        // message — especially one whose image bubble resizes after load — glides
        // into view instead of snapping ("scatto").
        let stick_bottom = Rc::new(Cell::new(true));
        let auto_scrolling = Rc::new(Cell::new(false));
        {
            let stick = stick_bottom.clone();
            let to_bottom = to_bottom.clone();
            let auto_scrolling = auto_scrolling.clone();
            scrolled.vadjustment().connect_value_changed(move |adj| {
                // While easing toward the bottom the value is transiently above it;
                // don't flip "stick" off (which would flash the jump-to-bottom button)
                // until the animation settles.
                if auto_scrolling.get() {
                    return;
                }
                let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 40.0;
                stick.set(at_bottom);
                // Show the "jump to last message" button while scrolled up.
                to_bottom.set_visible(!at_bottom);
            });
        }
        {
            let stick = stick_bottom.clone();
            let auto_scrolling = auto_scrolling.clone();
            let scrolled_t = scrolled.clone();
            scrolled.vadjustment().connect_changed(move |adj| {
                if !stick.get() {
                    return;
                }
                let target = adj.upper() - adj.page_size();
                if (adj.value() - target).abs() < 1.0 || auto_scrolling.get() {
                    return; // already at the bottom, or a tick is already easing there
                }
                auto_scrolling.set(true);
                let auto_scrolling = auto_scrolling.clone();
                let scrolled_inner = scrolled_t.clone();
                scrolled_t.add_tick_callback(move |_, _clock| {
                    let vadj = scrolled_inner.vadjustment();
                    // Re-read the target each frame: the content may still be settling
                    // (e.g. an image bubble finishing its layout).
                    let target = (vadj.upper() - vadj.page_size()).max(vadj.lower());
                    let cur = vadj.value();
                    let diff = target - cur;
                    if diff.abs() < 0.5 {
                        auto_scrolling.set(false);
                        vadj.set_value(target);
                        glib::ControlFlow::Break
                    } else {
                        // Ease ~28% of the remaining distance per frame (~120 ms).
                        vadj.set_value(cur + diff * 0.28);
                        glib::ControlFlow::Continue
                    }
                });
            });
        }

        // Inertial scrolling: drive the vadjustment ourselves with a decaying
        // velocity so the wheel/touchpad glides to a stop instead of halting
        // instantly. (Constants are tuned for a gentle deceleration.)
        {
            const STEP: f64 = 16.0; // px of velocity added per scroll notch
            const MAX_V: f64 = 110.0; // clamp so a fast flick doesn't fling wildly
            const FRICTION: f64 = 0.85; // per-frame velocity decay
            let velocity = Rc::new(Cell::new(0.0f64));
            let ticking = Rc::new(Cell::new(false));
            let ctrl = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            let scrolled_t = scrolled.clone();
            ctrl.connect_scroll(move |_, _dx, dy| {
                let v = (velocity.get() + dy * STEP).clamp(-MAX_V, MAX_V);
                velocity.set(v);
                if !ticking.get() {
                    ticking.set(true);
                    let velocity = velocity.clone();
                    let ticking = ticking.clone();
                    let vadj = scrolled_t.vadjustment();
                    scrolled_t.add_tick_callback(move |_, _clock| {
                        let v = velocity.get();
                        let max = (vadj.upper() - vadj.page_size()).max(vadj.lower());
                        let nv = (vadj.value() + v).clamp(vadj.lower(), max);
                        vadj.set_value(nv);
                        velocity.set(v * FRICTION);
                        let hit_edge = (nv <= vadj.lower() && v < 0.0) || (nv >= max && v > 0.0);
                        if v.abs() < 0.5 || hit_edge {
                            velocity.set(0.0);
                            ticking.set(false);
                            glib::ControlFlow::Break
                        } else {
                            glib::ControlFlow::Continue
                        }
                    });
                }
                glib::Propagation::Stop
            });
            scrolled.add_controller(ctrl);
        }

        // --- composer: reply banner + text entry + mic + send -----------------
        let on_send: SendCb = Rc::new(RefCell::new(None));
        let on_send_audio: SendAudioCb = Rc::new(RefCell::new(None));
        let on_send_files: SendFilesCb = Rc::new(RefCell::new(None));
        let recorder: Rc<RefCell<Option<Recorder>>> = Rc::new(RefCell::new(None));
        let reply: ReplyState = Rc::new(RefCell::new(None));

        // Reply banner (shown above the entry while quoting a message).
        let reply_banner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .build();
        reply_banner.add_css_class("reply-banner");
        reply_banner.set_visible(false);
        let reply_banner_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let reply_cancel = gtk::Button::from_icon_name("window-close-symbolic");
        reply_cancel.add_css_class("flat");
        {
            let reply = reply.clone();
            let reply_banner = reply_banner.clone();
            reply_cancel.connect_clicked(move |_| {
                *reply.borrow_mut() = None;
                reply_banner.set_visible(false);
            });
        }
        reply_banner.append(&reply_banner_label);
        reply_banner.append(&reply_cancel);

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Scrivi un messaggio")
            .build();
        let mic = gtk::Button::from_icon_name("audio-input-microphone-symbolic");
        mic.add_css_class("flat");
        mic.add_css_class("mic-big");
        let send = gtk::Button::from_icon_name("document-send-symbolic");
        send.add_css_class("flat");
        send.add_css_class("send-icon");
        send.set_visible(false);
        // Paperclip: stage image/document attachments.
        let paperclip = gtk::Button::from_icon_name("mail-attachment-symbolic");
        paperclip.add_css_class("flat");
        paperclip.add_css_class("mic-big");

        // Pending attachments + the preview strip above the composer.
        let pending: Rc<RefCell<Vec<PendingAttachment>>> = Rc::new(RefCell::new(Vec::new()));
        let attach_strip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .build();
        attach_strip.add_css_class("attach-strip");
        attach_strip.set_visible(false);

        // The send button shows when there's text OR a staged attachment.
        let update_send_vis: Rc<dyn Fn()> = {
            let entry = entry.clone();
            let send = send.clone();
            let mic = mic.clone();
            let pending = pending.clone();
            let recorder = recorder.clone();
            Rc::new(move || {
                let has = !entry.text().trim().is_empty() || !pending.borrow().is_empty();
                let recording = recorder.borrow().is_some();
                send.set_visible(has);
                mic.set_visible(!has && !recording);
            })
        };
        {
            let update = update_send_vis.clone();
            entry.connect_changed(move |_| update());
        }

        // Rebuild the preview strip from `pending` (late-bound so each ✕ can call it).
        let rebuild_holder: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let rebuild_attach: Rc<dyn Fn()> = {
            let strip = attach_strip.clone();
            let pending = pending.clone();
            let update = update_send_vis.clone();
            let holder = rebuild_holder.clone();
            Rc::new(move || {
                while let Some(c) = strip.first_child() {
                    strip.remove(&c);
                }
                let items = pending.borrow().clone();
                strip.set_visible(!items.is_empty());
                for (i, att) in items.iter().enumerate() {
                    let item = build_attach_item(att);
                    let x = gtk::Button::from_icon_name("window-close-symbolic");
                    x.add_css_class("attach-x");
                    x.set_halign(gtk::Align::End);
                    x.set_valign(gtk::Align::Start);
                    {
                        let pending = pending.clone();
                        let holder = holder.clone();
                        x.connect_clicked(move |_| {
                            {
                                let mut p = pending.borrow_mut();
                                if i < p.len() {
                                    p.remove(i);
                                }
                            }
                            if let Some(rb) = holder.borrow().as_ref() {
                                rb();
                            }
                        });
                    }
                    item.add_overlay(&x);
                    strip.append(&item);
                }
                update();
            })
        };
        *rebuild_holder.borrow_mut() = Some(rebuild_attach.clone());

        // Stage one attachment from bytes (picker / DnD / paste all funnel here).
        let add_attach: Rc<dyn Fn(Vec<u8>, String, String)> = {
            let pending = pending.clone();
            let rebuild = rebuild_attach.clone();
            Rc::new(move |data, mime: String, name| {
                let is_image = mime.starts_with("image/");
                pending
                    .borrow_mut()
                    .push(PendingAttachment { data, mime, name, is_image });
                rebuild();
            })
        };
        // Read gio files and stage them (picker + DnD share this).
        let add_files: Rc<dyn Fn(Vec<gtk::gio::File>)> = {
            let add_attach = add_attach.clone();
            Rc::new(move |files| {
                for f in files {
                    let Some(path) = f.path() else { continue };
                    let Ok(data) = std::fs::read(&path) else { continue };
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();
                    add_attach(data, guess_mime(&name, &[]), name);
                }
            })
        };
        // Paperclip → multi-file picker.
        {
            let add_files = add_files.clone();
            paperclip.connect_clicked(move |btn| {
                let dialog = gtk::FileDialog::builder().title("Allega file").build();
                let win = btn.root().and_downcast::<gtk::Window>();
                let add_files = add_files.clone();
                dialog.open_multiple(win.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(list) = res {
                        let mut files = Vec::new();
                        for i in 0..list.n_items() {
                            if let Some(f) = list.item(i).and_downcast::<gtk::gio::File>() {
                                files.push(f);
                            }
                        }
                        add_files(files);
                    }
                });
            });
        }

        // Send on Enter / the Send button: attachments first (with caption), else text.
        let do_send = {
            let entry = entry.clone();
            let on_send = on_send.clone();
            let on_send_files = on_send_files.clone();
            let reply = reply.clone();
            let reply_banner = reply_banner.clone();
            let pending = pending.clone();
            let rebuild = rebuild_attach.clone();
            move || {
                let text = entry.text().to_string();
                let atts: Vec<PendingAttachment> = pending.borrow_mut().drain(..).collect();
                if atts.is_empty() && text.trim().is_empty() {
                    return;
                }
                let quote = reply
                    .borrow_mut()
                    .take()
                    .map(|(id, sender, _name, body)| (id, sender, body));
                reply_banner.set_visible(false);
                if atts.is_empty() {
                    if let Some(cb) = on_send.borrow().as_ref() {
                        cb(text, quote);
                    }
                } else {
                    let caption = (!text.trim().is_empty()).then(|| text.clone());
                    let files: Vec<(Vec<u8>, String, String, bool)> = atts
                        .into_iter()
                        .map(|a| (a.data, a.mime, a.name, a.is_image))
                        .collect();
                    if let Some(cb) = on_send_files.borrow().as_ref() {
                        cb(files, caption, quote);
                    }
                    rebuild();
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

        // Cancel-recording button (trash), shown only while recording.
        let cancel_rec = gtk::Button::from_icon_name("user-trash-symbolic");
        cancel_rec.add_css_class("flat");
        cancel_rec.set_visible(false);

        // Restores the composer when a recording ends (whether sent or cancelled).
        let restore_rec_ui: Rc<dyn Fn()> = {
            let mic_btn = mic.clone();
            let entry_w = entry.clone();
            let rec_area_w = rec_area.clone();
            let rec_time_w = rec_time.clone();
            let cancel_w = cancel_rec.clone();
            Rc::new(move || {
                mic_btn.remove_css_class("send-icon");
                mic_btn.set_icon_name("audio-input-microphone-symbolic");
                cancel_w.set_visible(false);
                rec_area_w.set_visible(false);
                rec_time_w.set_visible(false);
                entry_w.set_visible(true);
            })
        };

        // Cancel: stop the recorder and discard the take (no send).
        {
            let recorder = recorder.clone();
            let restore = restore_rec_ui.clone();
            cancel_rec.connect_clicked(move |_| {
                let rec = recorder.borrow_mut().take();
                restore();
                if let Some(rec) = rec {
                    let _ = rec.stop();
                }
            });
        }

        // Mic button toggles a voice-note recording; while recording it shows the
        // send icon and the second tap stops & sends.
        {
            let recorder = recorder.clone();
            let on_send_audio = on_send_audio.clone();
            let mic_btn = mic.clone();
            let entry_w = entry.clone();
            let rec_area_w = rec_area.clone();
            let rec_time_w = rec_time.clone();
            let rec_levels = rec_levels.clone();
            let reply = reply.clone();
            let reply_banner_w = reply_banner.clone();
            let cancel_w = cancel_rec.clone();
            let restore = restore_rec_ui.clone();
            mic.connect_clicked(move |_| {
                let recording = recorder.borrow().is_some();
                if recording {
                    let rec = recorder.borrow_mut().take();
                    restore();
                    // Pull the active quote (id, sender jid, body) and clear the
                    // banner, so a recorded reply carries the citation like text does.
                    let quote = reply
                        .borrow_mut()
                        .take()
                        .map(|(id, sender, _name, body)| (id, sender, body));
                    reply_banner_w.set_visible(false);
                    if let Some(rec) = rec {
                        match rec.stop() {
                            Ok((ogg, secs, waveform)) => {
                                if let Some(cb) = on_send_audio.borrow().as_ref() {
                                    cb(ogg, secs, waveform, quote);
                                }
                            }
                            Err(e) => log::warn!("voice note failed: {e:?}"),
                        }
                    }
                } else {
                    match Recorder::start() {
                        Ok(rec) => {
                            *recorder.borrow_mut() = Some(rec);
                            mic_btn.add_css_class("send-icon");
                            mic_btn.set_icon_name("document-send-symbolic");
                            cancel_w.set_visible(true);
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
        composer.append(&paperclip);
        composer.append(&entry);
        composer.append(&cancel_rec);
        composer.append(&rec_area);
        composer.append(&rec_time);
        composer.append(&mic);
        composer.append(&send);

        // Drag & drop files onto the conversation to stage them.
        {
            let add_files = add_files.clone();
            let drop = gtk::DropTarget::new(
                gtk::gdk::FileList::static_type(),
                gtk::gdk::DragAction::COPY,
            );
            drop.connect_drop(move |_, value, _, _| {
                if let Ok(list) = value.get::<gtk::gdk::FileList>() {
                    add_files(list.files());
                    return true;
                }
                false
            });
            scrolled.add_controller(drop);
        }
        // Ctrl+V: paste an image (or files) from the clipboard as an attachment.
        {
            let add_attach = add_attach.clone();
            let add_files = add_files.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |ctrl, key, _, mods| {
                if mods.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                    && key == gtk::gdk::Key::v
                {
                    let Some(w) = ctrl.widget() else {
                        return glib::Propagation::Proceed;
                    };
                    let clip = w.clipboard();
                    {
                        let add_attach = add_attach.clone();
                        clip.read_texture_async(gtk::gio::Cancellable::NONE, move |res| {
                            if let Ok(Some(tex)) = res {
                                let bytes = tex.save_to_png_bytes();
                                add_attach(
                                    bytes.to_vec(),
                                    "image/png".to_string(),
                                    "incollata.png".to_string(),
                                );
                            }
                        });
                    }
                    {
                        let add_files = add_files.clone();
                        clip.read_value_async(
                            gtk::gdk::FileList::static_type(),
                            glib::Priority::DEFAULT,
                            gtk::gio::Cancellable::NONE,
                            move |res| {
                                if let Ok(val) = res {
                                    if let Ok(list) = val.get::<gtk::gdk::FileList>() {
                                        add_files(list.files());
                                    }
                                }
                            },
                        );
                    }
                }
                glib::Propagation::Proceed
            });
            entry.add_controller(keys);
        }

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&overlay);
        root.append(&reply_banner);
        root.append(&attach_strip);
        root.append(&composer);

        Self {
            root,
            scrolled,
            stick_bottom,
            list,
            is_group: Rc::new(Cell::new(false)),
            oldest,
            loading_older,
            exhausted,
            on_load_older,
            avatars: avatars.clone(),
            senders: Rc::new(RefCell::new(HashMap::new())),
            on_need_avatar: Rc::new(RefCell::new(None)),
            on_open_profile: Rc::new(RefCell::new(None)),
            on_open_media: Rc::new(RefCell::new(None)),
            on_load_inline: Rc::new(RefCell::new(None)),
            pending,
            rebuild_attach,
            on_send_files,
            media_pics: Rc::new(RefCell::new(HashMap::new())),
            ticks: Rc::new(RefCell::new(HashMap::new())),
            on_send,
            on_send_audio,
            on_play: Rc::new(RefCell::new(None)),
            player: Rc::new(RefCell::new(None)),
            entry,
            playing_id: Rc::new(RefCell::new(None)),
            audio_widgets: Rc::new(RefCell::new(HashMap::new())),
            reply,
            reply_banner,
            reply_banner_label,
            last_day: Rc::new(RefCell::new(None)),
        }
    }

    /// A clonable callback that begins composing a reply to a message: records the
    /// quote and shows the banner above the composer. Used by the bubble gestures.
    fn reply_starter(&self) -> Rc<dyn Fn(String, String, String, String)> {
        let reply = self.reply.clone();
        let banner = self.reply_banner.clone();
        let label = self.reply_banner_label.clone();
        let entry = self.entry.clone();
        Rc::new(move |id, sender_jid, name, body| {
            let preview: String = body.chars().take(80).collect();
            label.set_markup(&format!(
                "<b>↩ {}</b>\n{}",
                glib::markup_escape_text(if name.is_empty() { "Risposta" } else { &name }),
                glib::markup_escape_text(&preview),
            ));
            *reply.borrow_mut() = Some((id, sender_jid, name, body));
            banner.set_visible(true);
            entry.grab_focus();
        })
    }

    /// A cached avatar texture for `jid`, if one has been downloaded (used by the
    /// conversation header).
    pub fn avatar_texture(&self, jid: &str) -> Option<gtk::gdk::Texture> {
        self.avatars.borrow().get(jid).cloned()
    }

    /// Puts the keyboard focus in the composer so the user can type right away.
    pub fn focus_composer(&self) {
        self.entry.grab_focus();
    }

    /// Registers the callback invoked with the composed text (and an optional quote
    /// `(quoted_id, quoted_sender_jid, quoted_body)`) when the user sends.
    pub fn connect_send<F: Fn(String, Option<(String, String, String)>) + 'static>(&self, f: F) {
        *self.on_send.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(ogg_bytes, duration_secs)` for a
    /// recorded voice note.
    pub fn connect_send_audio<
        F: Fn(Vec<u8>, u32, Vec<u8>, Option<(String, String, String)>) + 'static,
    >(
        &self,
        f: F,
    ) {
        *self.on_send_audio.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with the staged attachments
    /// `(data, mime, name, is_image)`, an optional caption and reply quote.
    #[allow(clippy::type_complexity)]
    pub fn connect_send_files<
        F: Fn(
                Vec<(Vec<u8>, String, String, bool)>,
                Option<String>,
                Option<(String, String, String)>,
            ) + 'static,
    >(
        &self,
        f: F,
    ) {
        *self.on_send_files.borrow_mut() = Some(Box::new(f));
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
        let mut prev_day: Option<i64> = None;
        for m in messages {
            let k = day_key(m.ts);
            if prev_day != Some(k) {
                self.list.append(&date_separator(m.ts));
                prev_day = Some(k);
            }
            self.list.append(&self.bubble(m));
        }
        *self.last_day.borrow_mut() = prev_day;
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

        // The list always begins with a date separator. If the newest message in
        // this older page falls on the same day as the current top message, that
        // existing separator becomes redundant (the day now continues upward), so
        // drop it before prepending — the page below rebuilds it at the right spot.
        let old_top_day = self.oldest.borrow().as_ref().map(|(ts, _)| day_key(*ts));
        let page_last_day = messages.last().map(|m| day_key(m.ts));
        if old_top_day.is_some() && old_top_day == page_last_day {
            if let Some(sep) = self.list.first_child() {
                self.list.remove(&sep);
            }
        }

        // Build the page oldest-first with a separator before every day change
        // (including the very first message), then prepend in reverse so the final
        // top-to-bottom order is correct.
        let mut widgets: Vec<gtk::Widget> = Vec::new();
        let mut prev_day: Option<i64> = None;
        for m in messages {
            let k = day_key(m.ts);
            if prev_day != Some(k) {
                widgets.push(date_separator(m.ts));
                prev_day = Some(k);
            }
            widgets.push(self.bubble(m).upcast());
        }
        for w in widgets.iter().rev() {
            self.list.prepend(w);
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
        // Check the scroll position BEFORE adding the bubble (afterwards `upper`
        // has already grown and the test would always read "not at bottom").
        let vadj = self.scrolled.vadjustment();
        let at_bottom = vadj.value() + vadj.page_size() >= vadj.upper() - 40.0;
        let k = day_key(m.ts);
        if *self.last_day.borrow() != Some(k) {
            self.list.append(&date_separator(m.ts));
            *self.last_day.borrow_mut() = Some(k);
        }
        self.list.append(&self.bubble(m));
        // Our own sends always jump to the bottom; a received message follows down
        // only if the user was already there (don't yank someone reading history).
        if m.from_me || at_bottom {
            self.scroll_to_bottom();
        }
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

    /// Registers the callback invoked (with a participant JID) when a group
    /// message's sender avatar or name is clicked, to open their profile.
    pub fn connect_open_profile<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_open_profile.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(chat_jid, id, kind)` when a media
    /// thumbnail (photo/video) is clicked, to download + open it.
    pub fn connect_open_media<F: Fn(String, String, i32) + 'static>(&self, f: F) {
        *self.on_open_media.borrow_mut() = Some(Box::new(f));
    }

    /// Registers the callback invoked with `(chat_jid, id)` to download a photo for
    /// inline display in its bubble.
    pub fn connect_load_inline<F: Fn(String, String) + 'static>(&self, f: F) {
        *self.on_load_inline.borrow_mut() = Some(Box::new(f));
    }

    /// Fills a photo bubble with the downloaded image, sizing it to the image's
    /// aspect ratio.
    pub fn set_inline(&self, id: &str, path: &str) {
        if let Some(pic) = self.media_pics.borrow().get(id) {
            // Load a downscaled texture (long edge ≤ 512 px ≈ ~1 MB in RAM) instead
            // of the full-resolution file (a photo can be 7-8 MB of RGBA): the bubble
            // only ever displays it at ≤ 280×360. The full-res file stays on disk for
            // the media viewer to load on demand.
            if let Some(tex) = scaled_texture(path, 512) {
                size_picture(pic, tex.width(), tex.height());
                pic.set_paintable(Some(&tex));
                pic.remove_css_class("media-loading");
            }
        }
    }

    /// Attaches a click handler to `widget` that opens the profile of `jid`.
    fn make_profile_clickable(&self, widget: &impl IsA<gtk::Widget>, jid: &str) {
        widget.set_cursor_from_name(Some("pointer"));
        let click = gtk::GestureClick::new();
        let cb = self.on_open_profile.clone();
        let jid = jid.to_string();
        click.connect_released(move |_, _, _, _| {
            if let Some(f) = cb.borrow().as_ref() {
                f(jid.clone());
            }
        });
        widget.add_controller(click);
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
        *self.last_day.borrow_mut() = None;
        self.loading_older.set(false);
        self.exhausted.set(false);
        self.senders.borrow_mut().clear();
        self.ticks.borrow_mut().clear();
        self.audio_widgets.borrow_mut().clear();
        self.media_pics.borrow_mut().clear();
        // Drop any staged attachments when switching chats.
        self.pending.borrow_mut().clear();
        (self.rebuild_attach)();
        if let Some(p) = self.player.borrow_mut().take() {
            p.pause();
        }
        *self.playing_id.borrow_mut() = None;
        *self.reply.borrow_mut() = None;
        self.reply_banner.set_visible(false);
    }

    /// Advances the ✓/✓✓ glyph of one of our sent bubbles when a receipt lands
    /// (no-op if that message isn't currently on screen).
    pub fn set_status(&self, id: &str, status: i32) {
        if let Some(tick) = self.ticks.borrow().get(id) {
            tick.set_label(status_glyph(status));
            if status >= 3 && !crate::config::HIDE_READ_RECEIPTS {
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
        // A photo/video with no caption is the image itself (no background); with a
        // caption it keeps the chat bubble so the caption sits in it. Other kinds
        // always get the normal chrome.
        let media_img = m.media_kind == 1 || m.media_kind == 2;
        let caption = m
            .body
            .split_once(": ")
            .map(|(_, c)| c.to_string())
            .filter(|c| !c.is_empty());
        let bubbleless = media_img && caption.is_none();
        if !bubbleless {
            bubble.add_css_class(if m.from_me { "bubble-out" } else { "bubble-in" });
        }

        // Sender label for incoming group messages: resolved name, else number.
        if self.is_group.get() && !m.from_me && !m.sender_jid.is_empty() {
            let label = if m.sender_name.is_empty() {
                preview::pretty_number(&m.sender_jid)
            } else {
                m.sender_name.clone()
            };
            let sender = gtk::Label::builder().label(label).xalign(0.0).build();
            sender.add_css_class("caption-heading");
            self.make_profile_clickable(&sender, &m.sender_jid);
            bubble.append(&sender);
        }

        // Quoted message (reply): a left-barred block with the author + preview.
        if !m.reply_text.is_empty() {
            let quote = gtk::Box::new(gtk::Orientation::Vertical, 0);
            quote.add_css_class("reply-quote");
            if !m.reply_sender_name.is_empty() {
                let qs = gtk::Label::builder()
                    .label(&m.reply_sender_name)
                    .xalign(0.0)
                    .build();
                qs.add_css_class("caption-heading");
                quote.append(&qs);
            }
            let preview: String = m.reply_text.chars().take(120).collect();
            let qt = gtk::Label::builder()
                .label(&preview)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(40)
                .build();
            qt.add_css_class("caption");
            qt.add_css_class("dim-label");
            quote.append(&qt);
            bubble.append(&quote);
        }

        if m.media_kind == 1 || m.media_kind == 2 {
            // Photo/video: the image IS the bubble (iMessage style — no chrome,
            // rounded corners, sized to the image's aspect ratio). Photos auto-load
            // their full image inline (history-sync strips the thumbnail); a click
            // opens the full viewer. Videos show a thumbnail+play and load on click.
            let kind = m.media_kind;
            let pic = gtk::Picture::new();
            pic.set_can_shrink(true);
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_halign(gtk::Align::Start);
            pic.add_css_class("media-thumb");
            // Seed with the embedded thumbnail if present (live messages), else a
            // neutral loading box until the inline image arrives.
            if !m.media_thumb.is_empty() {
                let bytes = gtk::glib::Bytes::from(&m.media_thumb);
                if let Ok(tex) = gtk::gdk::Texture::from_bytes(&bytes) {
                    size_picture(&pic, tex.width(), tex.height());
                    pic.set_paintable(Some(&tex));
                } else {
                    pic.set_size_request(240, 180);
                }
            } else {
                pic.set_size_request(240, 180);
                pic.add_css_class("media-loading");
            }

            let frame = gtk::Overlay::new();
            frame.set_child(Some(&pic));
            frame.add_css_class("media-thumb");
            if kind == 2 {
                let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
                play.set_pixel_size(48);
                play.add_css_class("media-play-overlay");
                play.set_halign(gtk::Align::Center);
                play.set_valign(gtk::Align::Center);
                play.set_can_target(false);
                frame.add_overlay(&play);
            }
            // A caption-less image carries the time (+ tick) overlaid on the image;
            // a captioned one uses the normal time row under the caption instead.
            if bubbleless {
                let stamp = if m.from_me {
                    format!("{} {}", format_time(m.ts), status_glyph(m.status))
                } else {
                    format_time(m.ts)
                };
                let time_chip = gtk::Label::new(Some(stamp.trim()));
                time_chip.add_css_class("media-time");
                time_chip.set_halign(gtk::Align::End);
                time_chip.set_valign(gtk::Align::End);
                time_chip.set_margin_end(6);
                time_chip.set_margin_bottom(6);
                time_chip.set_can_target(false);
                frame.add_overlay(&time_chip);
            }

            frame.set_cursor_from_name(Some("pointer"));
            {
                let cb = self.on_open_media.clone();
                let chat = m.chat_jid.clone();
                let id = m.id.clone();
                let click = gtk::GestureClick::new();
                click.connect_released(move |_, _, _, _| {
                    if let Some(f) = cb.borrow().as_ref() {
                        f(chat.clone(), id.clone(), kind);
                    }
                });
                frame.add_controller(click);
            }
            bubble.append(&frame);

            // Track the picture so the inline image can fill it once downloaded,
            // and request that download for photos.
            self.media_pics.borrow_mut().insert(m.id.clone(), pic);
            if kind == 1 {
                if let Some(cb) = self.on_load_inline.borrow().as_ref() {
                    cb(m.chat_jid.clone(), m.id.clone());
                }
            }

            // Caption under the image (inside the bubble), if any.
            if let Some(cap) = &caption {
                let lbl = gtk::Label::builder()
                    .label(cap)
                    .xalign(0.0)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .max_width_chars(42)
                    .selectable(true)
                    .margin_top(4)
                    .build();
                bubble.append(&lbl);
            }
        } else if m.audio {
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
        } else if m.media_kind == 4 {
            // Document: icon + name + size; a click downloads it and opens it in the
            // system's default app.
            let icon = gtk::Image::from_icon_name(doc_icon(&m.media_mime));
            icon.set_pixel_size(36);
            icon.set_valign(gtk::Align::Center);

            let name = if m.media_name.is_empty() {
                "Documento".to_string()
            } else {
                m.media_name.clone()
            };
            let title = gtk::Label::builder()
                .label(&name)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(28)
                .build();
            title.add_css_class("heading");
            let sub = gtk::Label::builder()
                .label(fmt_bytes(m.media_size))
                .xalign(0.0)
                .build();
            sub.add_css_class("caption");
            sub.add_css_class("dim-label");
            let info = gtk::Box::new(gtk::Orientation::Vertical, 0);
            info.set_valign(gtk::Align::Center);
            info.append(&title);
            info.append(&sub);

            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(10)
                .build();
            row.append(&icon);
            row.append(&info);
            row.set_cursor_from_name(Some("pointer"));
            {
                let cb = self.on_open_media.clone();
                let chat = m.chat_jid.clone();
                let id = m.id.clone();
                let click = gtk::GestureClick::new();
                click.connect_released(move |_, _, _, _| {
                    if let Some(f) = cb.borrow().as_ref() {
                        f(chat.clone(), id.clone(), 4);
                    }
                });
                row.add_controller(click);
            }
            bubble.append(&row);
        } else {
            // Render URLs as clickable links (markup). Build with the already-escaped
            // markup as the label so GTK never tries to parse the raw text (a bare
            // `&` in a URL would otherwise log a markup error).
            let markup = crate::util::text::linkify(&m.body);
            let text = gtk::Label::builder()
                .label(&markup)
                .use_markup(true)
                .xalign(0.0)
                .wrap(true)
                // WordChar so a long unbreakable URL wraps by character instead of
                // forcing a huge minimum width on the bubble (and the window).
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(42)
                .selectable(true)
                .build();
            // Open links in the default browser, but SILENCE the child's
            // stdout/stderr so the browser's own logs don't leak into our terminal.
            // gio::Subprocess reaps the child itself (no zombies); `xdg-open` routes
            // to the user's default handler.
            text.connect_activate_link(|_label, uri| {
                use std::ffi::OsStr;
                let res = gtk::gio::Subprocess::newv(
                    &[OsStr::new("xdg-open"), OsStr::new(uri)],
                    gtk::gio::SubprocessFlags::STDOUT_SILENCE
                        | gtk::gio::SubprocessFlags::STDERR_SILENCE,
                );
                if let Err(e) = res {
                    log::warn!("open link failed: {e}");
                }
                glib::Propagation::Stop
            });
            bubble.append(&text);
        }

        // Photo/video already carry the time (+ tick) overlaid on the image.
        if !media_img {
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
                if m.status >= 3 && !crate::config::HIDE_READ_RECEIPTS {
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
        }

        // Reply triggers: double-click on the bubble, or right-click → "Rispondi".
        {
            let start = self.reply_starter();
            let disp = if m.from_me {
                "Tu".to_string()
            } else if !m.sender_name.is_empty() {
                m.sender_name.clone()
            } else {
                preview::pretty_number(&m.sender_jid)
            };
            let trigger: Rc<dyn Fn()> = {
                let id = m.id.clone();
                let sender = m.sender_jid.clone();
                let body = m.body.clone();
                Rc::new(move || start(id.clone(), sender.clone(), disp.clone(), body.clone()))
            };
            let dbl = gtk::GestureClick::new();
            {
                let trigger = trigger.clone();
                dbl.connect_pressed(move |_, n, _, _| {
                    if n >= 2 {
                        trigger();
                    }
                });
            }
            bubble.add_controller(dbl);
            let menu = gtk::GestureClick::new();
            menu.set_button(3);
            menu.set_propagation_phase(gtk::PropagationPhase::Capture);
            {
                let trigger = trigger.clone();
                let bubble_w = bubble.clone();
                menu.connect_pressed(move |g, _, x, y| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    let pop = gtk::Popover::builder().has_arrow(false).build();
                    pop.set_parent(&bubble_w);
                    pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                    let btn = gtk::Button::with_label("↩ Rispondi");
                    btn.add_css_class("flat");
                    {
                        let trigger = trigger.clone();
                        let pop = pop.clone();
                        btn.connect_clicked(move |_| {
                            trigger();
                            pop.popdown();
                        });
                    }
                    pop.set_child(Some(&btn));
                    pop.connect_closed(|p| p.unparent());
                    pop.popup();
                });
            }
            bubble.add_controller(menu);
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
            self.make_profile_clickable(&avatar, &m.sender_jid);
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
        // Pin to the bottom and defer until after layout so `upper` reflects the
        // new content (the vadjustment `changed` handler keeps us there as it grows).
        self.stick_bottom.set(true);
        let vadj = self.scrolled.vadjustment();
        glib::idle_add_local_once(move || {
            vadj.set_value(vadj.upper() - vadj.page_size());
        });
    }
}

/// The check glyph for an outgoing message's delivery status: 1 sent (✓), 2
/// delivered and 3 read (✓✓ — colour distinguishes read). 0/other → none.
/// Builds one preview-strip item (a 64×64 thumbnail or document tile) for a
/// staged attachment, as a `gtk::Overlay` the caller adds the ✕ button onto.
fn build_attach_item(att: &PendingAttachment) -> gtk::Overlay {
    let inner: gtk::Widget = if att.is_image {
        let pic = gtk::Picture::new();
        pic.set_size_request(64, 64);
        pic.set_content_fit(gtk::ContentFit::Cover);
        let bytes = gtk::glib::Bytes::from(&att.data);
        if let Ok(tex) = gtk::gdk::Texture::from_bytes(&bytes) {
            pic.set_paintable(Some(&tex));
        }
        pic.upcast()
    } else {
        let icon = gtk::Image::from_icon_name(doc_icon(&att.mime));
        icon.set_pixel_size(28);
        let label = gtk::Label::builder()
            .label(&att.name)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(8)
            .build();
        label.add_css_class("caption");
        let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
        b.set_size_request(64, 64);
        b.set_halign(gtk::Align::Center);
        b.set_valign(gtk::Align::Center);
        b.append(&icon);
        b.append(&label);
        b.upcast()
    };
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&inner));
    overlay.add_css_class("attach-item");
    overlay
}

/// Best-effort MIME type for a file name (+ optional bytes).
fn guess_mime(name: &str, data: &[u8]) -> String {
    let (ctype, _) = gtk::gio::content_type_guess(Some(name), data);
    gtk::gio::content_type_get_mime_type(&ctype)
        .map(|g| g.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Sizes a media `Picture` to the given intrinsic dimensions, capped to a max
/// width/height so a big photo doesn't blow out the bubble (keeps aspect ratio).
/// Decodes an image file into a texture downscaled so its long edge is at most
/// `max` px, preserving aspect ratio. This keeps only the small (display-sized)
/// bitmap in memory rather than the full-resolution one — the dominant source of
/// RAM use when many photos are on screen. Returns `None` if the file can't be read.
pub(crate) fn scaled_texture(path: &str, max: i32) -> Option<gtk::gdk::Texture> {
    let pb = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, max, max, true).ok()?;
    Some(gtk::gdk::Texture::for_pixbuf(&pb))
}

fn size_picture(pic: &gtk::Picture, w: i32, h: i32) {
    const MAX_W: f64 = 280.0;
    const MAX_H: f64 = 360.0;
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let scale = (MAX_W / w).min(MAX_H / h).min(1.0);
    pic.set_size_request((w * scale).round() as i32, (h * scale).round() as i32);
}

/// A symbolic icon name for a document's MIME type.
pub(crate) fn doc_icon(mime: &str) -> &'static str {
    if mime.contains("pdf") {
        "x-office-document-symbolic"
    } else if mime.contains("zip") || mime.contains("compressed") || mime.contains("tar") {
        "package-x-generic-symbolic"
    } else if mime.starts_with("audio") {
        "audio-x-generic-symbolic"
    } else if mime.starts_with("video") {
        "video-x-generic-symbolic"
    } else if mime.contains("sheet") || mime.contains("excel") || mime.contains("csv") {
        "x-office-spreadsheet-symbolic"
    } else {
        "text-x-generic-symbolic"
    }
}

/// A human-readable byte size, e.g. `1,2 MB` (empty for unknown/zero).
pub(crate) fn fmt_bytes(n: i64) -> String {
    if n <= 0 {
        return String::new();
    }
    let n = n as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if n >= GB {
        format!("{:.1} GB", n / GB).replace('.', ",")
    } else if n >= MB {
        format!("{:.1} MB", n / MB).replace('.', ",")
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

fn status_glyph(status: i32) -> &'static str {
    match status {
        1 => "✓",
        2 | 3 => "✓✓",
        _ => "",
    }
}

/// A stable per-day key (`year*1000 + day-of-year`, local time) used to detect
/// day boundaries between consecutive messages.
fn day_key(ts: i64) -> i64 {
    glib::DateTime::from_unix_local(ts)
        .ok()
        .map(|dt| dt.year() as i64 * 1000 + dt.day_of_year() as i64)
        .unwrap_or(0)
}

/// A human date label for a separator: "Oggi" / "Ieri" / `dd/mm/yyyy` (local).
fn date_label(ts: i64) -> String {
    let dt = match glib::DateTime::from_unix_local(ts) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let key = day_key(ts);
    if let Ok(now) = glib::DateTime::now_local() {
        if key == day_key(now.to_unix()) {
            return "Oggi".to_string();
        }
        if let Ok(yest) = now.add_days(-1) {
            if key == day_key(yest.to_unix()) {
                return "Ieri".to_string();
            }
        }
    }
    dt.format("%d/%m/%Y")
        .map(|g| g.to_string())
        .unwrap_or_default()
}

/// Builds a centered date separator pill for the given timestamp.
fn date_separator(ts: i64) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(date_label(ts))
        .halign(gtk::Align::Center)
        .build();
    label.add_css_class("date-sep");
    label.upcast()
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
