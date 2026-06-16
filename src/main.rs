//! Native GTK4 + libadwaita WhatsApp client.
//!
//! Step 1 scope: bring up the app, show the QR pairing code, complete pairing and
//! persist the session to SQLite so restarts reconnect without re-scanning.

mod app;
mod backend;
mod config;
mod model;
mod store;
mod ui;
mod util;

fn main() -> gtk::glib::ExitCode {
    // whatsapp-rust logs through the `log` facade; env_logger surfaces it.
    // Override verbosity at runtime with e.g. `RUST_LOG=debug`.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!(
        "starting {} v{}",
        config::APP_NAME,
        env!("CARGO_PKG_VERSION")
    );

    app::run()
}
