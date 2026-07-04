//! Wires everything together: creates the libadwaita application, spawns the
//! Tokio backend, and bridges backend events to the UI on the GTK main loop.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
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
    // Latest active-chat snapshot, kept for the "Inoltra a…" picker.
    let chats_now: Rc<RefCell<Vec<crate::model::ChatSummary>>> = Rc::new(RefCell::new(Vec::new()));
    // Recently opened chats' history pages, so re-opening renders instantly.
    let hist_cache: Rc<RefCell<HistCache>> = Rc::new(RefCell::new(HistCache::new()));
    // Number of pinned chats (WhatsApp caps at 3); refreshed on each snapshot.
    let pinned_count: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let command_tx = chans.command_tx.clone();

    // Selecting a chat: switch the content pane and render from the history
    // cache when possible (the backend still syncs read-state/presence either
    // way). The active and archived lists share the same open behaviour.
    let open_chat_cb: Rc<dyn Fn(String, String)> = {
        let win_sel = win.clone();
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        let hist_cache = hist_cache.clone();
        Rc::new(move |jid: String, name: String| {
            *current_open.borrow_mut() = Some(jid.clone());
            win_sel.open_chat(&jid, &name);
            let hit = {
                let cache = hist_cache.borrow();
                if let Some(msgs) = cache.get(&jid) {
                    win_sel.show_history(msgs);
                    true
                } else {
                    false
                }
            };
            if hit {
                log::info!("history cache hit for {jid}");
                hist_cache.borrow_mut().touch(&jid);
            }
            let _ = command_tx.try_send(WaCommand::OpenChat {
                jid,
                with_history: !hit,
            });
        })
    };
    {
        let open_chat_cb = open_chat_cb.clone();
        win.chat_list
            .connect_open(move |jid, name| open_chat_cb(jid, name));
    }
    {
        let open_chat_cb = open_chat_cb.clone();
        win.archived_list
            .connect_open(move |jid, name| open_chat_cb(jid, name));
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
                if crate::util::jid::is_real_user_jid(&jid)
                    && requested.borrow_mut().insert(jid.clone())
                {
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
    // Edit confirmed from the composer → send the edit to the open chat.
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        win.thread.connect_edit(move |id, new_text| {
            if let Some(jid) = current_open.borrow().clone() {
                let _ = command_tx.try_send(WaCommand::EditMessage { jid, id, new_text });
            }
        });
    }
    // "Elimina" from the message menu → confirm, then delete locally or revoke.
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        let win_del = win.clone();
        win.thread.connect_delete(move |id, can_revoke| {
            let Some(jid) = current_open.borrow().clone() else {
                return;
            };
            present_delete_dialog(&win_del.window, &command_tx, jid, id, can_revoke);
        });
    }
    // Same actions from the full-window media viewer.
    {
        let command_tx = command_tx.clone();
        let win_vw = win.clone();
        win.viewer.connect_delete(move |chat_jid, id, can_revoke| {
            present_delete_dialog(&win_vw.window, &command_tx, chat_jid, id, can_revoke);
        });
    }
    {
        let command_tx = command_tx.clone();
        let win_vw = win.clone();
        let chats_now = chats_now.clone();
        win.viewer.connect_forward(move |chat_jid, id| {
            let command_tx = command_tx.clone();
            present_forward_picker(&win_vw, &chats_now.borrow(), move |to_jid| {
                let _ = command_tx.try_send(WaCommand::ForwardMessage {
                    to_jid,
                    chat_jid: chat_jid.clone(),
                    id: id.clone(),
                });
            });
        });
    }
    // Left/right navigation in the viewer re-fetches the neighbor media.
    {
        let command_tx = command_tx.clone();
        win.viewer.connect_navigate(move |chat_jid, id| {
            let _ = command_tx.try_send(WaCommand::DownloadMedia { chat_jid, id });
        });
    }
    // "Inoltra" from the message menu → chat picker → forward command.
    {
        let command_tx = command_tx.clone();
        let current_open = current_open.clone();
        let win_fwd = win.clone();
        let chats_now = chats_now.clone();
        win.thread.connect_forward(move |id| {
            let Some(chat_jid) = current_open.borrow().clone() else {
                return;
            };
            let command_tx = command_tx.clone();
            present_forward_picker(&win_fwd, &chats_now.borrow(), move |to_jid| {
                let _ = command_tx.try_send(WaCommand::ForwardMessage {
                    to_jid,
                    chat_jid: chat_jid.clone(),
                    id: id.clone(),
                });
            });
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
    let chats_now_ev = chats_now.clone();
    let hist_cache_ev = hist_cache.clone();
    let presence_ev = presence.clone();
    let command_tx_ev = command_tx.clone();
    let pinned_count_ev = pinned_count.clone();
    // Photo tiles in the open gallery awaiting an on-demand download (id → Picture),
    // filled when the matching InlineReady arrives.
    let media_grid_tiles: crate::ui::media_grid::TileMap =
        Rc::new(RefCell::new(HashMap::new()));
    // The side pane's X button hides it.
    {
        let win_x = win.clone();
        win.side_pane
            .connect_close(move || win_x.set_side_pane_visible(false));
    }
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
                    hist_cache_ev.borrow_mut().clear();
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
                    *chats_now_ev.borrow_mut() = chats.clone();
                    win_ev.chat_list.update(&chats);
                }
                // Archived list (and its count) refreshed alongside the active list.
                WaEvent::ArchivedChatsSnapshot(chats) => {
                    win_ev.update_archived(&chats);
                }
                // History/live messages: cache the page, apply if still open.
                WaEvent::ChatHistory { jid, messages } => {
                    hist_cache_ev
                        .borrow_mut()
                        .put(jid.clone(), messages.clone());
                    if current_open_ev.borrow().as_deref() == Some(jid.as_str()) {
                        win_ev.show_history(&messages);
                    }
                }
                WaEvent::NewMessage(row) => {
                    hist_cache_ev.borrow_mut().push_row(&row);
                    if current_open_ev.borrow().as_deref() == Some(row.chat_jid.as_str()) {
                        win_ev.append_message(&row);
                    }
                }
                // A message was edited/revoked (by us or a peer): rebuild its
                // bubble in place if the chat is on screen.
                WaEvent::MessageUpdated { row } => {
                    hist_cache_ev.borrow_mut().replace(&row);
                    if current_open_ev.borrow().as_deref() == Some(row.chat_jid.as_str()) {
                        win_ev.thread.replace_bubble(&row.id.clone(), &row);
                    }
                }
                WaEvent::MessageDeleted { chat_jid, id } => {
                    hist_cache_ev.borrow_mut().remove(&chat_jid, &id);
                    if win_ev.viewer.is_showing(&id) {
                        win_ev.viewer.hide();
                    }
                    if current_open_ev.borrow().as_deref() == Some(chat_jid.as_str()) {
                        win_ev.thread.remove_bubble(&id);
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
                    hist_cache_ev
                        .borrow_mut()
                        .set_status(&chat_jid, &message_ids, status);
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
                // A media file finished downloading: photos/videos/GIFs open in
                // the full-window viewer; documents in the system's default app
                // (after an explicit confirmation — the file came from the network).
                WaEvent::MediaReady { chat_jid, id, kind, path, ts, from_me, siblings } => {
                    if kind == 4 {
                        let name = std::path::Path::new(&path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.clone());
                        let dialog = adw::MessageDialog::new(
                            Some(&win_ev.window),
                            Some("Aprire il documento?"),
                            Some(&name),
                        );
                        dialog.add_response("annulla", "Annulla");
                        dialog.add_response("apri", "Apri");
                        dialog
                            .set_response_appearance("apri", adw::ResponseAppearance::Suggested);
                        dialog.set_default_response(Some("apri"));
                        let parent_win = win_ev.window.clone();
                        dialog.connect_response(None, move |_, resp| {
                            if resp == "apri" {
                                crate::util::open_file(&path, Some(parent_win.upcast_ref()));
                            }
                        });
                        dialog.present();
                        continue;
                    }
                    let chat_name = chats_now_ev
                        .borrow()
                        .iter()
                        .find(|c| c.jid == chat_jid)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| win_ev.chat_title());
                    win_ev
                        .viewer
                        .show(&chat_name, chat_jid, id, kind, path, ts, from_me, siblings);
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
                // Profile/group info arrived: show it in the right side pane.
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
                    let w = crate::ui::profile::build(
                        &win_ev.avatars,
                        data,
                        on_open,
                        on_block,
                        on_media,
                    );
                    win_ev.side_pane.show_profile(&w, "Profilo");
                    win_ev.set_side_pane_visible(true);
                }
                // The chat's media gallery: show it on the pane's media page.
                WaEvent::ChatMedia {
                    jid,
                    photos,
                    videos,
                    documents,
                    links,
                } => {
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
                    let on_link: Rc<dyn Fn(String)> =
                        Rc::new(move |url: String| crate::util::open_uri(&url));
                    let w = crate::ui::media_grid::build(
                        photos,
                        videos,
                        documents,
                        links,
                        on_open,
                        on_link,
                        on_need,
                        media_grid_tiles.clone(),
                    );
                    win_ev.side_pane.show_media(&w);
                    win_ev.set_side_pane_visible(true);
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
        // Capture phase so the viewer's arrow navigation beats the inner
        // ScrolledWindow (which would otherwise scroll a zoomed photo).
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| {
            // While the media viewer is open, arrows step through the chat's
            // media and Esc closes it (before any chat/selection handling).
            if win_esc.viewer.visible() {
                match key {
                    gtk::gdk::Key::Escape => {
                        win_esc.viewer.hide();
                        return glib::Propagation::Stop;
                    }
                    gtk::gdk::Key::Left => {
                        win_esc.viewer.navigate(-1);
                        return glib::Propagation::Stop;
                    }
                    gtk::gdk::Key::Right => {
                        win_esc.viewer.navigate(1);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            if key == gtk::gdk::Key::Escape {
                if win_esc.chat_list.is_selecting() {
                    win_esc.chat_list.cancel_selection();
                    return glib::Propagation::Stop;
                }
                if win_esc.side_pane_visible() {
                    win_esc.set_side_pane_visible(false);
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

/// How many chats keep their history page cached, and how many rows per chat
/// (a fresh open loads 80; live appends grow it a bit before trimming).
const HIST_CAP: usize = 5;
const HIST_MAX_ROWS: usize = 300;

/// LRU cache of recently opened chats' history pages — rows only, never
/// widgets/textures (those would pin decoded images in RAM, undoing the
/// scaled-texture work). Maintained from the event stream, so an entry always
/// matches the DB state for the recent page. A chat re-keyed by the LID↔PN
/// merge simply misses (and ages out of the LRU).
struct HistCache {
    map: HashMap<String, Vec<crate::model::MessageRow>>,
    lru: VecDeque<String>,
}

impl HistCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    fn get(&self, jid: &str) -> Option<&Vec<crate::model::MessageRow>> {
        self.map.get(jid)
    }

    fn touch(&mut self, jid: &str) {
        self.lru.retain(|j| j != jid);
        self.lru.push_back(jid.to_string());
    }

    fn put(&mut self, jid: String, rows: Vec<crate::model::MessageRow>) {
        self.map.insert(jid.clone(), rows);
        self.touch(&jid);
        while self.lru.len() > HIST_CAP {
            if let Some(old) = self.lru.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    fn push_row(&mut self, row: &crate::model::MessageRow) {
        if let Some(v) = self.map.get_mut(&row.chat_jid) {
            v.push(row.clone());
            if v.len() > HIST_MAX_ROWS {
                let cut = v.len() - HIST_MAX_ROWS;
                v.drain(..cut);
            }
        }
    }

    /// Advances (never regresses) the status of the given messages.
    fn set_status(&mut self, chat_jid: &str, ids: &[String], status: i32) {
        if let Some(v) = self.map.get_mut(chat_jid) {
            for m in v.iter_mut() {
                if m.status < status && ids.contains(&m.id) {
                    m.status = status;
                }
            }
        }
    }

    fn replace(&mut self, row: &crate::model::MessageRow) {
        if let Some(v) = self.map.get_mut(&row.chat_jid) {
            if let Some(m) = v.iter_mut().find(|m| m.id == row.id) {
                *m = row.clone();
            }
        }
    }

    fn remove(&mut self, chat_jid: &str, id: &str) {
        if let Some(v) = self.map.get_mut(chat_jid) {
            v.retain(|m| m.id != id);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
    }
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

/// "Eliminare il messaggio?" dialog, shared by the message context menu and the
/// media viewer: "per me" always, "per tutti" (revoke) only for own messages.
fn present_delete_dialog(
    parent: &adw::ApplicationWindow,
    command_tx: &async_channel::Sender<WaCommand>,
    jid: String,
    id: String,
    can_revoke: bool,
) {
    let dialog = adw::MessageDialog::new(Some(parent), Some("Eliminare il messaggio?"), None);
    dialog.add_response("annulla", "Annulla");
    dialog.add_response("me", "Elimina per me");
    dialog.set_response_appearance("me", adw::ResponseAppearance::Destructive);
    if can_revoke {
        dialog.add_response("tutti", "Elimina per tutti");
        dialog.set_response_appearance("tutti", adw::ResponseAppearance::Destructive);
    }
    dialog.set_default_response(Some("annulla"));
    let command_tx = command_tx.clone();
    dialog.connect_response(None, move |_, resp| match resp {
        "me" => {
            let _ = command_tx.try_send(WaCommand::DeleteLocal {
                jid: jid.clone(),
                id: id.clone(),
            });
        }
        "tutti" => {
            let _ = command_tx.try_send(WaCommand::RevokeMessage {
                jid: jid.clone(),
                id: id.clone(),
            });
        }
        _ => {}
    });
    dialog.present();
}

/// Modal "Inoltra a…" chat picker: a searchable list of the active chats;
/// activating a row invokes `on_pick(jid)` and closes the picker.
fn present_forward_picker(
    win: &MainWindow,
    chats: &[crate::model::ChatSummary],
    on_pick: impl Fn(String) + 'static,
) {
    let dialog = adw::Window::builder()
        .transient_for(&win.window)
        .modal(true)
        .default_width(360)
        .default_height(480)
        .title("Inoltra a…")
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&adw::HeaderBar::new());

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Cerca chat")
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    root.append(&search);

    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::None);

    // Rows and the parallel jid/name vectors (row.index() is stable under
    // filtering, so index → jid stays correct while searching).
    let mut jids: Vec<String> = Vec::with_capacity(chats.len());
    let mut names: Vec<String> = Vec::with_capacity(chats.len());
    for c in chats {
        let h = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(6)
            .margin_end(6)
            .build();
        let av = adw::Avatar::new(32, Some(&c.name), true);
        if let Some(tex) = win.avatars.borrow().get(&c.jid) {
            av.set_custom_image(Some(tex));
        }
        let lbl = gtk::Label::builder()
            .label(&c.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        h.append(&av);
        h.append(&lbl);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&h));
        list.append(&row);
        jids.push(c.jid.clone());
        names.push(c.name.to_lowercase());
    }

    {
        let search = search.clone();
        list.set_filter_func(move |row| {
            let q = search.text().to_lowercase();
            q.is_empty()
                || names
                    .get(row.index() as usize)
                    .is_some_and(|n| n.contains(&q))
        });
    }
    {
        let list = list.clone();
        search.connect_search_changed(move |_| list.invalidate_filter());
    }
    {
        let dialog = dialog.clone();
        list.connect_row_activated(move |_, row| {
            if let Some(jid) = jids.get(row.index() as usize) {
                on_pick(jid.clone());
            }
            dialog.close();
        });
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    root.append(&scrolled);

    // Esc closes the picker.
    {
        let dialog_esc = dialog.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dialog_esc.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dialog.add_controller(keys);
    }

    dialog.set_content(Some(&root));
    dialog.present();
    search.grab_focus();
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
         .link-card { background-color: alpha(currentColor, 0.07); \
            border-radius: 8px; padding: 6px; margin-bottom: 2px; } \
         .viewer-dim { background-color: rgba(0, 0, 0, 0.88); } \
         .viewer-text { color: #ffffff; } \
         .viewer-osd { color: #ffffff; background-color: alpha(#000000, 0.45); \
            background-image: none; border-radius: 999px; } \
         .viewer-osd:hover { background-color: alpha(#ffffff, 0.25); } \
         .attach-item { border-radius: 12px; background-color: alpha(currentColor, 0.08); } \
         .attach-item picture { border-radius: 12px; } \
         .attach-x { opacity: 0.65; min-height: 0; min-width: 0; padding: 1px; margin: 3px; \
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
