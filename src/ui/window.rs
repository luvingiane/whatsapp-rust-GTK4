//! The main application window. Its content is a `gtk::Stack` with two pages:
//! `login` (QR / connection status) and `main` (the chat split view). The app
//! switches between them as the backend connects or logs out.

use adw::prelude::*;
use libadwaita as adw;

use super::chat_list::ChatList;
use super::login::LoginView;

#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub login: LoginView,
    pub chat_list: ChatList,
    stack: gtk::Stack,
    content_page: adw::NavigationPage,
    content_status: adw::StatusPage,
    sidebar_title: adw::WindowTitle,
}

impl MainWindow {
    pub fn new(app: &adw::Application) -> Self {
        // --- login page --------------------------------------------------------
        let login = LoginView::new();
        let login_tb = adw::ToolbarView::new();
        login_tb.add_top_bar(&adw::HeaderBar::new());
        login_tb.set_content(Some(&login.root));

        // --- sidebar (chat list) ----------------------------------------------
        let chat_list = ChatList::new();
        let sidebar_tb = adw::ToolbarView::new();
        let sidebar_header = adw::HeaderBar::new();
        let sidebar_title = adw::WindowTitle::new("Chat", "");
        sidebar_header.set_title_widget(Some(&sidebar_title));
        sidebar_tb.add_top_bar(&sidebar_header);
        sidebar_tb.set_content(Some(&chat_list.root));
        let sidebar_page = adw::NavigationPage::new(&sidebar_tb, "Chat");

        // --- content (placeholder until Step 3) -------------------------------
        let content_status = adw::StatusPage::builder()
            .icon_name("dialog-information-symbolic")
            .title("Seleziona una chat")
            .description("La conversazione completa arriverà nel prossimo modulo.")
            .build();
        let content_tb = adw::ToolbarView::new();
        content_tb.add_top_bar(&adw::HeaderBar::new());
        content_tb.set_content(Some(&content_status));
        let content_page = adw::NavigationPage::new(&content_tb, "WhatsApp");

        let split = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_page)
            .content(&content_page)
            .build();

        // --- stack -------------------------------------------------------------
        let stack = gtk::Stack::new();
        stack.add_named(&login_tb, Some("login"));
        stack.add_named(&split, Some("main"));
        stack.set_visible_child_name("login");

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("WhatsApp")
            .default_width(900)
            .default_height(650)
            .width_request(360)
            .height_request(420)
            .content(&stack)
            .build();

        // Selecting a chat updates the placeholder (real thread view is Step 3).
        {
            let content_page = content_page.clone();
            let content_status = content_status.clone();
            chat_list.connect_open(move |jid, name| {
                content_page.set_title(&name);
                content_status.set_icon_name(Some("user-available-symbolic"));
                content_status.set_title(&name);
                content_status.set_description(Some(&format!(
                    "{jid}\nLa conversazione arriverà nello Step 3."
                )));
            });
        }

        Self {
            window,
            login,
            chat_list,
            stack,
            content_page,
            content_status,
            sidebar_title,
        }
    }

    /// Shows our own account number as the sidebar subtitle (empty if unknown).
    pub fn set_account(&self, number: Option<&str>) {
        self.sidebar_title.set_subtitle(number.unwrap_or(""));
    }

    /// Show the QR / connection page.
    pub fn show_login(&self) {
        self.stack.set_visible_child_name("login");
    }

    /// Show the chat split view.
    pub fn show_main(&self) {
        self.stack.set_visible_child_name("main");
    }

    /// Reset the content placeholder (e.g. on logout).
    pub fn reset_content(&self) {
        self.content_page.set_title("WhatsApp");
        self.content_status
            .set_icon_name(Some("dialog-information-symbolic"));
        self.content_status.set_title("Seleziona una chat");
        self.content_status.set_description(Some(
            "La conversazione completa arriverà nel prossimo modulo.",
        ));
    }
}
