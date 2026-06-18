//! Construction and lifecycle of the whatsapp-rust [`Bot`], plus translation of
//! its [`Event`]s into our [`WaEvent`]s and into the application [`Store`].
//! Everything here runs on the Tokio runtime thread spawned by [`super::runtime`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_channel::{Receiver, Sender};
use log::{error, info, warn};
use tokio::sync::Notify;
use wacore::store::DevicePropsOverride;
use wacore::types::events::Event;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
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
            match store.list_chats().await {
                Ok(chats) => {
                    let _ = tx.send(WaEvent::ChatsSnapshot(chats)).await;
                }
                Err(e) => warn!("initial list_chats failed: {e:?}"),
            }
            loop {
                dirty.notified().await;
                tokio::time::sleep(SNAPSHOT_DEBOUNCE).await;
                match store.list_chats().await {
                    Ok(chats) => {
                        let _ = tx.send(WaEvent::ChatsSnapshot(chats)).await;
                    }
                    Err(e) => warn!("list_chats failed: {e:?}"),
                }
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

    // One-shot LID↔PN reconcile: app-state events arrive with `@lid` keys while
    // chats are keyed by phone number, so saved names / archive landed on keys
    // that don't match. Re-key every chat_meta entry under all its JID forms
    // (resolved from whatsapp.db — no re-pair needed). Runs in the background.
    {
        let client = handle.client();
        let store = store.clone();
        let dirty = dirty.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let rows = match store.all_chat_meta().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("reconcile: all_chat_meta failed: {e:?}");
                    return;
                }
            };
            let mut unified = 0u32;
            for (jid_str, archived, _pinned, _muted, saved) in rows {
                let Ok(jid) = jid_str.parse::<whatsapp_rust::Jid>() else {
                    continue;
                };
                let forms = jid_forms(&client, &jid).await;
                if forms.len() < 2 {
                    continue; // no alternate form to unify under
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
            if unified > 0 {
                info!("LID<->PN reconcile: unified {unified} chat_meta entries");
                dirty.notify_one();
            }
        });
    }

    // Serve UI commands until the window closes (Shutdown) or the run loop ends.
    loop {
        tokio::select! {
            _ = &mut handle => {
                warn!("bot run loop ended");
                break;
            }
            cmd = command_rx.recv() => match cmd {
                Ok(WaCommand::OpenChat(jid)) => {
                    match store.load_messages(jid.clone(), 200).await {
                        Ok(messages) => {
                            let _ = event_tx.send(WaEvent::ChatHistory { jid, messages }).await;
                        }
                        Err(e) => warn!("load_messages failed: {e:?}"),
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
                let mut msgs: Vec<MessageRow> = Vec::new();
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
                    info!("history sync: storing {} messages", msgs.len());
                    if let Err(e) = store.insert_messages(msgs).await {
                        warn!("insert_messages failed: {e:?}");
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
                    if let Err(e) = store
                        .apply_message(chat.clone(), body.clone(), ts, from_me)
                        .await
                    {
                        warn!("apply_message failed: {e:?}");
                    }
                    let row = MessageRow {
                        id: info.id.clone(),
                        chat_jid: chat,
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
                        body,
                    };
                    if let Err(e) = store.insert_message(row.clone()).await {
                        warn!("insert_message failed: {e:?}");
                    }
                    let _ = tx.send(WaEvent::NewMessage(row)).await;
                    dirty.notify_one();
                }
            }
        }
        // App-state full-sync (and live) updates: archive/pin/mute + the saved
        // address-book name. JIDs may be LID-form, so normalize to the chat key.
        Event::ArchiveUpdate(a) => {
            let archived = a.action.archived.unwrap_or(false);
            for jid in jid_forms(&client, &a.jid).await {
                if let Err(e) = store.set_archived(jid, archived).await {
                    warn!("set_archived failed: {e:?}");
                }
            }
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
        _ => {}
    }
}

/// All known JID forms for a peer: the raw event JID plus its phone-number and
/// LID forms (resolved via whatsapp-rust's LID↔PN map, which reads from
/// whatsapp.db so it works on reconnect without re-pairing). App-state events
/// arrive keyed by `@lid` while our chats are keyed `@s.whatsapp.net`; writing
/// metadata under every form lets both 1:1 chats (PN) and group senders (LID)
/// resolve it. Group/PN JIDs with no mapping simply return just themselves.
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

    let (last_message, msg_ts, last_from_me) = latest_message(c);
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
        unread: c.unread_count.unwrap_or(0),
        is_group,
        archived: c.archived.unwrap_or(false),
        pinned: c.pinned.unwrap_or(0) > 0,
        muted_until: c.mute_end_time.unwrap_or(0) as i64,
    })
}

/// Extracts all displayable messages of a conversation into owned [`MessageRow`]s
/// for the message store, keyed by the given (canonicalized) `chat` JID.
/// System/protocol/empty messages are skipped.
fn conv_to_messages(c: &wa::Conversation, chat: &str) -> Vec<MessageRow> {
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
            Some(MessageRow {
                id,
                chat_jid: chat.to_string(),
                sender_jid,
                // Resolved at read time (store::load_messages JOIN).
                sender_name: String::new(),
                from_me: wmi.key.from_me.unwrap_or(false),
                ts: wmi.message_timestamp.unwrap_or(0) as i64,
                body,
            })
        })
        .collect()
}

/// Finds the most recent message **with displayable content** and returns its
/// preview text, timestamp and direction. System/protocol/empty messages are
/// skipped: counting them made stale chats show a recent date with no preview
/// (e.g. a security-code-change notification on a years-old conversation).
fn latest_message(c: &wa::Conversation) -> (String, Option<u64>, bool) {
    let mut best: Option<(u64, String, bool)> = None;
    for hm in &c.messages {
        let Some(wmi) = &hm.message else { continue };
        let Some(msg) = &wmi.message else { continue };
        let preview = preview::message_preview(msg);
        if preview.is_empty() {
            continue;
        }
        let ts = wmi.message_timestamp.unwrap_or(0);
        if best.as_ref().map_or(true, |(best_ts, _, _)| *best_ts <= ts) {
            best = Some((ts, preview, wmi.key.from_me.unwrap_or(false)));
        }
    }
    match best {
        Some((ts, preview, from_me)) => (preview, Some(ts), from_me),
        None => (String::new(), None, false),
    }
}
