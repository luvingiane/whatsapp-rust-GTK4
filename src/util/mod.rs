//! Small, UI-agnostic helpers.

pub mod jid;
pub mod og;
pub mod preview;
pub mod qr;
pub mod text;

/// Opens a web URL in the default browser via the desktop portal.
pub fn open_uri(uri: &str) {
    gtk::UriLauncher::new(uri).launch(
        gtk::Window::NONE,
        gtk::gio::Cancellable::NONE,
        |res| {
            if let Err(e) = res {
                log::warn!("open uri failed: {e}");
            }
        },
    );
}

/// Opens a local file with the user's default application for its content type.
/// Goes through the portal/GIO (content-sniffing), unlike `xdg-open`, which can
/// pick the wrong app when the extension is missing or the desktop env differs.
pub fn open_file(path: &str, parent: Option<&gtk::Window>) {
    let file = gtk::gio::File::for_path(path);
    gtk::FileLauncher::new(Some(&file)).launch(parent, gtk::gio::Cancellable::NONE, |res| {
        if let Err(e) = res {
            log::warn!("open file failed: {e}");
        }
    });
}
