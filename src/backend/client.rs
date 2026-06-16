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
    let backend = Arc::new(SqliteStore::new(&db_path).await?);

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
    let mut bot = Bot::builder()
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

    // `run` starts the connection/handshake and returns a future that resolves
    // when the run loop finishes. whatsapp-rust handles reconnection internally.
    let handle = bot.run().await?;
    info!("WhatsApp backend run loop started");

    // Run the loop concurrently with a small command listener so the UI can ask
    // for a clean stop when its window closes.
    tokio::select! {
        res = handle => {
            if let Err(e) = res {
                warn!("bot run loop ended: {e:?}");
            }
        }
        cmd = command_rx.recv() => {
            match cmd {
                Ok(WaCommand::Shutdown) => info!("shutdown requested by UI"),
                Err(_) => info!("command channel closed; stopping backend"),
            }
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
            let jid = client.get_pn().await.map(|j| j.to_string());
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

                let rows: Vec<ChatUpsert> =
                    hs.conversations.iter().filter_map(conv_to_upsert).collect();
                if !rows.is_empty() {
                    info!("history sync: upserting {} chats", rows.len());
                    if let Err(e) = store.upsert_chats(rows).await {
                        warn!("upsert_chats failed: {e:?}");
                    }
                }
                dirty.notify_one();
            }
        }
        Event::Message(msg, info) => {
            let chat = info.source.chat.to_string();
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

                let text = preview::message_preview(msg);
                let ts = info.timestamp.timestamp();
                if let Err(e) = store
                    .apply_message(chat, text, ts, info.source.is_from_me)
                    .await
                {
                    warn!("apply_message failed: {e:?}");
                }
                dirty.notify_one();
            }
        }
        _ => {}
    }
}

/// Whether a JID is a normal user/group chat we show in the list (excludes
/// status broadcast, newsletters, LID-only, etc.).
fn is_user_chat(jid: &str) -> bool {
    jid.ends_with("@s.whatsapp.net") || jid.ends_with("@g.us")
}

/// Maps a history-sync conversation to an owned upsert row, or `None` if it is
/// not a chat we display.
fn conv_to_upsert(c: &wa::Conversation) -> Option<ChatUpsert> {
    let jid = c.id.clone();
    if !is_user_chat(&jid) {
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
        jid,
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
