//! Owns the dedicated Tokio runtime thread. The GTK main loop keeps running on
//! the main thread; all whatsapp-rust async work happens here, off the UI.

use async_channel::{Receiver, Sender};
use log::error;

use super::bridge::{WaCommand, WaEvent};

/// Spawns a background OS thread hosting a multi-threaded Tokio runtime and runs
/// the WhatsApp client on it. Returns immediately; results flow back over
/// `event_tx`. The thread lives for the duration of the process.
pub fn spawn(
    db_path: String,
    app_db_path: String,
    event_tx: Sender<WaEvent>,
    command_rx: Receiver<WaCommand>,
) {
    std::thread::Builder::new()
        .name("wa-tokio".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!("failed to build Tokio runtime: {e}");
                    let _ = event_tx
                        .send_blocking(WaEvent::Error(format!("Runtime non avviabile: {e}")));
                    return;
                }
            };

            rt.block_on(async move {
                if let Err(e) =
                    super::client::run(db_path, app_db_path, event_tx.clone(), command_rx).await
                {
                    error!("WhatsApp backend exited with error: {e:?}");
                    let _ = event_tx
                        .send(WaEvent::Error(format!("Errore del backend: {e}")))
                        .await;
                }
            });
        })
        .expect("failed to spawn wa-tokio thread");
}
