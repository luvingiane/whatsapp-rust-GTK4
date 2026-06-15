//! The main application window: a libadwaita `ApplicationWindow` with a header
//! bar and the [`LoginView`] as its content.

use libadwaita as adw;

use super::login::LoginView;

pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub login: LoginView,
}

impl MainWindow {
    pub fn new(app: &adw::Application) -> Self {
        let login = LoginView::new();

        let header = adw::HeaderBar::new();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&login.root));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("WhatsApp")
            .default_width(420)
            .default_height(580)
            .width_request(360)
            .height_request(420)
            .content(&toolbar)
            .build();

        Self { window, login }
    }
}
