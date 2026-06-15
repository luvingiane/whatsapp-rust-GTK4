//! Wires everything together: creates the libadwaita application, spawns the
//! Tokio backend, and bridges backend events to the UI on the GTK main loop.

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
    let win = MainWindow::new(app);

    // Resolve the session DB path up front so we can fail loudly in the UI.
    let db_path = match config::session_db_path() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            win.login
                .show_error(&format!("Impossibile creare il database: {e}"));
            win.window.present();
            return;
        }
    };

    // Create the bridge channels and start the backend on its own Tokio thread.
    let chans = backend::channels();
    backend::runtime::spawn(db_path, chans.event_tx.clone(), chans.command_rx.clone());

    // Drain backend events on the GTK main loop. `spawn_future_local` guarantees
    // this future runs on the main thread, so it is safe to touch widgets here.
    let login = win.login.clone();
    let event_rx = chans.event_rx.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = event_rx.recv().await {
            match ev {
                WaEvent::QrCode(code) => match qr::qr_texture(&code, 6, 4) {
                    Ok(tex) => login.show_qr(&tex),
                    Err(e) => login.show_error(&format!("QR non valido: {e}")),
                },
                WaEvent::PairSuccess { jid } => login.show_connected(Some(&jid)),
                WaEvent::Connected { jid } => login.show_connected(jid.as_deref()),
                WaEvent::Disconnected => login.show_connecting(),
                WaEvent::LoggedOut => login.show_waiting(),
                WaEvent::Error(msg) => login.show_error(&msg),
            }
        }
    });

    // Ask the backend to stop cleanly when the window is closed.
    let command_tx = chans.command_tx.clone();
    win.window.connect_close_request(move |_| {
        let _ = command_tx.try_send(WaCommand::Shutdown);
        glib::Propagation::Proceed
    });

    win.window.present();
}
