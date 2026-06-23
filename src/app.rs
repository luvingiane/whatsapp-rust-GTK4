//! Wires everything together: creates the libadwaita application, spawns the
//! Tokio backend, and bridges backend events to the UI on the GTK main loop.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use libadwaita as adw;

use crate::backend::{self, WaCommand, WaEvent};
use crate::config;
use crate::ui::window::MainWindow;
use crate::util::qr;

/// Builds the application and runs the GTK main loop. Returns the process exit
/// code.
pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(config::APP_ID)
        .build();

    app.connect_activate(on_activate);
    app.run()
}

fn on_activate(app: &adw::Application) {
    load_css();
    let win = MainWindow::new(app);

    // Resolve DB paths up front so we can fail loudly in the UI.
    let session_db = match config::session_db_path() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            return fail(
                &win,
                &format!("Impossibile creare il database sessione: {e}"),
            )
        }
    };
    let app_db = match config::app_db_path() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return fail(&win, &format!("Impossibile creare il database app: {e}")),
    };

    // Create the bridge channels and start the backend on its own Tokio thread.
    let chans = backend::channels();
    backend::runtime::spawn(
        session_db,
        app_db,
        chans.event_tx.clone(),
        chans.command_rx.clone(),
    );

    // Tracks which chat is currently open, so history/live messages target it.
    let current_open: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Number of pinned chats (WhatsApp caps at 3); refreshed on each snapshot.
    let pinned_count: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let command_tx = chans.command_tx.clone();

    // Selecting a chat: switch the content pane and ask the backend for history.
    // The active and archived lists share the same open behaviour.
    {
        let win_sel = win.clone();
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.chat_list.connect_open(move |jid, name| {
            *current_open.borrow_mut() = Some(jid.clone());
            win_sel.open_chat(&jid, &name);
            let _ = command_tx.try_send(WaCommand::OpenChat(jid));
        });
    }
    {
        let win_sel = win.clone();
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.archived_list.connect_open(move |jid, name| {
            *current_open.borrow_mut() = Some(jid.clone());
            win_sel.open_chat(&jid, &name);
            let _ = command_tx.try_send(WaCommand::OpenChat(jid));
        });
    }

    // The "Archiviate" entry opens the archived sub-page in the sidebar.
    {
        let win_arch = win.clone();
        win.chat_list
            .connect_open_archived(move || win_arch.open_archived());
    }

    // Right-click menu actions on both lists: bulk archive/unarchive and pin/unpin.
    for list in [&win.chat_list, &win.archived_list] {
        {
            let command_tx = command_tx.clone();
            list.connect_archive(move |jids, archived| {
                let _ = command_tx.try_send(WaCommand::SetArchived { jids, archived });
            });
        }
        {
            let command_tx = command_tx.clone();
            let pinned_count = pinned_count.clone();
            let win_pin = win.clone();
            list.connect_pin(move |jid, pinned| {
                // WhatsApp allows at most 3 pinned chats; refuse the 4th with a note.
                if pinned && pinned_count.get() >= 3 {
                    let dialog = adw::MessageDialog::new(
                        Some(&win_pin.window),
                        Some("Massimo 3 chat tra i preferiti"),
                        Some("Rimuovi una chat dai preferiti per aggiungerne un'altra."),
                    );
                    dialog.add_response("ok", "Ho capito");
                    dialog.set_default_response(Some("ok"));
                    dialog.present();
                    return;
                }
                let _ = command_tx.try_send(WaCommand::SetPinned { jid, pinned });
            });
        }
    }

    // Scrolled to the top of the open thread: ask for the previous page of the
    // currently open chat (backed by app.db; no network).
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.thread.connect_load_older(move |before_ts, before_id| {
            if let Some(jid) = current_open.borrow().clone() {
                let _ = command_tx.try_send(WaCommand::LoadOlder {
                    jid,
                    before_ts,
                    before_id,
                    count: 200,
                });
            }
        });
    }

    // Avatars: a visible row / group bubble lacking a picture asks the backend to
    // fetch it. Dedup requests so we hit the channel once per JID.
    {
        let requested: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
        let need = {
            let command_tx = command_tx.clone();
            let requested = requested.clone();
            move |jid: String| {
                if requested.borrow_mut().insert(jid.clone()) {
                    let _ = command_tx.try_send(WaCommand::FetchAvatar(jid));
                }
            }
        };
        let need2 = need.clone();
        let need3 = need.clone();
        win.chat_list.connect_need_avatar(need);
        win.thread.connect_need_avatar(need2);
        win.archived_list.connect_need_avatar(need3);
    }

    // Composer: send text / voice note to the currently open chat.
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.thread.connect_send(move |text, quote| {
            if let Some(jid) = current_open.borrow().clone() {
                let quote = quote.map(|(id, sender, body)| backend::ReplyQuote { id, sender, body });
                let _ = command_tx.try_send(WaCommand::SendText { jid, text, quote });
            }
        });
    }
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.thread.connect_send_audio(move |ogg, duration, waveform, quote| {
            if let Some(jid) = current_open.borrow().clone() {
                let quote = quote.map(|(id, sender, body)| backend::ReplyQuote { id, sender, body });
                let _ = command_tx
                    .try_send(WaCommand::SendAudio { jid, ogg, duration, waveform, quote });
            }
        });
    }
    // Attachments: send each staged file as an image or document; the caption
    // (composer text) goes on the first image, or as a separate text otherwise.
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.thread.connect_send_files(move |files, caption, quote| {
            let Some(jid) = current_open.borrow().clone() else {
                return;
            };
            let q = quote.map(|(id, sender, body)| backend::ReplyQuote { id, sender, body });
            let mut caption_left = caption;
            for (data, mime, name, is_image) in files {
                if is_image {
                    let cap = caption_left.take();
                    let _ = command_tx.try_send(WaCommand::SendImage {
                        jid: jid.clone(),
                        data,
                        mime,
                        caption: cap,
                        quote: q.clone(),
                    });
                } else {
                    let _ = command_tx.try_send(WaCommand::SendDocument {
                        jid: jid.clone(),
                        data,
                        mime,
                        file_name: name,
                        quote: q.clone(),
                    });
                }
            }
            if let Some(text) = caption_left {
                if !text.trim().is_empty() {
                    let _ = command_tx.try_send(WaCommand::SendText { jid, text, quote: q });
                }
            }
        });
    }
    // Voice-note playback: the play button asks the backend to fetch + decrypt it.
    {
        let command_tx = command_tx.clone();
        win.thread.connect_play(move |chat_jid, id| {
            let _ = command_tx.try_send(WaCommand::PlayAudio { chat_jid, id });
        });
    }
    // Conversation header click → fetch profile/group info for the panel.
    {
        let command_tx = command_tx.clone();
        win.connect_open_profile(move |jid| {
            let _ = command_tx.try_send(WaCommand::FetchProfile(jid));
        });
    }
    // Clicking a group member's avatar/name → open that person's profile.
    {
        let command_tx = command_tx.clone();
        win.thread.connect_open_profile(move |jid| {
            let _ = command_tx.try_send(WaCommand::FetchProfile(jid));
        });
    }
    // Clicking a photo/video thumbnail → download + open in the media viewer.
    {
        let command_tx = command_tx.clone();
        win.thread.connect_open_media(move |chat_jid, id, _kind| {
            let _ = command_tx.try_send(WaCommand::DownloadMedia { chat_jid, id });
        });
    }
    // A photo bubble needs its image: download it inline (lazy).
    {
        let command_tx = command_tx.clone();
        win.thread.connect_load_inline(move |chat_jid, id| {
            let _ = command_tx.try_send(WaCommand::LoadInline { chat_jid, id });
        });
    }

    // Presence: be "available" only while the window is focused AND the user is
    // active; "unavailable" when unfocused or idle, so the phone resumes its own
    // notifications when we step away. Presence is dropped on reconnect, so we
    // re-assert it on `Connected`.
    let presence = Rc::new(PresenceDriver::new(command_tx.clone()));
    presence.set_focused(win.window.is_active());
    {
        let presence = presence.clone();
        win.window
            .connect_is_active_notify(move |w| presence.set_focused(w.is_active()));
    }
    {
        // Pointer/keyboard activity on the window resets the idle timer.
        let motion = gtk::EventControllerMotion::new();
        {
            let presence = presence.clone();
            motion.connect_motion(move |_, _, _| presence.mark_active());
        }
        win.window.add_controller(motion);
        let key = gtk::EventControllerKey::new();
        {
            let presence = presence.clone();
            key.connect_key_pressed(move |_, _, _, _| {
                presence.mark_active();
                glib::Propagation::Proceed
            });
        }
        win.window.add_controller(key);
    }
    {
        // Periodic idle check: after IDLE without input we go unavailable.
        let presence = presence.clone();
        glib::timeout_add_seconds_local(60, move || {
            presence.tick_idle();
            glib::ControlFlow::Continue
        });
    }

    // Drain backend events on the GTK main loop. `spawn_future_local` guarantees
    // this future runs on the main thread, so it is safe to touch widgets here.
    let win_ev = win.clone();
    let event_rx = chans.event_rx.clone();
    let current_open_ev = current_open.clone();
    let presence_ev = presence.clone();
    let command_tx_ev = command_tx.clone();
    let pinned_count_ev = pinned_count.clone();
    // At most one profile window at a time; opening a new one closes the previous.
    let profile_win: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
    // Likewise for the media viewer and the gallery window.
    let media_win: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
    let media_grid_win: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
    // Photo tiles in the open gallery awaiting an on-demand download (id → Picture),
    // filled when the matching InlineReady arrives.
    let media_grid_tiles: crate::ui::media_grid::TileMap =
        Rc::new(RefCell::new(HashMap::new()));
    glib::spawn_future_local(async move {
        while let Ok(ev) = event_rx.recv().await {
            match ev {
                WaEvent::QrCode(code) => {
                    win_ev.show_login();
                    match qr::qr_texture(&code, 6, 4) {
                        Ok(tex) => win_ev.login.show_qr(&tex),
                        Err(e) => win_ev.login.show_error(&format!("QR non valido: {e}")),
                    }
                }
                // We're past pairing — show the chat UI now. We intentionally do
                // NOT wait for `Connected`: whatsapp-rust withholds it when the
                // post-login critical app-state sync fails (e.g. "didn't find app
                // state key" on a fresh pair), which would otherwise strand the UI
                // on the QR page even though the backend is authenticated and
                // history sync is populating the list.
                WaEvent::PairSuccess { jid } => {
                    win_ev.set_account(Some(&jid));
                    win_ev.show_main();
                }
                WaEvent::Connected { jid } => {
                    win_ev.set_account(jid.as_deref());
                    win_ev.show_main();
                    // Presence is lost across reconnects: re-send the current state.
                    presence_ev.set_focused(win_ev.window.is_active());
                    presence_ev.reassert();
                }
                // Transient drop: whatsapp-rust reconnects on its own; stay put.
                WaEvent::Disconnected => {}
                WaEvent::LoggedOut => {
                    *current_open_ev.borrow_mut() = None;
                    win_ev.chat_list.update(&[]);
                    win_ev.update_archived(&[]);
                    win_ev.reset_content();
                    win_ev.login.show_waiting();
                    win_ev.show_login();
                }
                WaEvent::Error(msg) => win_ev.login.show_error(&msg),
                // Chats arrived (cached at startup, or from history sync). Showing
                // them is enough to consider us "in" — switch to the main view even
                // if `Connected` never came. A QR/LoggedOut event afterwards still
                // takes precedence and sends us back to the login page.
                WaEvent::ChatsSnapshot(chats) => {
                    if !chats.is_empty() {
                        win_ev.show_main();
                    }
                    pinned_count_ev.set(chats.iter().filter(|c| c.pinned).count());
                    win_ev.chat_list.update(&chats);
                }
                // Archived list (and its count) refreshed alongside the active list.
                WaEvent::ArchivedChatsSnapshot(chats) => {
                    win_ev.update_archived(&chats);
                }
                // History/live messages: apply only if their chat is still open.
                WaEvent::ChatHistory { jid, messages } => {
                    if current_open_ev.borrow().as_deref() == Some(jid.as_str()) {
                        win_ev.show_history(&messages);
                    }
                }
                WaEvent::NewMessage(row) => {
                    if current_open_ev.borrow().as_deref() == Some(row.chat_jid.as_str()) {
                        win_ev.append_message(&row);
                    }
                }
                // An older page for the open chat: prepend, preserving scroll.
                WaEvent::OlderHistory { jid, messages } => {
                    if current_open_ev.borrow().as_deref() == Some(jid.as_str()) {
                        win_ev.thread.prepend_history(&messages);
                    }
                }
                // A receipt advanced our sent messages' status: update the open
                // thread's ticks live. The chat-list preview refreshes via the
                // debounced snapshot triggered backend-side.
                WaEvent::ReceiptUpdate {
                    chat_jid,
                    message_ids,
                    status,
                } => {
                    if current_open_ev.borrow().as_deref() == Some(chat_jid.as_str()) {
                        for id in &message_ids {
                            win_ev.thread.set_status(id, status);
                        }
                    }
                }
                // A profile picture landed on disk: decode it and update widgets.
                WaEvent::Avatar { jid, path } => {
                    if let Ok(tex) = gtk::gdk::Texture::from_filename(&path) {
                        win_ev.chat_list.set_avatar(&jid, &tex);
                        win_ev.archived_list.set_avatar(&jid, &tex);
                        win_ev.thread.set_avatar(&jid, &tex);
                    }
                }
                // A voice note finished downloading: play it.
                WaEvent::AudioReady { id, path } => {
                    win_ev.thread.play_audio(&id, &path);
                }
                // A photo's inline image finished downloading: fill its bubble and,
                // if it's showing in the open gallery, that thumbnail too.
                WaEvent::InlineReady { id, path } => {
                    win_ev.thread.set_inline(&id, &path);
                    if let Some(pic) = media_grid_tiles.borrow().get(&id) {
                        if let Some(tex) = crate::ui::thread::scaled_texture(&path, 240) {
                            pic.set_paintable(Some(&tex));
                            pic.remove_css_class("media-loading");
                        }
                    }
                }
                // A photo/video finished downloading: open it in the viewer
                // (replacing any open one).
                WaEvent::MediaReady { kind, path } => {
                    // Documents open in the system's default app; photos/videos use
                    // the in-app viewer.
                    if kind == 4 {
                        use std::ffi::OsStr;
                        let _ = gtk::gio::Subprocess::newv(
                            &[OsStr::new("xdg-open"), OsStr::new(&path)],
                            gtk::gio::SubprocessFlags::STDOUT_SILENCE
                                | gtk::gio::SubprocessFlags::STDERR_SILENCE,
                        );
                        continue;
                    }
                    let old = media_win.borrow_mut().take();
                    if let Some(old) = old {
                        old.close();
                    }
                    let w = crate::ui::media_viewer::present(&win_ev.window, kind, &path);
                    {
                        let media_win = media_win.clone();
                        w.connect_close_request(move |_| {
                            *media_win.borrow_mut() = None;
                            glib::Propagation::Proceed
                        });
                    }
                    *media_win.borrow_mut() = Some(w);
                }
                // Header subtitle: the open chat's online presence.
                WaEvent::PresenceInfo {
                    jid,
                    is_group,
                    online_names,
                    total,
                } => {
                    if current_open_ev.borrow().as_deref() == Some(jid.as_str()) {
                        win_ev.set_presence(is_group, &online_names, total);
                    }
                }
                // Profile/group info arrived: open the panel (replacing any open one).
                WaEvent::Profile {
                    is_group,
                    jid,
                    title,
                    subtitle,
                    status,
                    pic_path,
                    blocked,
                    rows,
                    media_count,
                } => {
                    // Bind in its own statement so the borrow ends before close():
                    // close() fires the close-request handler, which borrows too.
                    let old = profile_win.borrow_mut().take();
                    if let Some(old) = old {
                        old.close();
                    }
                    let data = crate::ui::profile::ProfileData {
                        is_group,
                        jid,
                        title,
                        subtitle,
                        status,
                        pic_path,
                        blocked,
                        rows,
                        media_count,
                    };
                    let on_open: Rc<dyn Fn(String)> = {
                        let tx = command_tx_ev.clone();
                        Rc::new(move |jid: String| {
                            let _ = tx.try_send(WaCommand::FetchProfile(jid));
                        })
                    };
                    let on_block: Rc<dyn Fn(String, bool)> = {
                        let tx = command_tx_ev.clone();
                        Rc::new(move |jid: String, blocked: bool| {
                            let _ = tx.try_send(WaCommand::SetBlocked { jid, blocked });
                        })
                    };
                    let on_media: Rc<dyn Fn(String)> = {
                        let tx = command_tx_ev.clone();
                        Rc::new(move |jid: String| {
                            let _ = tx.try_send(WaCommand::FetchChatMedia(jid));
                        })
                    };
                    let w = crate::ui::profile::present(
                        &win_ev.window,
                        &win_ev.avatars,
                        data,
                        on_open,
                        on_block,
                        on_media,
                    );
                    {
                        let profile_win = profile_win.clone();
                        w.connect_close_request(move |_| {
                            *profile_win.borrow_mut() = None;
                            glib::Propagation::Proceed
                        });
                    }
                    *profile_win.borrow_mut() = Some(w);
                }
                // The chat's media gallery: open it in a tabbed window.
                WaEvent::ChatMedia {
                    jid,
                    photos,
                    videos,
                    documents,
                    links,
                } => {
                    let old = media_grid_win.borrow_mut().take();
                    if let Some(old) = old {
                        old.close();
                    }
                    // Fresh gallery: drop the previous tile registrations.
                    media_grid_tiles.borrow_mut().clear();
                    let on_open: Rc<dyn Fn(String, i32)> = {
                        let tx = command_tx_ev.clone();
                        let jid = jid.clone();
                        Rc::new(move |id: String, _kind: i32| {
                            let _ = tx.try_send(WaCommand::DownloadMedia {
                                chat_jid: jid.clone(),
                                id,
                            });
                        })
                    };
                    // Request an on-demand inline download for empty photo tiles
                    // (deduped + throttled in the backend).
                    let on_need: Rc<dyn Fn(String)> = {
                        let tx = command_tx_ev.clone();
                        let jid = jid.clone();
                        Rc::new(move |id: String| {
                            let _ = tx.try_send(WaCommand::LoadInline {
                                chat_jid: jid.clone(),
                                id,
                            });
                        })
                    };
                    let on_link: Rc<dyn Fn(String)> = Rc::new(move |url: String| {
                        use std::ffi::OsStr;
                        let _ = gtk::gio::Subprocess::newv(
                            &[OsStr::new("xdg-open"), OsStr::new(&url)],
                            gtk::gio::SubprocessFlags::STDOUT_SILENCE
                                | gtk::gio::SubprocessFlags::STDERR_SILENCE,
                        );
                    });
                    let w = crate::ui::media_grid::present(
                        &win_ev.window,
                        photos,
                        videos,
                        documents,
                        links,
                        on_open,
                        on_link,
                        on_need,
                        media_grid_tiles.clone(),
                    );
                    {
                        let media_grid_win = media_grid_win.clone();
                        w.connect_close_request(move |_| {
                            *media_grid_win.borrow_mut() = None;
                            glib::Propagation::Proceed
                        });
                    }
                    *media_grid_win.borrow_mut() = Some(w);
                }
            }
        }
    });

    // Esc: first cancel an active selection, otherwise close the open chat and
    // return to the empty state. (A modal profile grabs Esc first to close itself.)
    {
        let win_esc = win.clone();
        let current_open_esc = current_open.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if win_esc.chat_list.is_selecting() {
                    win_esc.chat_list.cancel_selection();
                    return glib::Propagation::Stop;
                }
                if current_open_esc.borrow().is_some() {
                    *current_open_esc.borrow_mut() = None;
                    win_esc.reset_content();
                    win_esc.chat_list.clear_selection();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        win.window.add_controller(keys);
    }

    // Ask the backend to stop cleanly when the window is closed.
    win.window.connect_close_request(move |_| {
        let _ = command_tx.try_send(WaCommand::Shutdown);
        glib::Propagation::Proceed
    });

    win.window.present();
}

/// How long the window can go without pointer/keyboard input before we report
/// the user as away (presence unavailable), even if the window keeps focus.
const IDLE: Duration = Duration::from_secs(300);

/// Tracks window focus + input idleness and pushes presence to the backend,
/// de-duplicated so we only send on real state changes. Lives on the GTK main
/// thread (`!Send`), driven by focus notifies, input controllers and an idle tick.
struct PresenceDriver {
    tx: async_channel::Sender<WaCommand>,
    focused: Cell<bool>,
    last_input: Cell<Instant>,
    idle: Cell<bool>,
    last_sent: Cell<Option<bool>>,
}

impl PresenceDriver {
    fn new(tx: async_channel::Sender<WaCommand>) -> Self {
        Self {
            tx,
            focused: Cell::new(false),
            last_input: Cell::new(Instant::now()),
            idle: Cell::new(false),
            last_sent: Cell::new(None),
        }
    }

    /// Available only when the window is focused and the user is not idle.
    fn desired(&self) -> bool {
        self.focused.get() && !self.idle.get()
    }

    /// Sends the current desired presence, skipping a redundant resend unless
    /// `force` (used after a reconnect, which drops presence server-side).
    fn apply(&self, force: bool) {
        let available = self.desired();
        if force || self.last_sent.get() != Some(available) {
            self.last_sent.set(Some(available));
            let _ = self.tx.try_send(WaCommand::SetPresence { available });
        }
    }

    fn set_focused(&self, focused: bool) {
        self.focused.set(focused);
        if focused {
            self.last_input.set(Instant::now());
            self.idle.set(false);
        }
        self.apply(false);
    }

    fn mark_active(&self) {
        self.last_input.set(Instant::now());
        if self.idle.get() {
            self.idle.set(false);
            self.apply(false);
        }
    }

    fn tick_idle(&self) {
        let idle = self.last_input.get().elapsed() >= IDLE;
        if idle != self.idle.get() {
            self.idle.set(idle);
            self.apply(false);
        }
    }

    /// Re-send the current state even if unchanged (presence is lost on reconnect).
    fn reassert(&self) {
        self.apply(true);
    }
}

/// Shows a fatal startup error on the login view and presents the window.
fn fail(win: &MainWindow, msg: &str) {
    win.login.show_error(msg);
    win.window.present();
}

/// Loads the small amount of custom CSS we need (the unread badge).
fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".badge { \
            background-color: @accent_bg_color; \
            color: @accent_fg_color; \
            border-radius: 999px; \
            padding: 0px 7px; \
            font-size: 0.8em; \
            font-weight: bold; \
         } \
         .bubble-in, .bubble-out { \
            border-radius: 12px; \
            padding: 6px 10px; \
            margin: 1px 0px; \
         } \
         .bubble-in { background-color: alpha(currentColor, 0.08); } \
         .bubble-out { background-color: alpha(@accent_bg_color, 0.85); color: @accent_fg_color; } \
         .tick { font-size: 0.8em; opacity: 0.85; } \
         .tick-read { color: #53bdeb; opacity: 1; } \
         .reply-quote { \
            border-left: 3px solid alpha(currentColor, 0.5); \
            padding: 1px 6px; margin-bottom: 2px; \
            background-color: alpha(currentColor, 0.06); \
            border-radius: 4px; \
         } \
         .reply-banner { \
            border-left: 3px solid @accent_bg_color; \
            padding: 2px 6px; \
         } \
         .send-icon { color: @accent_bg_color; } \
         .mic-big { -gtk-icon-size: 20px; } \
         .media-thumb { border-radius: 12px; } \
         .media-loading { background-color: alpha(currentColor, 0.10); } \
         .media-play-overlay { color: #ffffff; \
            background-color: alpha(#000000, 0.45); border-radius: 999px; padding: 8px; } \
         .media-time { color: #ffffff; font-size: 0.75em; \
            background-color: alpha(#000000, 0.45); border-radius: 8px; padding: 1px 6px; } \
         .attach-item { border-radius: 8px; background-color: alpha(currentColor, 0.08); } \
         .attach-x { opacity: 0; min-height: 0; min-width: 0; padding: 1px; margin: 1px; \
            color: #ffffff; background-color: alpha(#000000, 0.6); border-radius: 999px; } \
         .attach-item:hover .attach-x { opacity: 1; } \
         .block-btn { \
            background-color: #ff3b30; background-image: none; color: #ffffff; \
            font-size: 0.9em; padding: 2px 14px; min-height: 0; \
         } \
         .block-btn:hover { background-color: #ff5a50; } \
         entry:focus-within { box-shadow: none; outline-color: alpha(currentColor, 0.3); } \
         .date-sep { \
            background-color: alpha(currentColor, 0.08); \
            border-radius: 8px; \
            padding: 2px 10px; \
            margin: 4px 0px; \
            font-size: 0.8em; \
            opacity: 0.75; \
         }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
