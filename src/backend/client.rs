//! Construction and lifecycle of the whatsapp-rust [`Bot`], plus translation of
//! its [`Event`]s into our [`WaEvent`]s and into the application [`Store`].
//! Everything here runs on the Tokio runtime thread spawned by [`super::runtime`].

use std::collections::{HashMap, HashSet};
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
use whatsapp_rust::media::{
    audio_message, document_message, image_message, AudioOptions, DocumentOptions, ImageOptions,
};
use whatsapp_rust::waproto::codec;
use whatsapp_rust::UploadOptions;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::waproto::whatsapp::device_props::PlatformType;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use super::bridge::{MediaEntry, WaCommand, WaEvent};
use crate::config;
use crate::model::MessageRow;
use crate::store::{ChatUpsert, Store};
use crate::util::preview;

/// Coalescing window for chat-list snapshots: many history-sync events arrive in
/// a burst, so we wait a moment after a change before querying + pushing.
const SNAPSHOT_DEBOUNCE: Duration = Duration::from_millis(400);

/// Online-presence state for the currently-open chat. Shared between the command
/// loop (which sets the open chat + subscribes) and the event handler (which
/// updates the online set from `Event::Presence`). All JIDs are compared by their
/// `user_base()` so LID/PN/device variants of the same person match.
#[derive(Default)]
struct PresenceState {
    /// Canonical JID of the open chat (empty if none).
    open_jid: String,
    is_group: bool,
    /// Acceptable `user_base`s for a 1:1 chat (PN and LID forms), so presence that
    /// arrives keyed by `@lid` still matches a PN-keyed chat.
    bases: HashSet<String>,
    /// Group members as `(user_base, display name)`.
    members: Vec<(String, String)>,
    /// `user_base` of members currently online.
    online: HashSet<String>,
}

/// Aggregates per-message delivery/read receipts for **group** chats, so a sent
/// message only turns ✓✓ (all delivered) / blue (all read) once every recipient
/// has done so — matching the official clients instead of flipping on the first
/// receipt. In-memory only (re-derived from history-sync status on restart).
#[derive(Default)]
struct TickTracker {
    /// `(chat, msg_id)` → (bases that delivered, bases that read).
    agg: HashMap<(String, String), (HashSet<String>, HashSet<String>)>,
    /// Group JID → recipient count (participants − 1, excluding ourselves).
    recipients: HashMap<String, usize>,
}

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
    // The session DB holds Signal/identity keys in plaintext — lock it to the
    // owner so other local users can't read it.
    crate::store::restrict_db_perms(&db_path);

    info!("opening app database at {app_db_path}");
    let store = Store::open(app_db_path).await?;
    // One-time cleanup: collapse device-suffixed sender JIDs (`user:NN@server`) to
    // their base so a group member who posts from several devices is ONE sender
    // (and matches the contact-name join) instead of appearing as duplicate users.
    if let Err(e) = store.normalize_message_senders().await {
        warn!("normalize_message_senders failed: {e:?}");
    }
    let dirty = Arc::new(Notify::new());
    // Presence tracking for the currently-open chat (online dots under the header).
    let presence: Arc<Mutex<PresenceState>> = Arc::new(Mutex::new(PresenceState::default()));
    // Per-message group receipt aggregation (✓✓/blue only when all recipients done).
    let ticks: Arc<Mutex<TickTracker>> = Arc::new(Mutex::new(TickTracker::default()));

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
    let ev_presence = presence.clone();
    let ev_ticks = ticks.clone();
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
            let presence = ev_presence.clone();
            let ticks = ev_ticks.clone();
            async move {
                handle_event(event, client, tx, store, dirty, presence, ticks).await;
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
            // One-shot: fetch subjects for active groups that lost/never had a name,
            // throttled so the burst of group-metadata queries doesn't hit the 429
            // rate limit. Lazy on-open (OpenChat) covers the rest.
            if let Ok(groups) = store.unnamed_active_groups().await {
                for jid in groups.into_iter().take(30) {
                    if fetch_group_subject(&client, &store, &jid).await {
                        dirty.notify_one();
                    }
                    tokio::time::sleep(Duration::from_millis(800)).await;
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
                // Learn the PN for each @lid-keyed chat from the library's own
                // LID<->PN map (which `lid_map` may not yet hold), so the merge
                // below can collapse duplicates the library already knows about.
                if let Ok(lids) = store.lid_chats().await {
                    for lid in lids {
                        let Ok(jid) = lid.parse::<whatsapp_rust::Jid>() else {
                            continue;
                        };
                        for form in jid_forms(&client, &jid).await {
                            if form.ends_with("@s.whatsapp.net") {
                                let _ = store.learn_lid_pn(lid.clone(), form).await;
                            }
                        }
                    }
                }
                // Collapse @lid/PN duplicate chats now that more pairs are known.
                // NOTE: only the identity-based merge is safe. Name-based merging is
                // intentionally NOT done — many contacts share a first name (13 distinct
                // "Antonio"), so merging by name would fuse different people.
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
    // Inline photo loads: deduped, and throttled to a few concurrent downloads so
    // opening a chat full of images doesn't flood the network.
    let media_inflight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let media_sem = Arc::new(tokio::sync::Semaphore::new(6));

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
                    // the list. `jid` is already the canonical chat key. Capture the
                    // read-receipt target (latest unread incoming) BEFORE clearing, so
                    // we can fan "read" out to our other devices below.
                    let read_target = store.read_receipt_target(jid.clone()).await.ok().flatten();
                    if let Err(e) = store.clear_unread(jid.clone()).await {
                        warn!("clear_unread (open) failed: {e:?}");
                    }
                    dirty.notify_one();
                    // Read-state sync + group-subject backfill are network round-trips;
                    // run them OFF the command loop so the chat opens instantly (the
                    // local history below is sent without waiting on the server).
                    {
                        let cmd_client = cmd_client.clone();
                        let store = store.clone();
                        let dirty = dirty.clone();
                        let jid = jid.clone();
                        tokio::spawn(async move {
                            if let Ok(chat) = jid.parse::<whatsapp_rust::Jid>() {
                                // 1) Sync read-state to our OWN devices via the app-state
                                //    markChatAsRead mutation (does not notify the sender).
                                if let Err(e) = cmd_client
                                    .chat_actions()
                                    .mark_chat_as_read(&chat, true, None)
                                    .await
                                {
                                    warn!("mark_chat_as_read (open) failed: {e:?}");
                                }
                                // 2) Send a read receipt for the latest unread incoming
                                //    message — this is what actually fans "read" out to our
                                //    other devices (the same read-self receipt we receive
                                //    when reading on WhatsApp Web). Whether the SENDER sees a
                                //    blue tick is enforced server-side by the account's
                                //    read-receipt privacy setting (off → no blue tick).
                                if let Some((id, sender)) = read_target {
                                    let sender_jid = if jid.ends_with("@g.us") {
                                        sender.parse::<whatsapp_rust::Jid>().ok()
                                    } else {
                                        None
                                    };
                                    match cmd_client
                                        .mark_as_read(&chat, sender_jid.as_ref(), &[id.as_str()])
                                        .await
                                    {
                                        Ok(_) => info!("mark_as_read sent: chat={jid} id={id}"),
                                        Err(e) => warn!("mark_as_read failed: {e:?}"),
                                    }
                                }
                            }
                            // Lazily resolve a group's subject if we still show its number.
                            if fetch_group_subject(&cmd_client, &store, &jid).await {
                                dirty.notify_one();
                            }
                        });
                    }
                    // Online presence: mark this the open chat now (so live presence
                    // events match), then subscribe + (for groups) load members off
                    // the command loop. Clears the subtitle immediately.
                    {
                        let is_group = jid.ends_with("@g.us");
                        {
                            let mut st = presence.lock().unwrap();
                            st.open_jid = jid.clone();
                            st.is_group = is_group;
                            st.bases.clear();
                            st.members.clear();
                            st.online.clear();
                        }
                        let _ = event_tx
                            .send(WaEvent::PresenceInfo {
                                jid: jid.clone(),
                                is_group,
                                online_names: Vec::new(),
                                total: 0,
                            })
                            .await;
                        let cmd_client = cmd_client.clone();
                        let presence = presence.clone();
                        let store = store.clone();
                        let event_tx = event_tx.clone();
                        let jid_c = jid.clone();
                        tokio::spawn(async move {
                            let Ok(parsed) = jid_c.parse::<whatsapp_rust::Jid>() else {
                                return;
                            };
                            if is_group {
                                if let Ok(meta) = cmd_client.groups().get_metadata(&parsed).await {
                                    let mut members = Vec::new();
                                    for p in &meta.participants {
                                        // Group metadata always carries the phone number,
                                        // so learn every @lid→PN pair here. This lets
                                        // bubble senders and the profile resolve to the
                                        // real number (and dedupe) instead of the raw LID.
                                        if let Some(pn) = &p.phone_number {
                                            let lid = base_jid(&p.jid.to_string());
                                            let pn = base_jid(&pn.to_string());
                                            if lid.ends_with("@lid") && pn.ends_with("@s.whatsapp.net")
                                            {
                                                let _ = store.learn_lid_pn(lid, pn).await;
                                            }
                                        }
                                        let base = p.jid.user_base().to_string();
                                        let name = store
                                            .display_name(p.jid.to_string())
                                            .await
                                            .unwrap_or_default();
                                        let name = if name.is_empty() {
                                            preview::pretty_number(
                                                &p.phone_number
                                                    .as_ref()
                                                    .map(|x| x.to_string())
                                                    .unwrap_or_else(|| p.jid.to_string()),
                                            )
                                        } else {
                                            name
                                        };
                                        members.push((base, name));
                                    }
                                    {
                                        let mut st = presence.lock().unwrap();
                                        if st.open_jid == jid_c {
                                            st.members = members;
                                            let total = st.members.len();
                                            let names: Vec<String> = st
                                                .members
                                                .iter()
                                                .filter(|(b, _)| st.online.contains(b))
                                                .map(|(_, n)| n.clone())
                                                .collect();
                                            let _ = event_tx.try_send(WaEvent::PresenceInfo {
                                                jid: jid_c.clone(),
                                                is_group: true,
                                                online_names: names,
                                                total,
                                            });
                                        }
                                    }
                                    // Subscribe to members' presence (capped/throttled).
                                    let n = meta.participants.len().min(60);
                                    for p in meta.participants.iter().take(60) {
                                        let _ = cmd_client.presence().subscribe(p.jid.clone()).await;
                                    }
                                    info!("presence: subscribed to {n} group member(s)");
                                }
                            } else {
                                // Accept presence under any JID form (PN and LID) of this
                                // contact, since it often arrives keyed by @lid.
                                let bases: HashSet<String> = jid_forms(&cmd_client, &parsed)
                                    .await
                                    .iter()
                                    .filter_map(|f| f.parse::<whatsapp_rust::Jid>().ok())
                                    .map(|j| j.user_base().to_string())
                                    .collect();
                                {
                                    let mut st = presence.lock().unwrap();
                                    if st.open_jid == jid_c {
                                        st.bases = bases;
                                    }
                                }
                                let _ = cmd_client.presence().subscribe(parsed).await;
                                info!("presence: subscribed to contact {jid_c}");
                            }
                        });
                    }
                    // Load a small first page; older messages backfill on scroll-up
                    // (LoadOlder). A large initial page keeps many bubbles — and their
                    // inline image textures — resident at once, inflating RAM.
                    match store.load_messages(jid.clone(), 80).await {
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
                Ok(WaCommand::SendText { jid, text, quote }) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        match jid.parse::<whatsapp_rust::Jid>() {
                            Ok(to) => {
                                // A reply must carry the quote in a ContextInfo on an
                                // extended-text message; a plain message uses send_text.
                                let res = if let Some(q) = &quote {
                                    let msg = wa::Message {
                                        extended_text_message: Some(Box::new(
                                            wa::message::ExtendedTextMessage {
                                                text: Some(text.clone()),
                                                context_info: Some(Box::new(wa::ContextInfo {
                                                    stanza_id: Some(q.id.clone()),
                                                    participant: (!q.sender.is_empty())
                                                        .then(|| q.sender.clone()),
                                                    quoted_message: Some(Box::new(wa::Message {
                                                        conversation: Some(q.body.clone()),
                                                        ..Default::default()
                                                    })),
                                                    ..Default::default()
                                                })),
                                                ..Default::default()
                                            },
                                        )),
                                        ..Default::default()
                                    };
                                    cmd_client.send_message(to, msg).await
                                } else {
                                    cmd_client.send_text(to, text.clone()).await
                                };
                                match res {
                                    Ok(res) => {
                                        let reply = quote.map(|q| (q.sender, q.body));
                                        store_outgoing(&store, &event_tx, &dirty, jid, text, res.message_id, None, reply).await;
                                    }
                                    Err(e) => {
                                        warn!("send_text failed: {e:?}");
                                        let _ = event_tx.send(WaEvent::Error(format!("Invio fallito: {e}"))).await;
                                    }
                                }
                            }
                            Err(e) => warn!("send_text: bad jid {jid}: {e:?}"),
                        }
                    }
                }
                Ok(WaCommand::SendAudio { jid, ogg, duration, waveform, quote }) => {
                    match jid.parse::<whatsapp_rust::Jid>() {
                        Ok(to) => match cmd_client.upload(ogg, MediaType::Audio, UploadOptions::default()).await {
                            Ok(up) => {
                                let wf = if waveform.is_empty() { None } else { Some(waveform.clone()) };
                                let mut msg = audio_message(up, AudioOptions {
                                    ptt: Some(true),
                                    duration_seconds: Some(duration),
                                    mimetype: None,
                                    waveform: wf,
                                });
                                // A reply carries the citation in a ContextInfo on the
                                // audio sub-message (same shape as a text reply).
                                if let Some(q) = &quote {
                                    if let Some(am) = msg.audio_message.as_mut() {
                                        am.context_info = Some(Box::new(wa::ContextInfo {
                                            stanza_id: Some(q.id.clone()),
                                            participant: (!q.sender.is_empty())
                                                .then(|| q.sender.clone()),
                                            quoted_message: Some(Box::new(wa::Message {
                                                conversation: Some(q.body.clone()),
                                                ..Default::default()
                                            })),
                                            ..Default::default()
                                        }));
                                    }
                                }
                                // Keep the media proto so our own note is replayable.
                                let media = codec::message_to_vec(&msg);
                                let reply = quote.map(|q| (q.sender, q.body));
                                match cmd_client.send_message(to, msg).await {
                                    Ok(res) => {
                                        store_outgoing(&store, &event_tx, &dirty, jid, "🎤 Messaggio vocale".to_string(), res.message_id, Some(OutgoingMedia { proto: media, kind: 3, mime: "audio/ogg".to_string(), name: String::new(), thumb: Vec::new(), size: 0, audio_secs: duration, audio_waveform: waveform }), reply).await;
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
                Ok(WaCommand::SendImage { jid, data, mime, caption, quote }) => {
                    match jid.parse::<whatsapp_rust::Jid>() {
                        Ok(to) => {
                            let size = data.len() as i64;
                            match cmd_client.upload(data.clone(), MediaType::Image, UploadOptions::default()).await {
                                Ok(up) => {
                                    let mut msg = image_message(up, ImageOptions {
                                        caption: caption.clone(),
                                        mimetype: Some(mime.clone()),
                                        jpeg_thumbnail: None,
                                    });
                                    if let Some(q) = &quote {
                                        if let Some(im) = msg.image_message.as_mut() {
                                            im.context_info = Some(Box::new(reply_context(q)));
                                        }
                                    }
                                    let proto = codec::message_to_vec(&msg);
                                    let reply = quote.map(|q| (q.sender, q.body));
                                    match cmd_client.send_message(to, msg).await {
                                        Ok(res) => {
                                            // Cache local bytes under the message id so our
                                            // own bubble shows it without re-downloading.
                                            let ext = ext_from_mime(Some(&mime), "jpg");
                                            if let Ok(path) = config::media_cache_path(&res.message_id, &ext) {
                                                let _ = std::fs::write(&path, &data);
                                            }
                                            let body = caption.clone().filter(|c| !c.is_empty()).map(|c| format!("📷 Foto: {c}")).unwrap_or_else(|| "📷 Foto".to_string());
                                            store_outgoing(&store, &event_tx, &dirty, jid, body, res.message_id, Some(OutgoingMedia { proto, kind: 1, mime, name: String::new(), thumb: Vec::new(), size, audio_secs: 0, audio_waveform: Vec::new() }), reply).await;
                                        }
                                        Err(e) => {
                                            warn!("send image failed: {e:?}");
                                            let _ = event_tx.send(WaEvent::Error(format!("Invio immagine fallito: {e}"))).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("image upload failed: {e:?}");
                                    let _ = event_tx.send(WaEvent::Error(format!("Upload immagine fallito: {e}"))).await;
                                }
                            }
                        }
                        Err(e) => warn!("send_image: bad jid {jid}: {e:?}"),
                    }
                }
                Ok(WaCommand::SendDocument { jid, data, mime, file_name, quote }) => {
                    match jid.parse::<whatsapp_rust::Jid>() {
                        Ok(to) => {
                            let size = data.len() as i64;
                            match cmd_client.upload(data.clone(), MediaType::Document, UploadOptions::default()).await {
                                Ok(up) => {
                                    let mut msg = document_message(up, DocumentOptions {
                                        mimetype: Some(mime.clone()),
                                        file_name: Some(file_name.clone()),
                                        title: Some(file_name.clone()),
                                        caption: None,
                                        page_count: None,
                                        jpeg_thumbnail: None,
                                    });
                                    if let Some(q) = &quote {
                                        if let Some(dm) = msg.document_message.as_mut() {
                                            dm.context_info = Some(Box::new(reply_context(q)));
                                        }
                                    }
                                    let proto = codec::message_to_vec(&msg);
                                    let reply = quote.map(|q| (q.sender, q.body));
                                    match cmd_client.send_message(to, msg).await {
                                        Ok(res) => {
                                            let ext = doc_ext_from(&file_name, &mime);
                                            if let Ok(path) = config::media_cache_path(&res.message_id, &ext) {
                                                let _ = std::fs::write(&path, &data);
                                            }
                                            store_outgoing(&store, &event_tx, &dirty, jid, format!("📄 {file_name}"), res.message_id, Some(OutgoingMedia { proto, kind: 4, mime, name: file_name, thumb: Vec::new(), size, audio_secs: 0, audio_waveform: Vec::new() }), reply).await;
                                        }
                                        Err(e) => {
                                            warn!("send document failed: {e:?}");
                                            let _ = event_tx.send(WaEvent::Error(format!("Invio documento fallito: {e}"))).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("document upload failed: {e:?}");
                                    let _ = event_tx.send(WaEvent::Error(format!("Upload documento fallito: {e}"))).await;
                                }
                            }
                        }
                        Err(e) => warn!("send_document: bad jid {jid}: {e:?}"),
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
                Ok(WaCommand::DownloadMedia { chat_jid, id }) => {
                    // Decode the stored proto → media sub-message → download + decrypt
                    // → cache on disk; reply with MediaReady (cache hit replies fast).
                    let msg = match store.get_media(chat_jid.clone(), id.clone()).await {
                        Ok(Some(b)) => match codec::message_decode(&b) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("decode media proto failed: {e:?}");
                                continue;
                            }
                        },
                        Ok(None) => {
                            warn!("no media stored for {id}");
                            continue;
                        }
                        Err(e) => {
                            warn!("get_media failed: {e:?}");
                            continue;
                        }
                    };
                    // Resolve (kind, extension) without downloading yet.
                    let kind_ext = if let Some(m) = msg.image_message.as_ref() {
                        Some((1, ext_from_mime(m.mimetype.as_deref(), "jpg")))
                    } else if let Some(m) = msg.video_message.as_ref() {
                        Some((2, ext_from_mime(m.mimetype.as_deref(), "mp4")))
                    } else if let Some(m) = msg.document_message.as_ref() {
                        Some((4, doc_ext(m)))
                    } else if let Some(m) = msg.sticker_message.as_ref() {
                        Some((5, ext_from_mime(m.mimetype.as_deref(), "webp")))
                    } else {
                        None
                    };
                    let Some((kind, ext)) = kind_ext else {
                        warn!("DownloadMedia: unsupported media for {id}");
                        continue;
                    };
                    let path = match config::media_cache_path(&id, &ext) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("media_cache_path failed: {e:?}");
                            continue;
                        }
                    };
                    if path.exists() {
                        let _ = event_tx
                            .send(WaEvent::MediaReady { kind, path: path.to_string_lossy().into_owned() })
                            .await;
                        continue;
                    }
                    let data = if let Some(m) = msg.image_message.as_ref() {
                        cmd_client.download(&**m).await
                    } else if let Some(m) = msg.video_message.as_ref() {
                        cmd_client.download(&**m).await
                    } else if let Some(m) = msg.document_message.as_ref() {
                        cmd_client.download(&**m).await
                    } else if let Some(m) = msg.sticker_message.as_ref() {
                        cmd_client.download(&**m).await
                    } else {
                        continue;
                    };
                    match data {
                        Ok(bytes) => {
                            if let Err(e) = std::fs::write(&path, &bytes) {
                                warn!("write media failed: {e:?}");
                            } else {
                                let _ = event_tx
                                    .send(WaEvent::MediaReady { kind, path: path.to_string_lossy().into_owned() })
                                    .await;
                            }
                        }
                        Err(e) => {
                            warn!("media download failed: {e:?}");
                            let _ = event_tx.send(WaEvent::Error(format!("Download media fallito: {e}"))).await;
                        }
                    }
                }
                Ok(WaCommand::LoadInline { chat_jid, id }) => {
                    spawn_load_inline(
                        chat_jid,
                        id,
                        &store,
                        &cmd_client,
                        &event_tx,
                        &media_inflight,
                        &media_sem,
                    );
                }
                Ok(WaCommand::FetchProfile(jid)) => {
                    if let Ok(parsed) = jid.parse::<whatsapp_rust::Jid>() {
                        let is_group = jid.ends_with("@g.us");
                        let pic_path = fetch_full_pic(&cmd_client, &jid).await;
                        // rows carry the participant/group JID so the UI can make
                        // them clickable: (jid, display name, subtitle).
                        let (title, subtitle, status, blocked, rows) = if is_group {
                            match cmd_client.groups().get_metadata(&parsed).await {
                                Ok(meta) => {
                                    // Dedupe participants by canonical IDENTITY (not name):
                                    // during LID migration the same member can be listed
                                    // under both an @lid and a phone-number form. Key by the
                                    // phone number when known (else the lid base) and keep
                                    // the phone-number entry, so each person appears once.
                                    let mut idx: HashMap<String, usize> = HashMap::new();
                                    // (jid, display, number, has_pn)
                                    let mut acc: Vec<(String, String, String, bool)> = Vec::new();
                                    for p in &meta.participants {
                                        let mut pn = p.phone_number.as_ref().map(|x| x.to_string());
                                        // Resolve an @lid-only participant to its phone number
                                        // via the library's LID map, so it dedupes with the PN
                                        // entry and shows a real number/name instead of the raw
                                        // LID digits (the "numbers without +39").
                                        if pn.is_none() {
                                            pn = jid_forms(&cmd_client, &p.jid)
                                                .await
                                                .into_iter()
                                                .find(|f| f.ends_with("@s.whatsapp.net"));
                                        }
                                        let key = match &pn {
                                            Some(pn) => pn
                                                .parse::<whatsapp_rust::Jid>()
                                                .map(|j| j.user_base().to_string())
                                                .unwrap_or_else(|_| pn.clone()),
                                            None => p.jid.user_base().to_string(),
                                        };
                                        let has_pn = pn.is_some();
                                        let jid_str = pn.clone().unwrap_or_else(|| p.jid.to_string());
                                        let number = preview::pretty_number(
                                            &pn.unwrap_or_else(|| p.jid.to_string()),
                                        );
                                        let name =
                                            store.display_name(jid_str.clone()).await.unwrap_or_default();
                                        let display = if name.is_empty() { number.clone() } else { name };
                                        match idx.get(&key) {
                                            Some(&i) => {
                                                // Upgrade to the phone-number form if we had a lid one.
                                                if has_pn && !acc[i].3 {
                                                    acc[i] = (jid_str, display, number, has_pn);
                                                }
                                            }
                                            None => {
                                                idx.insert(key, acc.len());
                                                acc.push((jid_str, display, number, has_pn));
                                            }
                                        }
                                    }
                                    let rows: Vec<(String, String, String)> =
                                        acc.into_iter().map(|(j, d, n, _)| (j, d, n)).collect();
                                    let sub = format!("Gruppo · {} partecipanti", rows.len());
                                    (meta.subject.clone(), sub, meta.description.clone().unwrap_or_default(), false, rows)
                                }
                                Err(e) => {
                                    // Network failed (e.g. 429): fall back to the members
                                    // OpenChat loaded into presence, deduped by base.
                                    warn!("group metadata failed: {e:?}");
                                    let cached: Vec<(String, String)> = {
                                        let st = presence.lock().unwrap();
                                        if st.open_jid == jid { st.members.clone() } else { Vec::new() }
                                    };
                                    let mut seen = HashSet::new();
                                    let mut rows = Vec::new();
                                    for (base, name) in &cached {
                                        if !seen.insert(base.clone()) {
                                            continue;
                                        }
                                        let number = preview::pretty_number(base);
                                        let display =
                                            if name.is_empty() { number.clone() } else { name.clone() };
                                        rows.push((format!("{base}@s.whatsapp.net"), display, number));
                                    }
                                    let title = store.display_name(jid.clone()).await.unwrap_or_default();
                                    let title = if title.is_empty() {
                                        preview::pretty_number(&jid)
                                    } else {
                                        title
                                    };
                                    let sub = format!("Gruppo · {} partecipanti", rows.len());
                                    (title, sub, String::new(), false, rows)
                                }
                            }
                        } else {
                            let number = preview::pretty_number(&jid);
                            let name = store.display_name(jid.clone()).await.unwrap_or_default();
                            let title = if name.is_empty() { number.clone() } else { name };
                            // About/status text + block state for the 1:1 panel.
                            let status = match cmd_client.contacts().get_user_info(std::slice::from_ref(&parsed)).await {
                                Ok(info) => info.values().next().and_then(|u| u.status.clone()).unwrap_or_default(),
                                Err(e) => { warn!("get_user_info (status) failed: {e:?}"); String::new() }
                            };
                            let blocked = cmd_client.blocking().is_blocked(&parsed).await.unwrap_or(false);
                            let mut rows = Vec::new();
                            match cmd_client.groups().get_participating().await {
                                Ok(groups) => {
                                    for (gjid, meta) in &groups {
                                        let want = parsed.user_base();
                                        let common = meta.participants.iter().any(|p| {
                                            p.jid.user_base() == want
                                                || p.phone_number
                                                    .as_ref()
                                                    .is_some_and(|pn| pn.user_base() == want)
                                        });
                                        if common {
                                            rows.push((gjid.to_string(), meta.subject.clone(), String::new()));
                                        }
                                    }
                                }
                                Err(e) => warn!("get_participating failed: {e:?}"),
                            }
                            rows.sort_by(|a, b| a.1.cmp(&b.1));
                            (title, number, status, blocked, rows)
                        };
                        let media_count = store.chat_media_count(jid.clone()).await.unwrap_or(0);
                        let _ = event_tx
                            .send(WaEvent::Profile { is_group, jid: jid.clone(), title, subtitle, status, pic_path, blocked, rows, media_count })
                            .await;
                    }
                }
                Ok(WaCommand::FetchChatMedia(jid)) => {
                    let items = store.chat_media(jid.clone()).await.unwrap_or_default();
                    let (mut photos, mut videos, mut documents) = (Vec::new(), Vec::new(), Vec::new());
                    for it in items {
                        let ext = if it.kind == 4 {
                            doc_ext_from(&it.name, &it.mime)
                        } else {
                            ext_from_mime(Some(&it.mime), if it.kind == 2 { "mp4" } else { "jpg" })
                        };
                        let cached = config::media_cache_path(&it.id, &ext)
                            .ok()
                            .filter(|p| p.exists())
                            .map(|p| p.to_string_lossy().into_owned());
                        let entry = MediaEntry { id: it.id, name: it.name, size: it.size, thumb: it.thumb, cached };
                        match it.kind {
                            1 => photos.push(entry),
                            2 => videos.push(entry),
                            _ => documents.push(entry),
                        }
                    }
                    // Extract unique links from message bodies.
                    let mut links = Vec::new();
                    let mut seen = HashSet::new();
                    for body in store.chat_link_bodies(jid.clone()).await.unwrap_or_default() {
                        for url in crate::util::text::find_urls(&body) {
                            if seen.insert(url.clone()) {
                                links.push(url);
                            }
                        }
                    }
                    let _ = event_tx
                        .send(WaEvent::ChatMedia { jid, photos, videos, documents, links })
                        .await;
                }
                Ok(WaCommand::SetArchived { jids, archived }) => {
                    for jid in jids {
                        let Ok(parsed) = jid.parse::<whatsapp_rust::Jid>() else {
                            continue;
                        };
                        let res = if archived {
                            cmd_client.chat_actions().archive_chat(&parsed, None).await
                        } else {
                            cmd_client.chat_actions().unarchive_chat(&parsed, None).await
                        };
                        if let Err(e) = res {
                            warn!("archive_chat({archived}) failed for {jid}: {e:?}");
                        }
                        // Reflect locally under every JID form the chat may be keyed by.
                        for form in jid_forms(&cmd_client, &parsed).await {
                            let _ = store.set_archived(form, archived).await;
                        }
                    }
                    dirty.notify_one();
                }
                Ok(WaCommand::SetPinned { jid, pinned }) => {
                    if let Ok(parsed) = jid.parse::<whatsapp_rust::Jid>() {
                        let res = if pinned {
                            cmd_client.chat_actions().pin_chat(&parsed).await
                        } else {
                            cmd_client.chat_actions().unpin_chat(&parsed).await
                        };
                        if let Err(e) = res {
                            warn!("pin_chat({pinned}) failed for {jid}: {e:?}");
                        }
                        for form in jid_forms(&cmd_client, &parsed).await {
                            let _ = store.set_pinned(form, pinned).await;
                        }
                        dirty.notify_one();
                    }
                }
                Ok(WaCommand::SetBlocked { jid, blocked }) => {
                    if let Ok(parsed) = jid.parse::<whatsapp_rust::Jid>() {
                        let res = if blocked {
                            cmd_client.blocking().block(&parsed).await
                        } else {
                            cmd_client.blocking().unblock(&parsed).await
                        };
                        if let Err(e) = res {
                            warn!("block({blocked}) failed for {jid}: {e:?}");
                            let _ = event_tx
                                .send(WaEvent::Error(format!("Operazione blocca fallita: {e}")))
                                .await;
                        }
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

/// Spawns a deduped, throttled background download of a photo for inline display,
/// replying with [`WaEvent::InlineReady`] when the file is cached.
fn spawn_load_inline(
    chat_jid: String,
    id: String,
    store: &Store,
    client: &Arc<Client>,
    tx: &Sender<WaEvent>,
    inflight: &Arc<Mutex<HashSet<String>>>,
    sem: &Arc<tokio::sync::Semaphore>,
) {
    let store = store.clone();
    let client = client.clone();
    let tx = tx.clone();
    let inflight = inflight.clone();
    let sem = sem.clone();
    tokio::spawn(async move {
        if !inflight.lock().unwrap().insert(id.clone()) {
            return; // already loading
        }
        let result = load_inline_image(&store, &client, &sem, &chat_jid, &id).await;
        inflight.lock().unwrap().remove(&id);
        match result {
            Ok(Some(path)) => {
                let _ = tx
                    .send(WaEvent::InlineReady { id, path: path.to_string_lossy().into_owned() })
                    .await;
            }
            Ok(None) => {}
            Err(e) => warn!("inline load failed for {id}: {e:?}"),
        }
    });
}

/// Downloads (or cache-hits) a photo message's full image, returning its path.
async fn load_inline_image(
    store: &Store,
    client: &Arc<Client>,
    sem: &Arc<tokio::sync::Semaphore>,
    chat_jid: &str,
    id: &str,
) -> Result<Option<std::path::PathBuf>> {
    let Some(bytes) = store.get_media(chat_jid.to_string(), id.to_string()).await? else {
        return Ok(None);
    };
    let msg = codec::message_decode(&bytes)?;
    let Some(img) = msg.image_message.as_ref() else {
        return Ok(None);
    };
    let ext = ext_from_mime(img.mimetype.as_deref(), "jpg");
    let path = config::media_cache_path(id, &ext)?;
    if !path.exists() {
        let _permit = sem.acquire().await?;
        let data = client.download(&**img).await?;
        std::fs::write(&path, &data)?;
    }
    Ok(Some(path))
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

/// Downloads (and disk-caches) the full-res profile picture for `jid`, returning
/// the cache path. Uses the cache if present; `None` if the contact has none.
async fn fetch_full_pic(client: &Arc<Client>, jid: &str) -> Option<String> {
    let path = config::profile_pic_path(jid).ok()?;
    if path.exists() {
        return Some(path.to_string_lossy().into_owned());
    }
    let parsed: whatsapp_rust::Jid = jid.parse().ok()?;
    let pic = client
        .contacts()
        .get_profile_picture(&parsed, false)
        .await
        .ok()
        .flatten()?;
    if pic.url.is_empty() {
        return None;
    }
    let url = pic.url.clone();
    let dest = path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut body = ureq::get(&url).call()?.into_body();
        let bytes = body.read_to_vec()?;
        std::fs::write(&dest, &bytes)?;
        Ok(())
    })
    .await
    .ok()?
    .ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Media payload to persist alongside an outgoing message (proto + metadata).
struct OutgoingMedia {
    proto: Vec<u8>,
    kind: i32,
    mime: String,
    name: String,
    thumb: Vec<u8>,
    size: i64,
    audio_secs: u32,
    audio_waveform: Vec<u8>,
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
    media: Option<OutgoingMedia>,
    reply: Option<(String, String)>, // (quoted author jid, quoted preview)
) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (reply_sender, reply_text) = reply.clone().unwrap_or_default();
    let row = MessageRow {
        id: id.clone(),
        chat_jid: chat.clone(),
        sender_jid: String::new(),
        sender_name: String::new(),
        from_me: true,
        ts,
        body: body.clone(),
        status: 1,
        audio: media.as_ref().is_some_and(|m| m.kind == 3),
        audio_secs: media.as_ref().map(|m| m.audio_secs).unwrap_or(0),
        audio_waveform: media.as_ref().map(|m| m.audio_waveform.clone()).unwrap_or_default(),
        reply_text: reply_text.clone(),
        reply_sender_name: if reply_sender.is_empty() {
            String::new()
        } else {
            crate::util::preview::pretty_number(&reply_sender)
        },
        media_kind: media.as_ref().map(|m| m.kind).unwrap_or(0),
        media_mime: media.as_ref().map(|m| m.mime.clone()).unwrap_or_default(),
        media_name: media.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
        media_thumb: media.as_ref().map(|m| m.thumb.clone()).unwrap_or_default(),
        media_size: media.as_ref().map(|m| m.size).unwrap_or(0),
    };
    if let Err(e) = store.insert_message(row.clone()).await {
        warn!("store_outgoing insert failed: {e:?}");
    }
    if let Some(m) = media {
        if let Err(e) = store.set_media(chat.clone(), id.clone(), m.proto).await {
            warn!("store_outgoing set_media failed: {e:?}");
        }
        if let Err(e) = store
            .set_media_meta(chat.clone(), id.clone(), m.kind, m.mime, m.name, m.thumb, m.size)
            .await
        {
            warn!("store_outgoing set_media_meta failed: {e:?}");
        }
        if m.kind == 3 {
            if let Err(e) = store
                .set_audio_meta(chat.clone(), id.clone(), m.audio_secs, m.audio_waveform)
                .await
            {
                warn!("store_outgoing set_audio_meta failed: {e:?}");
            }
        }
    }
    if !reply_text.is_empty() {
        if let Err(e) = store
            .set_reply(chat.clone(), id, reply_sender, reply_text)
            .await
        {
            warn!("store_outgoing set_reply failed: {e:?}");
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
    presence: Arc<Mutex<PresenceState>>,
    ticks: Arc<Mutex<TickTracker>>,
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
                #[allow(clippy::type_complexity)]
                let mut msgs: Vec<(MessageRow, Option<Vec<u8>>, Option<(String, String)>)> =
                    Vec::new();
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
                    let from_me = msgs.iter().filter(|(m, ..)| m.from_me).count();
                    let with_status = msgs.iter().filter(|(m, ..)| m.status >= 2).count();
                    info!(
                        "history sync: storing {} messages ({from_me} ours, {with_status} already ✓✓/read)",
                        msgs.len()
                    );
                    // Media proto + metadata to persist after the rows exist, so any
                    // media (photo/video/audio/document/sticker) is downloadable later.
                    #[allow(clippy::type_complexity)]
                    let media: Vec<(MessageRow, Vec<u8>)> = msgs
                        .iter()
                        .filter_map(|(m, media, _)| media.as_ref().map(|b| (m.clone(), b.clone())))
                        .collect();
                    // Reply quotes to persist (quoted author jid + preview).
                    let replies: Vec<(String, String, String, String)> = msgs
                        .iter()
                        .filter_map(|(m, _, reply)| {
                            reply.as_ref().map(|(sender, text)| {
                                (m.chat_jid.clone(), m.id.clone(), sender.clone(), text.clone())
                            })
                        })
                        .collect();
                    let rows: Vec<MessageRow> = msgs.into_iter().map(|(m, ..)| m).collect();
                    if let Err(e) = store.insert_messages(rows).await {
                        warn!("insert_messages failed: {e:?}");
                    }
                    for (m, bytes) in media {
                        if let Err(e) = store.set_media(m.chat_jid.clone(), m.id.clone(), bytes).await {
                            warn!("set_media (history) failed: {e:?}");
                        }
                        if let Err(e) = store
                            .set_media_meta(m.chat_jid.clone(), m.id.clone(), m.media_kind, m.media_mime.clone(), m.media_name.clone(), m.media_thumb.clone(), m.media_size)
                            .await
                        {
                            warn!("set_media_meta (history) failed: {e:?}");
                        }
                        if m.media_kind == 3 {
                            if let Err(e) = store
                                .set_audio_meta(m.chat_jid, m.id, m.audio_secs, m.audio_waveform)
                                .await
                            {
                                warn!("set_audio_meta (history) failed: {e:?}");
                            }
                        }
                    }
                    for (chat_jid, id, sender, text) in replies {
                        if let Err(e) = store.set_reply(chat_jid, id, sender, text).await {
                            warn!("set_reply (history) failed: {e:?}");
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
                    // Keep the media proto + metadata so any media (photo/video/
                    // audio/document/sticker) can be downloaded + rendered later.
                    let media_info = media_meta(msg);
                    let media_proto = media_info.as_ref().map(|_| codec::message_to_vec(msg));
                    let (m_kind, m_mime, m_name, m_thumb, m_size) = media_info
                        .clone()
                        .unwrap_or((0, String::new(), String::new(), Vec::new(), 0));
                    let (audio_secs, audio_waveform) = audio_meta(msg);
                    let reply = quote_of(msg);
                    let (reply_text, reply_sender_name) = reply
                        .as_ref()
                        .map(|(sender, text)| (text.clone(), reply_name(sender)))
                        .unwrap_or_default();
                    let row = MessageRow {
                        id: info.id.clone(),
                        chat_jid: chat.clone(),
                        sender_jid: base_jid(&info.source.sender.to_string()),
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
                        audio: m_kind == 3,
                        audio_secs,
                        audio_waveform: audio_waveform.clone(),
                        reply_text,
                        reply_sender_name,
                        media_kind: m_kind,
                        media_mime: m_mime.clone(),
                        media_name: m_name.clone(),
                        media_thumb: m_thumb.clone(),
                        media_size: m_size,
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
                        if let Some(bytes) = media_proto {
                            if let Err(e) = store.set_media(chat.clone(), info.id.clone(), bytes).await {
                                warn!("set_media (live) failed: {e:?}");
                            }
                            if let Err(e) = store
                                .set_media_meta(chat.clone(), info.id.clone(), m_kind, m_mime.clone(), m_name.clone(), m_thumb.clone(), m_size)
                                .await
                            {
                                warn!("set_media_meta (live) failed: {e:?}");
                            }
                            if m_kind == 3 {
                                if let Err(e) = store
                                    .set_audio_meta(chat.clone(), info.id.clone(), audio_secs, audio_waveform.clone())
                                    .await
                                {
                                    warn!("set_audio_meta (live) failed: {e:?}");
                                }
                            }
                        }
                        if let Some((sender, text)) = reply {
                            if let Err(e) = store
                                .set_reply(chat.clone(), info.id.clone(), sender, text)
                                .await
                            {
                                warn!("set_reply (live) failed: {e:?}");
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
            let forms = jid_forms(&client, &u.jid).await;
            info!(
                "mark-read: jid={} read={:?} full_sync={} -> forms={:?}",
                u.jid, u.action.read, u.from_full_sync, forms
            );
            match u.action.read {
                Some(true) => {
                    for jid in &forms {
                        if let Err(e) = store.clear_unread(jid.clone()).await {
                            warn!("clear_unread failed: {e:?}");
                        }
                    }
                }
                Some(false) => {
                    for jid in &forms {
                        if let Err(e) = store.set_unread(jid.clone(), 1).await {
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
            // We read the chat elsewhere (phone / another linked device): clear its
            // unread badge here too, so the count stays in sync across clients.
            // NB: on a read-self receipt `source.chat` is OUR OWN reading device
            // (e.g. `<ownlid>:19@lid`), NOT the conversation — so resolve the real
            // chat(s) from the read message ids; fall back to recipient/chat.
            if matches!(r.r#type, ReceiptType::ReadSelf | ReceiptType::PlayedSelf) {
                let mut chats = store
                    .chats_for_messages(r.message_ids.clone())
                    .await
                    .unwrap_or_default();
                if chats.is_empty() {
                    let fallback = r.source.recipient.clone().unwrap_or(r.source.chat.clone());
                    chats.push(canonical_chat_jid(&client, &fallback).await);
                }
                info!("read-self: clearing unread for {:?}", chats);
                for chat in chats {
                    if let Err(e) = store.clear_unread(chat).await {
                        warn!("clear_unread (read-self) failed: {e:?}");
                    }
                }
                dirty.notify_one();
            }
            if let Some(status) = receipt_status(&r.r#type) {
                let chat = canonical_chat_jid(&client, &r.source.chat).await;
                // Group chats: aggregate per-recipient receipts so the tick only
                // advances to ✓✓ (all delivered) / blue (all read) once everyone
                // has, like the official clients. 1:1 keeps the direct behavior.
                let group_recip = if chat.ends_with("@g.us") && status >= 2 {
                    let cached = ticks.lock().unwrap().recipients.get(&chat).copied();
                    match cached {
                        Some(n) => Some(n),
                        None => {
                            let n = match chat.parse::<whatsapp_rust::Jid>() {
                                Ok(p) => client
                                    .groups()
                                    .get_metadata(&p)
                                    .await
                                    .ok()
                                    .map(|m| m.participants.len().saturating_sub(1)),
                                Err(_) => None,
                            };
                            if let Some(n) = n {
                                ticks.lock().unwrap().recipients.insert(chat.clone(), n);
                            }
                            n
                        }
                    }
                } else {
                    None
                };

                if let Some(recip) = group_recip {
                    let sender_base = r.source.sender.user_base().to_string();
                    for id in &r.message_ids {
                        let target = {
                            let mut st = ticks.lock().unwrap();
                            let key = (chat.clone(), id.clone());
                            let entry = st.agg.entry(key.clone()).or_default();
                            entry.0.insert(sender_base.clone()); // delivered
                            if status >= 3 {
                                entry.1.insert(sender_base.clone()); // read
                            }
                            let t = if entry.1.len() >= recip {
                                3
                            } else if entry.0.len() >= recip {
                                2
                            } else {
                                1
                            };
                            if t >= 3 {
                                st.agg.remove(&key);
                            }
                            t
                        };
                        match store
                            .update_message_status(chat.clone(), id.clone(), target)
                            .await
                        {
                            Ok(n) if n > 0 => {
                                let _ = tx
                                    .send(WaEvent::ReceiptUpdate {
                                        chat_jid: chat.clone(),
                                        message_ids: vec![id.clone()],
                                        status: target,
                                    })
                                    .await;
                            }
                            Ok(_) => {}
                            Err(e) => warn!("update_message_status failed: {e:?}"),
                        }
                    }
                    dirty.notify_one();
                    return;
                }

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
        // Online presence for the open chat: update the online set and push the
        // header subtitle data. Matched by `user_base` so LID/PN variants align.
        Event::Presence(p) => {
            let base = p.from.user_base().to_string();
            info!("presence from={} unavailable={}", p.from, p.unavailable);
            let emit = {
                let mut st = presence.lock().unwrap();
                if st.open_jid.is_empty() {
                    None
                } else if st.is_group {
                    if st.members.iter().any(|(b, _)| b == &base) {
                        if p.unavailable {
                            st.online.remove(&base);
                        } else {
                            st.online.insert(base.clone());
                        }
                        let names: Vec<String> = st
                            .members
                            .iter()
                            .filter(|(b, _)| st.online.contains(b))
                            .map(|(_, n)| n.clone())
                            .collect();
                        Some(WaEvent::PresenceInfo {
                            jid: st.open_jid.clone(),
                            is_group: true,
                            online_names: names,
                            total: st.members.len(),
                        })
                    } else {
                        None
                    }
                } else {
                    // 1:1: match any known JID form (PN/LID) of the open contact.
                    if st.bases.contains(&base) {
                        let names = if p.unavailable {
                            Vec::new()
                        } else {
                            vec!["online".to_string()]
                        };
                        Some(WaEvent::PresenceInfo {
                            jid: st.open_jid.clone(),
                            is_group: false,
                            online_names: names,
                            total: 1,
                        })
                    } else {
                        None
                    }
                }
            };
            if let Some(ev) = emit {
                let _ = tx.send(ev).await;
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
/// Fetches a group's subject and stores it as the chat name, but only if the
/// group is currently unnamed. Returns `true` if a new subject was stored (so the
/// caller can refresh the list). No-op for non-groups or already-named groups.
async fn fetch_group_subject(client: &Client, store: &Store, jid: &str) -> bool {
    if !jid.ends_with("@g.us") {
        return false;
    }
    match store.chat_name(jid.to_string()).await {
        Ok(name) if !name.is_empty() => return false,
        Ok(_) => {}
        Err(e) => {
            warn!("chat_name failed: {e:?}");
            return false;
        }
    }
    let Ok(parsed) = jid.parse::<whatsapp_rust::Jid>() else {
        return false;
    };
    match client.groups().get_metadata(&parsed).await {
        Ok(meta) if !meta.subject.is_empty() => {
            match store.set_group_subject(jid.to_string(), meta.subject).await {
                Ok(()) => true,
                Err(e) => {
                    warn!("set_group_subject failed: {e:?}");
                    false
                }
            }
        }
        Ok(_) => false,
        Err(e) => {
            warn!("group subject fetch failed for {jid}: {e:?}");
            false
        }
    }
}

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

/// Strips a JID's device suffix (`user:NN@server` → `user@server`) so all of a
/// person's linked devices collapse to one sender identity. A group member posting
/// from multiple devices otherwise appears as several "users", and the
/// device-suffixed JID misses the contact-name join (showing a bare number).
fn base_jid(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, server)) => {
            let user = user.split(':').next().unwrap_or(user);
            format!("{user}@{server}")
        }
        None => jid.to_string(),
    }
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
#[allow(clippy::type_complexity)]
fn conv_to_messages(
    c: &wa::Conversation,
    chat: &str,
) -> Vec<(MessageRow, Option<Vec<u8>>, Option<(String, String)>)> {
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
            let sender_jid = base_jid(
                &wmi.key
                    .participant
                    .clone()
                    .unwrap_or_else(|| chat.to_string()),
            );
            let from_me = wmi.key.from_me.unwrap_or(false);
            // Keep the media proto + metadata for any downloadable media.
            let media_info = media_meta(msg);
            let media = media_info.as_ref().map(|_| codec::message_to_vec(msg));
            let (m_kind, m_mime, m_name, m_thumb, m_size) = media_info
                .clone()
                .unwrap_or((0, String::new(), String::new(), Vec::new(), 0));
            let (audio_secs, audio_waveform) = audio_meta(msg);
            let reply = quote_of(msg);
            let (reply_text, reply_sender_name) = reply
                .as_ref()
                .map(|(sender, text)| (text.clone(), reply_name(sender)))
                .unwrap_or_default();
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
                audio: m_kind == 3,
                audio_secs,
                audio_waveform,
                reply_text,
                reply_sender_name,
                media_kind: m_kind,
                media_mime: m_mime,
                media_name: m_name,
                media_thumb: m_thumb,
                media_size: m_size,
            };
            Some((row, media, reply))
        })
        .collect()
}

/// Media kind + metadata for a downloadable media message, as
/// `(kind, mime, document name, jpeg thumbnail)`. Kinds: 1 image, 2 video,
/// 3 audio, 4 document, 5 sticker. `None` for a non-media (text) message.
fn media_meta(msg: &wa::Message) -> Option<(i32, String, String, Vec<u8>, i64)> {
    if let Some(m) = &msg.image_message {
        return Some((
            1,
            m.mimetype.clone().unwrap_or_default(),
            String::new(),
            m.jpeg_thumbnail.clone().unwrap_or_default(),
            0,
        ));
    }
    if let Some(m) = &msg.video_message {
        return Some((
            2,
            m.mimetype.clone().unwrap_or_default(),
            String::new(),
            m.jpeg_thumbnail.clone().unwrap_or_default(),
            0,
        ));
    }
    if let Some(m) = &msg.audio_message {
        return Some((3, m.mimetype.clone().unwrap_or_default(), String::new(), Vec::new(), 0));
    }
    if let Some(m) = &msg.document_message {
        let name = m
            .file_name
            .clone()
            .or_else(|| m.title.clone())
            .unwrap_or_default();
        return Some((
            4,
            m.mimetype.clone().unwrap_or_default(),
            name,
            m.jpeg_thumbnail.clone().unwrap_or_default(),
            m.file_length.unwrap_or(0) as i64,
        ));
    }
    if let Some(m) = &msg.sticker_message {
        return Some((5, m.mimetype.clone().unwrap_or_default(), String::new(), Vec::new(), 0));
    }
    None
}

/// Builds the `ContextInfo` that carries a reply quote on an outgoing message.
fn reply_context(q: &crate::backend::ReplyQuote) -> wa::ContextInfo {
    wa::ContextInfo {
        stanza_id: Some(q.id.clone()),
        participant: (!q.sender.is_empty()).then(|| q.sender.clone()),
        quoted_message: Some(Box::new(wa::Message {
            conversation: Some(q.body.clone()),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// A file extension for a document we're sending, from its name then MIME.
fn doc_ext_from(name: &str, mime: &str) -> String {
    if let Some((_, ext)) = name.rsplit_once('.') {
        if !ext.is_empty() && ext.len() <= 5 {
            return ext.to_ascii_lowercase();
        }
    }
    ext_from_mime(Some(mime), "bin")
}

/// A lowercase file extension for a MIME type (subtype after `/`, params
/// stripped), falling back to `default` when unknown.
fn ext_from_mime(mime: Option<&str>, default: &str) -> String {
    mime.and_then(|m| m.split('/').nth(1))
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty() && s.len() <= 5 && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| default.to_string())
}

/// File extension for a document: from its file name, else its MIME, else `bin`.
fn doc_ext(m: &wa::message::DocumentMessage) -> String {
    if let Some(name) = m.file_name.as_deref() {
        if let Some((_, ext)) = name.rsplit_once('.') {
            if !ext.is_empty() && ext.len() <= 5 {
                return ext.to_ascii_lowercase();
            }
        }
    }
    ext_from_mime(m.mimetype.as_deref(), "bin")
}

/// Voice-note duration (seconds) + amplitude waveform (0..100) from a message's
/// audio payload; `(0, empty)` for non-audio messages.
fn audio_meta(msg: &wa::Message) -> (u32, Vec<u8>) {
    match &msg.audio_message {
        Some(a) => (a.seconds.unwrap_or(0), a.waveform.clone().unwrap_or_default()),
        None => (0, Vec::new()),
    }
}

/// The quoted message (reply) carried by `msg`, as `(quoted author JID, quoted
/// preview)`, from the first sub-message that has a `ContextInfo` with a quote.
fn quote_of(msg: &wa::Message) -> Option<(String, String)> {
    let ctx = msg
        .extended_text_message
        .as_ref()
        .and_then(|m| m.context_info.as_deref())
        .or_else(|| msg.image_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.video_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.audio_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.document_message.as_ref().and_then(|m| m.context_info.as_deref()))?;
    let quoted = ctx.quoted_message.as_deref()?;
    let preview = preview::message_preview(quoted);
    if preview.is_empty() {
        return None;
    }
    Some((ctx.participant.clone().unwrap_or_default(), preview))
}

/// Best-effort display name for a quoted author JID (number; empty if absent).
/// The proper saved name is resolved at read time via the store JOIN.
fn reply_name(sender: &str) -> String {
    if sender.is_empty() {
        String::new()
    } else {
        preview::pretty_number(sender)
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
