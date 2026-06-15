//! The login / connection view: a single libadwaita `StatusPage` whose icon,
//! title, description and central content change as the backend progresses
//! through waiting → QR shown → connecting → connected (or error).
//!
//! `LoginView` is `Clone`: every field is a GObject widget, so cloning just bumps
//! reference counts. That lets us hand a clone to the async event loop while the
//! window keeps its own.

use gtk::gdk;
use gtk::prelude::*;
use libadwaita as adw;

#[derive(Clone)]
pub struct LoginView {
    /// Root widget to embed in the window.
    pub root: adw::StatusPage,
    picture: gtk::Picture,
    spinner: gtk::Spinner,
}

impl LoginView {
    pub fn new() -> Self {
        let picture = gtk::Picture::builder()
            .width_request(280)
            .height_request(280)
            .can_shrink(false)
            .visible(false)
            .build();

        let spinner = gtk::Spinner::builder()
            .width_request(48)
            .height_request(48)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .spacing(18)
            .build();
        content.append(&spinner);
        content.append(&picture);

        let root = adw::StatusPage::builder()
            .icon_name("dialog-information-symbolic")
            .title("Avvio…")
            .description("Inizializzazione del client WhatsApp")
            .child(&content)
            .build();

        let view = Self {
            root,
            picture,
            spinner,
        };
        view.show_waiting();
        view
    }

    /// Spinner state: connected to nothing yet, waiting for the first QR code.
    pub fn show_waiting(&self) {
        self.picture.set_visible(false);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.root.set_icon_name(Some("dialog-information-symbolic"));
        self.root.set_title("In attesa del codice QR…");
        self.root
            .set_description(Some("Connessione ai server di WhatsApp"));
    }

    /// Show a freshly generated QR code for the user to scan.
    pub fn show_qr(&self, texture: &gdk::Texture) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.picture.set_paintable(Some(texture));
        self.picture.set_visible(true);
        self.root.set_icon_name(None);
        self.root.set_title("Scansiona per accedere");
        self.root.set_description(Some(
            "Sul telefono: WhatsApp → Dispositivi collegati → Collega un dispositivo",
        ));
    }

    /// Pairing accepted, finishing the connection.
    pub fn show_connecting(&self) {
        self.picture.set_visible(false);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.root.set_icon_name(Some("dialog-information-symbolic"));
        self.root.set_title("Connessione…");
        self.root.set_description(Some("Accesso in corso"));
    }

    /// Fully connected. `jid` is our own number, when known.
    pub fn show_connected(&self, jid: Option<&str>) {
        self.picture.set_visible(false);
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.root.set_icon_name(Some("emblem-ok-symbolic"));
        self.root.set_title("Connesso");
        let desc = match jid {
            Some(j) => format!("Connesso come {j}"),
            None => "Sessione attiva".to_string(),
        };
        self.root.set_description(Some(&desc));
    }

    /// Surface a backend error without crashing.
    pub fn show_error(&self, msg: &str) {
        self.picture.set_visible(false);
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.root.set_icon_name(Some("dialog-error-symbolic"));
        self.root.set_title("Errore");
        self.root.set_description(Some(msg));
    }
}

impl Default for LoginView {
    fn default() -> Self {
        Self::new()
    }
}
