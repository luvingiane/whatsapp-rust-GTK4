//! Construction and lifecycle of the whatsapp-rust [`Bot`], plus translation of
//! its [`Event`]s into our [`WaEvent`]s. Everything here runs on the Tokio
//! runtime thread spawned by [`super::runtime`].

use std::sync::Arc;

use anyhow::Result;
use async_channel::{Receiver, Sender};
use log::{error, info, warn};
use wacore::types::events::Event;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
use whatsapp_rust::TokioRuntime;
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use super::bridge::{WaCommand, WaEvent};

/// Builds the bot against the SQLite session DB and drives its run loop until it
/// ends or a [`WaCommand::Shutdown`] arrives.
///
/// `db_path` is the absolute path returned by [`crate::config::session_db_path`].
/// Errors are propagated to the caller (which surfaces them to the UI); routine
/// protocol hiccups (decrypt failures, disconnects) are logged, never panic.
pub async fn run(
    db_path: String,
    event_tx: Sender<WaEvent>,
    command_rx: Receiver<WaCommand>,
) -> Result<()> {
    info!("opening session database at {db_path}");
    // SqliteStore enables WAL journaling and runs migrations on open, so session
    // and keys are persisted atomically across restarts.
    let backend = Arc::new(SqliteStore::new(&db_path).await?);

    let ev_tx = event_tx.clone();
    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .on_event(move |event, client| {
            let tx = ev_tx.clone();
            async move {
                handle_event(event, client, tx).await;
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

/// Translates a single whatsapp-rust [`Event`] into a [`WaEvent`] for the UI.
/// Unhandled variants are intentionally ignored for this step (chat, receipts,
/// presence, etc. arrive in later modules).
async fn handle_event(event: Arc<Event>, client: Arc<Client>, tx: Sender<WaEvent>) {
    match &*event {
        Event::PairingQrCode { code, timeout } => {
            info!("QR code received (valid ~{}s)", timeout.as_secs());
            let _ = tx.send(WaEvent::QrCode(code.clone())).await;
        }
        Event::PairSuccess(p) => {
            info!("pairing success as {}", p.id);
            let _ = tx
                .send(WaEvent::PairSuccess {
                    jid: pretty_number(&p.id.to_string()),
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
                    jid: jid.as_deref().map(pretty_number),
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
        _ => {}
    }
}

/// Turns a raw JID like `393284448052:6@s.whatsapp.net` into a friendlier
/// `+393284448052` by dropping the device suffix and server, for display only.
fn pretty_number(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    let user = user.split(':').next().unwrap_or(user);
    format!("+{user}")
}
