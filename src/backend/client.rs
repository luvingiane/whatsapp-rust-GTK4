//! Construction and lifecycle of the whatsapp-rust [`Bot`], plus translation of
//! its [`Event`]s into our [`WaEvent`]s and into the application [`Store`].
//! Everything here runs on the Tokio runtime thread spawned by [`super::runtime`].

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_channel::{Receiver, Sender};
use log::{error, info, warn};
use tokio::sync::Notify;
use wacore::download::MediaType;
use wacore::store::DevicePropsOverride;
use wacore::types::events::Event;
use wacore::types::presence::ReceiptType;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
use whatsapp_rust::media::{audio_message, AudioOptions};
use whatsapp_rust::waproto::codec;
use whatsapp_rust::UploadOptions;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::waproto::whatsapp::device_props::PlatformType;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use super::bridge::{WaCommand, WaEvent};
use crate::config;
use crate::model::MessageRow;
use crate::store::{ChatUpsert, Store};
use crate::util::preview;

/// Coalescing window for chat-list snapshots: many history-sync events arrive in
/// a burst, so we wait a moment after a change before querying + pushing.
const SNAPSHOT_DEBOUNCE: Duration = Duration::from_millis(400);

/// Builds the bot against the session DB, wires events to the app [`Store`], and
/// drives the run loop until it ends or a [`WaCommand::Shutdown`] arrives.
///
/// `db_path` is the whatsapp-rust session DB; `app_db_path` is our chat store.
/// Routine protocol hiccups (decrypt failures, disconnects) are logged, never panic.
pub async fn run(
    db_path: String,
    app_db_path: String,
    event_tx: Sender<WaEvent>,
    command_rx: Receiver<WaCommand>,
) -> Result<()> {
    info!("opening session database at {db_path}");
    // SqliteStore enables WAL journaling and runs migrations on open, so session
    // and keys are persisted atomically across restarts.
    // HEAD: with_backend takes `impl Backend` (SqliteStore impls it directly) —
    // no Arc wrapper anymore.
    let backend = SqliteStore::new(&db_path).await?;

    info!("opening app database at {app_db_path}");
    let store = Store::open(app_db_path).await?;
    let dirty = Arc::new(Notify::new());

    // Snapshot task: pushes an initial chat list immediately (instant UI on
    // reconnect), then a debounced refresh whenever the store changes.
    {
        let store = store.clone();
        let tx = event_tx.clone();
        let dirty = dirty.clone();
        tokio::spawn(async move {
            // Push both the active and the archived lists so the sidebar and the
            // archived view (with its count) always reflect the same store state.
            async fn push_snapshots(store: &Store, tx: &Sender<WaEvent>) {
                match store.list_chats().await {
                    Ok(chats) => {
                        let _ = tx.send(WaEvent::ChatsSnapshot(chats)).await;
                    }
                    Err(e) => warn!("list_chats failed: {e:?}"),
                }
                match store.list_archived_chats().await {
                    Ok(chats) => {
                        let _ = tx.send(WaEvent::ArchivedChatsSnapshot(chats)).await;
                    }
                    Err(e) => warn!("list_archived_chats failed: {e:?}"),
                }
            }
            push_snapshots(&store, &tx).await;
            loop {
                dirty.notified().await;
                tokio::time::sleep(SNAPSHOT_DEBOUNCE).await;
                push_snapshots(&store, &tx).await;
            }
        });
    }

    let ev_tx = event_tx.clone();
    let ev_store = store.clone();
    let ev_dirty = dirty.clone();
    let bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        // Present as "Google Chrome (Linux)" in the phone's Linked Devices list
        // instead of an "unknown device". Sent only at pairing; cosmetic only.
        .with_device_props(
            DevicePropsOverride::new()
                .with_os(config::DEVICE_OS)
                .with_platform_type(PlatformType::Chrome),
        )
        .on_event(move |event, client| {
            let tx = ev_tx.clone();
            let store = ev_store.clone();
            let dirty = ev_dirty.clone();
            async move {
                handle_event(event, client, tx, store, dirty).await;
            }
        })
        .build()
        .await?;

    // `spawn` starts the connection/handshake on the runtime and returns a handle
    // that resolves when the run loop exits. whatsapp-rust handles reconnection.
    let mut handle = bot.spawn();
    info!("WhatsApp backend run loop started");

    // Periodic LID↔PN reconcile. App-state events (archive/pin/saved-name) arrive
    // with `@lid` keys while 1:1 chats are keyed by phone number, so the flags land
    // on a key that doesn't match the chat row. We (a) re-key each chat_meta entry
    // under every JID form the library knows, and (b) re-key `@lid` metadata onto
    // the PN form via the `lid_map` we learn from ContactUpdate/messages. The
    // LID↔PN knowledge fills over time (usync, incoming messages), so we re-run
    // for the first few minutes after connect rather than once.
    {
        let client = handle.client();
        let store = store.clone();
        let dirty = dirty.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            // Explicitly resolve @lid → PN for archived chats via a batched usync:
            // `get_user_info` persists the LID↔PN pairs into the library map, which
            // `jid_forms` then uses to re-key the archive flag onto the PN chat row.
            // Passive learning (ContactUpdate/messages) never covers archived
            // contacts we don't open, so without this they leak into the main list.
            let lids: Vec<whatsapp_rust::Jid> = match store.all_chat_meta().await {
                Ok(rows) => rows
                    .into_iter()
                    .filter(|(j, archived, ..)| *archived && j.ends_with("@lid"))
                    .filter_map(|(j, ..)| j.parse().ok())
                    .collect(),
                Err(_) => Vec::new(),
            };
            if !lids.is_empty() {
                info!("usync: resolving {} archived @lid JIDs", lids.len());
                for chunk in lids.chunks(50) {
                    if let Err(e) = client.contacts().get_user_info(chunk).await {
                        warn!("get_user_info (lid resolve) failed: {e:?}");
                    }
                }
            }
            // ~10 minutes of catch-up (20 × 30s); live events keep it fresh after.
            for _ in 0..20 {
                let mut unified = 0u32;
                if let Ok(rows) = store.all_chat_meta().await {
                    for (jid_str, archived, _pinned, _muted, saved) in rows {
                        let Ok(jid) = jid_str.parse::<whatsapp_rust::Jid>() else {
                            continue;
                        };
                        let forms = jid_forms(&client, &jid).await;
                        if forms.len() < 2 {
                            continue;
                        }
                        for key in forms {
                            if archived {
                                let _ = store.set_archived(key.clone(), true).await;
                            }
                            if !saved.is_empty() {
                                let _ = store.set_saved_name(key, saved.clone()).await;
                            }
                        }
                        unified += 1;
                    }
                }
                let propagated = store.propagate_lid_meta().await.unwrap_or(0);
                // Collapse @lid/PN duplicate chats now that more pairs are known.
                let merged = store.merge_lid_duplicates().await.unwrap_or(0);
                if unified > 0 || propagated > 0 || merged > 0 {
                    info!(
                        "LID<->PN reconcile: {unified} via library map, {propagated} via lid_map, {merged} merged"
                    );
                    dirty.notify_one();
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }

    // Client handle + in-flight set for avatar downloads (deduped, off the
    // command loop so a slow network never blocks OpenChat/LoadOlder).
    let cmd_client = handle.client();
    let avatar_inflight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Serve UI commands until the window closes (Shutdown) or the run loop ends.
    loop {
        tokio::select! {
            _ = &mut handle => {
                warn!("bot run loop ended");
                break;
            }
            cmd = command_rx.recv() => match cmd {
                Ok(WaCommand::OpenChat(jid)) => {
                    // Opening a chat reads it: clear its unread badge and refresh
                    // the list. `jid` is already the canonical chat key.
                    if let Err(e) = store.clear_unread(jid.clone()).await {
                        warn!("clear_unread (open) failed: {e:?}");
                    }
                    dirty.notify_one();
                    match store.load_messages(jid.clone(), 200).await {
                        Ok(messages) => {
                            let _ = event_tx.send(WaEvent::ChatHistory { jid, messages }).await;
                        }
                        Err(e) => warn!("load_messages failed: {e:?}"),
                    }
                }
                Ok(WaCommand::LoadOlder { jid, before_ts, before_id, count }) => {
                    // Local-only backfill: page back through what history sync
                    // already stored in app.db (no network request).
                    match store
                        .load_messages_before(jid.clone(), before_ts, before_id, count)
                        .await
                    {
                        Ok(messages) => {
                            let _ = event_tx.send(WaEvent::OlderHistory { jid, messages }).await;
                        }
                        Err(e) => warn!("load_messages_before failed: {e:?}"),
                    }
                }
                Ok(WaCommand::FetchAvatar(jid)) => {
                    spawn_fetch_avatar(jid, &cmd_client, &event_tx, &avatar_inflight);
                }
                Ok(WaCommand::SetPresence { available }) => {
                    let presence = cmd_client.presence();
                    let res = if available {
                        presence.set_available().await
                    } else {
                        presence.set_unavailable().await
                    };
                    match res {
                        Ok(()) => info!("presence -> {}", if available { "available" } else { "unavailable" }),
                        // Push name not yet known (early post-login): the UI re-sends
                        // presence on focus and on Connected, so this self-heals.
                        Err(e) => warn!("set presence (available={available}) failed: {e:?}"),
                    }
                }
                Ok(WaCommand::SendText { jid, text }) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        match jid.parse::<whatsapp_rust::Jid>() {
                            Ok(to) => match cmd_client.send_text(to, text.clone()).await {
                                Ok(res) => {
                                    store_outgoing(&store, &event_tx, &dirty, jid, text, res.message_id, None, 0, Vec::new()).await;
                                }
                                Err(e) => {
                                    warn!("send_text failed: {e:?}");
                                    let _ = event_tx.send(WaEvent::Error(format!("Invio fallito: {e}"))).await;
                                }
                            },
                            Err(e) => warn!("send_text: bad jid {jid}: {e:?}"),
                        }
                    }
                }
                Ok(WaCommand::SendAudio { jid, ogg, duration, waveform }) => {
                    match jid.parse::<whatsapp_rust::Jid>() {
                        Ok(to) => match cmd_client.upload(ogg, MediaType::Audio, UploadOptions::default()).await {
                            Ok(up) => {
                                let wf = if waveform.is_empty() { None } else { Some(waveform.clone()) };
                                let msg = audio_message(up, AudioOptions {
                                    ptt: Some(true),
                                    duration_seconds: Some(duration),
                                    mimetype: None,
                                    waveform: wf,
                                });
                                // Keep the media proto so our own note is replayable.
                                let media = codec::message_to_vec(&msg);
                                match cmd_client.send_message(to, msg).await {
                                    Ok(res) => {
                                        store_outgoing(&store, &event_tx, &dirty, jid, "🎤 Messaggio vocale".to_string(), res.message_id, Some(media), duration, waveform).await;
                                    }
                                    Err(e) => {
                                        warn!("send audio failed: {e:?}");
                                        let _ = event_tx.send(WaEvent::Error(format!("Invio vocale fallito: {e}"))).await;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("audio upload failed: {e:?}");
                                let _ = event_tx.send(WaEvent::Error(format!("Upload vocale fallito: {e}"))).await;
                            }
                        },
                        Err(e) => warn!("send_audio: bad jid {jid}: {e:?}"),
                    }
                }
                Ok(WaCommand::PlayAudio { chat_jid, id }) => {
                    let path = match config::audio_cache_path(&id) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("audio_cache_path failed: {e:?}");
                            continue;
                        }
                    };
                    // Cache hit: reply at once.
                    if path.exists() {
                        let _ = event_tx
                            .send(WaEvent::AudioReady { id: id.clone(), path: path.to_string_lossy().into_owned() })
                            .await;
                        continue;
                    }
                    // Decode the stored proto → AudioMessage → download + decrypt.
                    match store.get_media(chat_jid.clone(), id.clone()).await {
                        Ok(Some(bytes)) => {
                            match codec::message_decode(&bytes) {
                                Ok(msg) => match msg.audio_message {
                                    Some(audio) => match cmd_client.download(&*audio).await {
                                        Ok(ogg) => {
                                            if let Err(e) = std::fs::write(&path, &ogg) {
                                                warn!("write voice note failed: {e:?}");
                                            } else {
                                                let _ = event_tx
                                                    .send(WaEvent::AudioReady { id: id.clone(), path: path.to_string_lossy().into_owned() })
                                                    .await;
                                            }
                                        }
                                        Err(e) => {
                                            warn!("audio download failed: {e:?}");
                                            let _ = event_tx.send(WaEvent::Error(format!("Download vocale fallito: {e}"))).await;
                                        }
                                    },
                                    None => warn!("stored media has no audio_message: {id}"),
                                },
                                Err(e) => warn!("decode media proto failed: {e:?}"),
                            }
                        }
                        Ok(None) => warn!("no media stored for {id}"),
                        Err(e) => warn!("get_media failed: {e:?}"),
                    }
                }
                Ok(WaCommand::Shutdown) => {
                    info!("shutdown requested by UI");
                    handle.shutdown().await;
                    break;
                }
                Err(_) => {
                    info!("command channel closed; stopping backend");
                    handle.abort();
                    break;
                }
            },
        }
    }

    Ok(())
}

/// Resolves a profile picture for `jid` and notifies the UI when it lands on
/// disk. A cache hit replies immediately; otherwise the download runs in a
/// spawned task (deduped via `inflight`) so the command loop stays responsive.
fn spawn_fetch_avatar(
    jid: String,
    client: &Arc<Client>,
    tx: &Sender<WaEvent>,
    inflight: &Arc<Mutex<HashSet<String>>>,
) {
    let client = client.clone();
    let tx = tx.clone();
    let inflight = inflight.clone();
    tokio::spawn(async move {
        let path = match config::avatar_cache_path(&jid) {
            Ok(p) => p,
            Err(e) => {
                warn!("avatar_cache_path failed for {jid}: {e:?}");
                return;
            }
        };
        // Cache hit: nothing to download.
        if path.exists() {
            let _ = tx
                .send(WaEvent::Avatar {
                    jid,
                    path: path.to_string_lossy().into_owned(),
                })
                .await;
            return;
        }
        // Dedup concurrent downloads for the same JID.
        if !inflight.lock().unwrap().insert(jid.clone()) {
            return;
        }
        let result = download_avatar(&client, &jid, &path).await;
        inflight.lock().unwrap().remove(&jid);
        match result {
            Ok(true) => {
                let _ = tx
                    .send(WaEvent::Avatar {
                        jid,
                        path: path.to_string_lossy().into_owned(),
                    })
                    .await;
            }
            Ok(false) => {} // no picture set for this contact
            Err(e) => warn!("avatar download failed for {jid}: {e:?}"),
        }
    });
}

/// Fetches the preview profile picture URL for `jid` and writes the bytes to
/// `path`. Returns `Ok(false)` if the contact has no picture.
async fn download_avatar(client: &Arc<Client>, jid: &str, path: &Path) -> Result<bool> {
    let parsed: whatsapp_rust::Jid = jid.parse()?;
    let contacts = client.contacts();
    let Some(pic) = contacts.get_profile_picture(&parsed, true).await? else {
        return Ok(false);
    };
    if pic.url.is_empty() {
        return Ok(false);
    }
    let url = pic.url.clone();
    let dest = path.to_path_buf();
    // ureq is blocking and std::fs::write is sync, so do both off the runtime.
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut body = ureq::get(&url).call()?.into_body();
        let bytes = body.read_to_vec()?;
        std::fs::write(&dest, &bytes)?;
        Ok(())
    })
    .await??;
    Ok(true)
}

/// Inserts a message we just sent into the store (optimistic, status = Sent) and
/// notifies the UI so the bubble appears immediately. The later self-fanout echo
/// is suppressed by `insert_message` returning `false` for the duplicate id.
async fn store_outgoing(
    store: &Store,
    tx: &Sender<WaEvent>,
    dirty: &Notify,
    chat: String,
    body: String,
    id: String,
    media: Option<Vec<u8>>,
    audio_secs: u32,
    audio_waveform: Vec<u8>,
) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let row = MessageRow {
        id: id.clone(),
        chat_jid: chat.clone(),
        sender_jid: String::new(),
        sender_name: String::new(),
        from_me: true,
        ts,
        body: body.clone(),
        status: 1,
        audio: media.is_some(),
        audio_secs,
        audio_waveform: audio_waveform.clone(),
    };
    if let Err(e) = store.insert_message(row.clone()).await {
        warn!("store_outgoing insert failed: {e:?}");
    }
    if let Some(bytes) = media {
        if let Err(e) = store.set_media(chat.clone(), id.clone(), bytes).await {
            warn!("store_outgoing set_media failed: {e:?}");
        }
        if let Err(e) = store
            .set_audio_meta(chat.clone(), id, audio_secs, audio_waveform)
            .await
        {
            warn!("store_outgoing set_audio_meta failed: {e:?}");
        }
    }
    if let Err(e) = store.apply_message(chat, body, ts, true, 1).await {
        warn!("store_outgoing apply failed: {e:?}");
    }
    let _ = tx.send(WaEvent::NewMessage(row)).await;
    dirty.notify_one();
}

/// Translates a single whatsapp-rust [`Event`] into UI updates and store writes.
/// Unhandled variants are intentionally ignored for this step.
async fn handle_event(
    event: Arc<Event>,
    client: Arc<Client>,
    tx: Sender<WaEvent>,
    store: Store,
    dirty: Arc<Notify>,
) {
    match &*event {
        Event::PairingQrCode { code, timeout } => {
            info!("QR code received (valid ~{}s)", timeout.as_secs());
            let _ = tx.send(WaEvent::QrCode(code.clone())).await;
        }
        Event::PairSuccess(p) => {
            info!("pairing success as {}", p.id);
            let _ = tx
                .send(WaEvent::PairSuccess {
                    jid: preview::pretty_number(&p.id.to_string()),
                })
                .await;
        }
        Event::PairError(e) => {
            error!("pairing error: {}", e.error);
            let _ = tx
                .send(WaEvent::Error(format!("Errore di pairing: {}", e.error)))
                .await;
        }
        Event::Connected(_) => {
            // `Connected` carries no JID; read our own number from the store.
            let jid = client.get_pn().map(|j| j.to_string());
            info!("connected (jid={jid:?})");
            let _ = tx
                .send(WaEvent::Connected {
                    jid: jid.as_deref().map(preview::pretty_number),
                })
                .await;
        }
        Event::Disconnected(_) => {
            warn!("disconnected; whatsapp-rust will retry");
            let _ = tx.send(WaEvent::Disconnected).await;
        }
        Event::LoggedOut(l) => {
            warn!("logged out (reason={:?})", l.reason);
            let _ = tx.send(WaEvent::LoggedOut).await;
        }
        Event::ClientOutdated(_) => {
            error!("client outdated: the pinned whatsapp-rust version may need updating");
            let _ = tx
                .send(WaEvent::Error(
                    "Client non aggiornato: whatsapp-rust va aggiornato".into(),
                ))
                .await;
        }
        Event::TemporaryBan(b) => {
            error!("temporary ban: {:?}", b.code);
            let _ = tx
                .send(WaEvent::Error(format!(
                    "Ban temporaneo dell'account: {:?}",
                    b.code
                )))
                .await;
        }
        Event::ConnectFailure(f) => {
            error!("connect failure: {} ({:?})", f.message, f.reason);
            let _ = tx
                .send(WaEvent::Error(format!(
                    "Connessione fallita: {}",
                    f.message
                )))
                .await;
        }
        Event::UndecryptableMessage(_) => {
            // Bad MAC / No Session: log and keep going, never crash.
            warn!("undecryptable message received (ignored)");
        }
        Event::HistorySync(lazy) => {
            if let Some(hs) = lazy.get() {
                // Contact pushnames (profile names) → resolve numbers to names.
                let names: Vec<(String, String)> = hs
                    .pushnames
                    .iter()
                    .filter_map(|p| Some((p.id.clone()?, p.pushname.clone()?)))
                    .filter(|(id, name)| !id.is_empty() && !name.is_empty())
                    .collect();
                if !names.is_empty() {
                    info!("history sync: {} pushnames", names.len());
                    if let Err(e) = store.upsert_contacts(names).await {
                        warn!("upsert_contacts failed: {e:?}");
                    }
                }

                // Canonicalize each conversation's JID (resolve @lid→PN) so chats
                // are keyed consistently and @lid chats aren't dropped/duplicated.
                let mut rows: Vec<ChatUpsert> = Vec::new();
                let mut msgs: Vec<(MessageRow, Option<Vec<u8>>)> = Vec::new();
                for c in &hs.conversations {
                    if !is_user_chat(&c.id) {
                        continue;
                    }
                    let canon = canonical_chat_jid_str(&client, &c.id).await;
                    if let Some(r) = conv_to_upsert(c, &canon) {
                        rows.push(r);
                    }
                    msgs.extend(conv_to_messages(c, &canon));
                }
                if !rows.is_empty() {
                    info!("history sync: upserting {} chats", rows.len());
                    if let Err(e) = store.upsert_chats(rows).await {
                        warn!("upsert_chats failed: {e:?}");
                    }
                }
                if !msgs.is_empty() {
                    let from_me = msgs.iter().filter(|(m, _)| m.from_me).count();
                    let with_status = msgs.iter().filter(|(m, _)| m.status >= 2).count();
                    info!(
                        "history sync: storing {} messages ({from_me} ours, {with_status} already ✓✓/read)",
                        msgs.len()
                    );
                    // Media + waveform to persist after the rows exist (voice notes).
                    #[allow(clippy::type_complexity)]
                    let media: Vec<(String, String, Vec<u8>, u32, Vec<u8>)> = msgs
                        .iter()
                        .filter_map(|(m, media)| {
                            media.as_ref().map(|b| {
                                (m.chat_jid.clone(), m.id.clone(), b.clone(), m.audio_secs, m.audio_waveform.clone())
                            })
                        })
                        .collect();
                    let rows: Vec<MessageRow> = msgs.into_iter().map(|(m, _)| m).collect();
                    if let Err(e) = store.insert_messages(rows).await {
                        warn!("insert_messages failed: {e:?}");
                    }
                    for (chat_jid, id, bytes, secs, waveform) in media {
                        if let Err(e) = store.set_media(chat_jid.clone(), id.clone(), bytes).await {
                            warn!("set_media (history) failed: {e:?}");
                        }
                        if let Err(e) = store.set_audio_meta(chat_jid, id, secs, waveform).await {
                            warn!("set_audio_meta (history) failed: {e:?}");
                        }
                    }
                }
                dirty.notify_one();
            }
        }
        Event::Message(msg, info) => {
            // Canonicalize @lid → PN so the message lands on the same chat key as
            // the list/open thread (fixes live updates for LID-addressed chats).
            let chat = canonical_chat_jid(&client, &info.source.chat).await;
            if is_user_chat(&chat) {
                // Learn LID↔PN from the message's primary/alt JIDs (peers are
                // addressed by one form with the other in *_alt).
                learn_alt(&store, &info.source.sender, info.source.sender_alt.as_ref()).await;
                if let Some(rcpt) = &info.source.recipient {
                    learn_alt(&store, rcpt, info.source.recipient_alt.as_ref()).await;
                }
                // Learn the sender's pushname from incoming messages.
                if !info.source.is_from_me && !info.push_name.is_empty() {
                    let sender = info.source.sender.to_string();
                    if let Err(e) = store
                        .upsert_contacts(vec![(sender, info.push_name.clone())])
                        .await
                    {
                        warn!("upsert_contacts (live) failed: {e:?}");
                    }
                }

                let body = preview::message_preview(msg);
                // Skip system/protocol messages (empty preview): no bubble, no
                // chat-list preview change.
                if !body.is_empty() {
                    let ts = info.timestamp.timestamp();
                    let from_me = info.source.is_from_me;
                    // Our own live message is at least server-acked → one tick;
                    // incoming messages carry no status.
                    let status = if from_me { 1 } else { 0 };
                    // Keep the media proto + waveform for playable voice notes.
                    let audio_media = msg
                        .audio_message
                        .as_ref()
                        .map(|_| codec::message_to_vec(msg));
                    let (audio_secs, audio_waveform) = audio_meta(msg);
                    let row = MessageRow {
                        id: info.id.clone(),
                        chat_jid: chat.clone(),
                        sender_jid: info.source.sender.to_string(),
                        // Sender's profile name straight from the event (group labels);
                        // empty for our own messages.
                        sender_name: if from_me {
                            String::new()
                        } else {
                            info.push_name.clone()
                        },
                        from_me,
                        ts,
                        body: body.clone(),
                        status,
                        audio: audio_media.is_some(),
                        audio_secs,
                        audio_waveform: audio_waveform.clone(),
                    };
                    // Insert first: a `false` means this is the self-fanout echo of
                    // a message we already inserted on send — skip the preview bump
                    // and the UI append so we don't duplicate the bubble.
                    let inserted = match store.insert_message(row.clone()).await {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("insert_message failed: {e:?}");
                            false
                        }
                    };
                    if inserted {
                        info!("message: from_me={from_me} status={status} id={} chat={chat}", info.id);
                        if let Some(bytes) = audio_media {
                            if let Err(e) = store.set_media(chat.clone(), info.id.clone(), bytes).await {
                                warn!("set_media (live) failed: {e:?}");
                            }
                            if let Err(e) = store
                                .set_audio_meta(chat.clone(), info.id.clone(), audio_secs, audio_waveform.clone())
                                .await
                            {
                                warn!("set_audio_meta (live) failed: {e:?}");
                            }
                        }
                        if let Err(e) = store
                            .apply_message(chat, body, ts, from_me, status)
                            .await
                        {
                            warn!("apply_message failed: {e:?}");
                        }
                        let _ = tx.send(WaEvent::NewMessage(row)).await;
                        dirty.notify_one();
                    }
                }
            }
        }
        // App-state full-sync (and live) updates: archive/pin/mute + the saved
        // address-book name. JIDs may be LID-form, so normalize to the chat key.
        Event::ArchiveUpdate(a) => {
            let archived = a.action.archived.unwrap_or(false);
            info!("archive: jid={} archived={archived}", a.jid);
            for jid in jid_forms(&client, &a.jid).await {
                if let Err(e) = store.set_archived(jid, archived).await {
                    warn!("set_archived failed: {e:?}");
                }
            }
            // Re-key onto the PN chat row immediately if we already learned the
            // @lid↔PN pair (the library map is often empty right after pairing).
            let _ = store.propagate_lid_meta().await;
            dirty.notify_one();
        }
        Event::PinUpdate(p) => {
            let pinned = p.action.pinned.unwrap_or(false);
            for jid in jid_forms(&client, &p.jid).await {
                if let Err(e) = store.set_pinned(jid, pinned).await {
                    warn!("set_pinned failed: {e:?}");
                }
            }
            dirty.notify_one();
        }
        Event::MuteUpdate(m) => {
            let muted = m.action.muted.unwrap_or(false);
            let until = m.action.mute_end_timestamp.unwrap_or(0);
            for jid in jid_forms(&client, &m.jid).await {
                if let Err(e) = store.set_muted(jid, muted, until).await {
                    warn!("set_muted failed: {e:?}");
                }
            }
            dirty.notify_one();
        }
        Event::ContactUpdate(c) => {
            // ContactAction carries both JID forms explicitly — the most reliable
            // LID↔PN pair we get. Learn it so app-state archive flags (keyed @lid)
            // can be re-keyed onto the PN chat row.
            if let (Some(pn), Some(lid)) = (&c.action.pn_jid, &c.action.lid_jid) {
                if !pn.is_empty() && !lid.is_empty() {
                    if let Err(e) = store.learn_lid_pn(lid.clone(), pn.clone()).await {
                        warn!("learn_lid_pn failed: {e:?}");
                    }
                }
            }
            let name = c
                .action
                .full_name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| c.action.first_name.clone())
                .unwrap_or_default();
            if !name.is_empty() {
                // Store the saved name under EVERY known key for this contact —
                // phone-number JID, LID JID, and the raw event JID — so it resolves
                // whether a chat (1:1, keyed PN) or a group sender (keyed LID) looks
                // it up. ContactAction carries both pn_jid and lid_jid.
                let keys = [
                    c.action.pn_jid.clone(),
                    c.action.lid_jid.clone(),
                    Some(c.jid.to_string()),
                ];
                for key in keys.into_iter().flatten() {
                    if key.is_empty() {
                        continue;
                    }
                    if let Err(e) = store.set_saved_name(key, name.clone()).await {
                        warn!("set_saved_name failed: {e:?}");
                    }
                }
                dirty.notify_one();
            }
        }
        // A chat was marked read/unread (here or on the phone), via app-state. We
        // never decremented `unread` before, so already-read chats kept showing a
        // badge. `read=Some(true)` clears it; `Some(false)` marks unread. Keyed
        // under every JID form so it matches the chat row (PN) like archive does.
        Event::MarkChatAsReadUpdate(u) => {
            info!("mark-read: jid={} read={:?}", u.jid, u.action.read);
            match u.action.read {
                Some(true) => {
                    for jid in jid_forms(&client, &u.jid).await {
                        if let Err(e) = store.clear_unread(jid).await {
                            warn!("clear_unread failed: {e:?}");
                        }
                    }
                }
                Some(false) => {
                    for jid in jid_forms(&client, &u.jid).await {
                        if let Err(e) = store.set_unread(jid, 1).await {
                            warn!("set_unread failed: {e:?}");
                        }
                    }
                }
                None => {}
            }
            dirty.notify_one();
        }
        // Delivery/read receipt for one or more of OUR sent messages. Advance
        // their stored status and tell the UI so open bubbles update live.
        Event::Receipt(r) => {
            info!(
                "receipt: type={:?} offline={} ids={} chat={}",
                r.r#type,
                r.offline,
                r.message_ids.len(),
                r.source.chat
            );
            if let Some(status) = receipt_status(&r.r#type) {
                let chat = canonical_chat_jid(&client, &r.source.chat).await;
                let mut changed = 0usize;
                for id in &r.message_ids {
                    match store
                        .update_message_status(chat.clone(), id.clone(), status)
                        .await
                    {
                        Ok(n) => changed += n,
                        Err(e) => warn!("update_message_status failed: {e:?}"),
                    }
                }
                info!(
                    "receipt applied: status={status} chat={chat} matched={changed}/{} ids",
                    r.message_ids.len()
                );
                let _ = tx
                    .send(WaEvent::ReceiptUpdate {
                        chat_jid: chat,
                        message_ids: r.message_ids.clone(),
                        status,
                    })
                    .await;
                dirty.notify_one();
            } else {
                info!("receipt ignored (type does not advance ticks)");
            }
        }
        _ => {}
    }
}

/// Maps a WhatsApp receipt type to our delivery-status scale (1 sent, 2
/// delivered, 3 read/played). Returns `None` for receipt kinds that don't
/// advance an outgoing message's ticks (retries, self-read, inactive, …).
fn receipt_status(t: &ReceiptType) -> Option<i32> {
    match t {
        ReceiptType::Sent => Some(1),
        ReceiptType::Delivered => Some(2),
        ReceiptType::Read | ReceiptType::Played => Some(3),
        _ => None,
    }
}

/// Maps a synced `WebMessageInfo` status to our scale, for our own messages
/// (incoming messages carry no ticks → 0). Unknown/pending for an outgoing
/// message still shows one tick (it was at least handed to the server).
fn wmi_local_status(wmi: &wa::WebMessageInfo, from_me: bool) -> i32 {
    if !from_me {
        return 0;
    }
    use wa::web_message_info::Status as S;
    match wmi.status() {
        S::DeliveryAck => 2,
        S::Read | S::Played => 3,
        _ => 1,
    }
}

/// All known JID forms for a peer: the raw event JID plus its phone-number and
/// LID forms (resolved via whatsapp-rust's LID↔PN map, which reads from
/// whatsapp.db so it works on reconnect without re-pairing). App-state events
/// arrive keyed by `@lid` while our chats are keyed `@s.whatsapp.net`; writing
/// metadata under every form lets both 1:1 chats (PN) and group senders (LID)
/// resolve it. Group/PN JIDs with no mapping simply return just themselves.
/// Learns a LID↔PN pair from a message source's primary and alternate JIDs, when
/// one is an `@lid` JID and the other a phone-number JID.
async fn learn_alt(store: &Store, a: &whatsapp_rust::Jid, b: Option<&whatsapp_rust::Jid>) {
    let Some(b) = b else { return };
    let (a, b) = (a.to_string(), b.to_string());
    let (lid, pn) = if a.ends_with("@lid") && b.ends_with("@s.whatsapp.net") {
        (a, b)
    } else if b.ends_with("@lid") && a.ends_with("@s.whatsapp.net") {
        (b, a)
    } else {
        return;
    };
    if let Err(e) = store.learn_lid_pn(lid, pn).await {
        warn!("learn_lid_pn (msg) failed: {e:?}");
    }
}

async fn jid_forms(client: &Client, jid: &whatsapp_rust::Jid) -> Vec<String> {
    let mut forms = vec![jid.to_string()];
    if let Ok(Some(e)) = client.get_lid_pn_entry(jid).await {
        if !e.phone_number.is_empty() {
            forms.push(format!("{}@s.whatsapp.net", e.phone_number));
        }
        if !e.lid.is_empty() {
            forms.push(format!("{}@lid", e.lid));
        }
    }
    forms.sort();
    forms.dedup();
    forms
}

/// Canonical chat key for a JID: the phone-number form when an `@lid` JID can be
/// resolved (so a chat has ONE key and isn't duplicated/dropped), otherwise the
/// JID unchanged.
async fn canonical_chat_jid(client: &Client, jid: &whatsapp_rust::Jid) -> String {
    if jid.is_lid() {
        if let Ok(Some(e)) = client.get_lid_pn_entry(jid).await {
            if !e.phone_number.is_empty() {
                return format!("{}@s.whatsapp.net", e.phone_number);
            }
        }
    }
    jid.to_string()
}

/// Like [`canonical_chat_jid`] but from a string JID (history-sync conversation
/// ids are strings). Unparseable JIDs pass through unchanged.
async fn canonical_chat_jid_str(client: &Client, jid: &str) -> String {
    match jid.parse::<whatsapp_rust::Jid>() {
        Ok(j) => canonical_chat_jid(client, &j).await,
        Err(_) => jid.to_string(),
    }
}

/// Whether a JID is a normal user/group chat we show in the list. Accepts phone
/// number (1:1), group, and LID JIDs — at HEAD many chats are LID-addressed, and
/// dropping `@lid` made chats disappear and their live messages get skipped.
/// Excludes status broadcast, newsletters, etc.
fn is_user_chat(jid: &str) -> bool {
    jid.ends_with("@s.whatsapp.net") || jid.ends_with("@g.us") || jid.ends_with("@lid")
}

/// Maps a history-sync conversation to an owned upsert row, keyed by the given
/// (already-canonicalized) `jid`. `None` if it is not a chat we display.
fn conv_to_upsert(c: &wa::Conversation, jid: &str) -> Option<ChatUpsert> {
    if !is_user_chat(jid) {
        return None;
    }
    let is_group = jid.ends_with("@g.us");
    // Store only an authoritative name (group subject / saved name); leave empty
    // otherwise so the contact pushname — or the number — can fill in at read time.
    let name = c
        .name
        .clone()
        .or_else(|| c.display_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let (last_message, msg_ts, last_from_me, last_status) = latest_message(c);
    // Order by the real last content message. Fall back to the conversation's own
    // `last_msg_timestamp`, but NOT `conversation_timestamp` — WhatsApp bumps the
    // latter for non-message activity, which made stale chats look recent.
    let last_ts = msg_ts.or(c.last_msg_timestamp).unwrap_or(0) as i64;

    Some(ChatUpsert {
        jid: jid.to_string(),
        name,
        last_message,
        last_ts,
        last_from_me,
        last_status,
        unread: c.unread_count.unwrap_or(0),
        is_group,
        archived: c.archived.unwrap_or(false),
        pinned: c.pinned.unwrap_or(0) > 0,
        muted_until: c.mute_end_time.unwrap_or(0) as i64,
    })
}

/// Extracts all displayable messages of a conversation into owned [`MessageRow`]s
/// for the message store (paired with the serialized media proto for audio/voice
/// notes), keyed by the given (canonicalized) `chat` JID. System/protocol/empty
/// messages are skipped.
fn conv_to_messages(c: &wa::Conversation, chat: &str) -> Vec<(MessageRow, Option<Vec<u8>>)> {
    if !is_user_chat(chat) {
        return Vec::new();
    }
    c.messages
        .iter()
        .filter_map(|hm| {
            let wmi = hm.message.as_ref()?;
            let msg = wmi.message.as_ref()?;
            let body = preview::message_preview(msg);
            if body.is_empty() {
                return None;
            }
            let id = wmi.key.id.clone()?;
            // Group sender is in key.participant; for 1:1 use the chat jid.
            let sender_jid = wmi
                .key
                .participant
                .clone()
                .unwrap_or_else(|| chat.to_string());
            let from_me = wmi.key.from_me.unwrap_or(false);
            // Keep the media proto for playable voice notes, plus its waveform.
            let media = msg
                .audio_message
                .as_ref()
                .map(|_| codec::message_to_vec(msg));
            let (audio_secs, audio_waveform) = audio_meta(msg);
            let row = MessageRow {
                id,
                chat_jid: chat.to_string(),
                sender_jid,
                // Resolved at read time (store::load_messages JOIN).
                sender_name: String::new(),
                from_me,
                ts: wmi.message_timestamp.unwrap_or(0) as i64,
                body,
                // Delivery status from the synced WebMessageInfo (our msgs only).
                status: wmi_local_status(wmi, from_me),
                audio: media.is_some(),
                audio_secs,
                audio_waveform,
            };
            Some((row, media))
        })
        .collect()
}

/// Voice-note duration (seconds) + amplitude waveform (0..100) from a message's
/// audio payload; `(0, empty)` for non-audio messages.
fn audio_meta(msg: &wa::Message) -> (u32, Vec<u8>) {
    match &msg.audio_message {
        Some(a) => (a.seconds.unwrap_or(0), a.waveform.clone().unwrap_or_default()),
        None => (0, Vec::new()),
    }
}

/// Finds the most recent message **with displayable content** and returns its
/// preview text, timestamp, direction and (for our own) delivery status. System/
/// protocol/empty messages are skipped: counting them made stale chats show a
/// recent date with no preview (e.g. a security-code-change notification).
fn latest_message(c: &wa::Conversation) -> (String, Option<u64>, bool, i32) {
    let mut best: Option<(u64, String, bool, i32)> = None;
    for hm in &c.messages {
        let Some(wmi) = &hm.message else { continue };
        let Some(msg) = &wmi.message else { continue };
        let preview = preview::message_preview(msg);
        if preview.is_empty() {
            continue;
        }
        let ts = wmi.message_timestamp.unwrap_or(0);
        if best.as_ref().map_or(true, |(best_ts, ..)| *best_ts <= ts) {
            let from_me = wmi.key.from_me.unwrap_or(false);
            best = Some((ts, preview, from_me, wmi_local_status(wmi, from_me)));
        }
    }
    match best {
        Some((ts, preview, from_me, status)) => (preview, Some(ts), from_me, status),
        None => (String::new(), None, false, 0),
    }
}
