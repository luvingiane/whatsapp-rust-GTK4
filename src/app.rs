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

    // Drain backend events on the GTK main loop. `spawn_future_local` guarantees
    // this future runs on the main thread, so it is safe to touch widgets here.
    let win_ev = win.clone();
    let event_rx = chans.event_rx.clone();
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
                }
                // Transient drop: whatsapp-rust reconnects on its own; stay put.
                WaEvent::Disconnected => {}
                WaEvent::LoggedOut => {
                    win_ev.chat_list.update(&[]);
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
                    win_ev.chat_list.update(&chats);
                }
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
