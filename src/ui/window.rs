//! The main application window. Its content is a top-level `gtk::Stack` with two
//! pages: `login` (QR / connection status) and `main` (the chat split view). The
//! content side of the split view is itself a stack: an empty placeholder until a
//! chat is selected, then the conversation [`ThreadView`].

use adw::prelude::*;
use libadwaita as adw;

use super::chat_list::ChatList;
use super::login::LoginView;
use super::thread::ThreadView;
use crate::model::MessageRow;

#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub login: LoginView,
    pub chat_list: ChatList,
    pub thread: ThreadView,
    stack: gtk::Stack,
    content_page: adw::NavigationPage,
    content_stack: gtk::Stack,
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

        // --- content: empty placeholder + conversation thread -----------------
        let empty = adw::StatusPage::builder()
            .icon_name("dialog-information-symbolic")
            .title("Seleziona una chat")
            .description("Scegli una conversazione dalla lista.")
            .build();
        let thread = ThreadView::new();
        let content_stack = gtk::Stack::new();
        content_stack.add_named(&empty, Some("empty"));
        content_stack.add_named(&thread.root, Some("thread"));
        content_stack.set_visible_child_name("empty");

        let content_tb = adw::ToolbarView::new();
        content_tb.add_top_bar(&adw::HeaderBar::new());
        content_tb.set_content(Some(&content_stack));
        let content_page = adw::NavigationPage::new(&content_tb, "WhatsApp");

        let split = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_page)
            .content(&content_page)
            .build();

        // --- top-level stack ---------------------------------------------------
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

        Self {
            window,
            login,
            chat_list,
            thread,
            stack,
            content_page,
            content_stack,
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

    /// Open a chat in the content pane: set the title and switch to the (empty,
    /// loading) thread view. History arrives later via [`Self::show_history`].
    pub fn open_chat(&self, jid: &str, name: &str) {
        self.content_page.set_title(name);
        self.thread.set_loading(jid.ends_with("@g.us"));
        self.content_stack.set_visible_child_name("thread");
    }

    /// Render the loaded history for the open chat.
    pub fn show_history(&self, messages: &[MessageRow]) {
        self.thread.show_history(messages);
    }

    /// Append a live message to the open thread.
    pub fn append_message(&self, m: &MessageRow) {
        self.thread.append(m);
    }

    /// Reset the content pane to the empty placeholder (e.g. on logout).
    pub fn reset_content(&self) {
        self.content_page.set_title("WhatsApp");
        self.thread.clear();
        self.content_stack.set_visible_child_name("empty");
    }
}
