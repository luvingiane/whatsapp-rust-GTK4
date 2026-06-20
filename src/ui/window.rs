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
    /// The archived-chats list, shown on a pushed sub-page of the sidebar.
    pub archived_list: ChatList,
    pub thread: ThreadView,
    stack: gtk::Stack,
    content_page: adw::NavigationPage,
    content_stack: gtk::Stack,
    sidebar_title: adw::WindowTitle,
    /// Sidebar navigation stack: the active list, with the archived list pushed
    /// on top when the "Archiviate" entry is clicked.
    sidebar_nav: adw::NavigationView,
    /// "N chat archiviate" header, shown only inside the archived sub-page.
    archived_count: gtk::Label,
}

impl MainWindow {
    pub fn new(app: &adw::Application) -> Self {
        // --- login page --------------------------------------------------------
        let login = LoginView::new();
        let login_tb = adw::ToolbarView::new();
        login_tb.add_top_bar(&adw::HeaderBar::new());
        login_tb.set_content(Some(&login.root));

        // Shared profile-picture cache, used by the sidebar and the thread.
        let avatars = super::new_avatar_cache();

        // --- sidebar: active list (root) + pushable archived sub-page ----------
        let chat_list = ChatList::new(&avatars, true);
        let sidebar_tb = adw::ToolbarView::new();
        let sidebar_header = adw::HeaderBar::new();
        let sidebar_title = adw::WindowTitle::new("Chat", "");
        sidebar_header.set_title_widget(Some(&sidebar_title));
        sidebar_tb.add_top_bar(&sidebar_header);
        sidebar_tb.set_content(Some(&chat_list.root));
        let chats_page = adw::NavigationPage::new(&sidebar_tb, "Chat");
        chats_page.set_tag(Some("chats"));

        // Archived sub-page: its own header (back button auto-provided by the
        // NavigationView), a "N chat archiviate" count line, then the list.
        let archived_list = ChatList::new(&avatars, false);
        let archived_header = adw::HeaderBar::new();
        archived_header.set_title_widget(Some(&adw::WindowTitle::new("Archiviate", "")));
        let archived_count = gtk::Label::builder()
            .xalign(0.5)
            .margin_top(8)
            .margin_bottom(4)
            .build();
        archived_count.add_css_class("dim-label");
        archived_count.add_css_class("caption");
        let archived_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        archived_box.append(&archived_count);
        archived_box.append(&archived_list.root);
        let archived_tb = adw::ToolbarView::new();
        archived_tb.add_top_bar(&archived_header);
        archived_tb.set_content(Some(&archived_box));
        let archived_page = adw::NavigationPage::new(&archived_tb, "Archiviate");
        archived_page.set_tag(Some("archived"));

        // The first added page is the visible root; the archived page stays
        // available for `push_by_tag` but is not shown until requested.
        let sidebar_nav = adw::NavigationView::new();
        sidebar_nav.add(&chats_page);
        sidebar_nav.add(&archived_page);
        let sidebar_page = adw::NavigationPage::new(&sidebar_nav, "Chat");

        // --- content: empty placeholder + conversation thread -----------------
        let empty = adw::StatusPage::builder()
            .icon_name("dialog-information-symbolic")
            .title("Seleziona una chat")
            .description("Scegli una conversazione dalla lista.")
            .build();
        let thread = ThreadView::new(&avatars);
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
            archived_list,
            thread,
            stack,
            content_page,
            content_stack,
            sidebar_title,
            sidebar_nav,
            archived_count,
        }
    }

    /// Shows our own account number as the sidebar subtitle (empty if unknown).
    pub fn set_account(&self, number: Option<&str>) {
        self.sidebar_title.set_subtitle(number.unwrap_or(""));
    }

    /// Refreshes the archived list, the "Archiviate" entry (visible while any
    /// chat is archived; its badge shows the unread count), and the in-page
    /// "N chat archiviate" header (the total).
    pub fn update_archived(&self, chats: &[crate::model::ChatSummary]) {
        let total = chats.len();
        let unread = chats.iter().filter(|c| c.unread > 0).count();
        self.archived_list.update(chats);
        self.chat_list.set_archived_count(total, unread);
        self.archived_count
            .set_label(&format!("{total} chat archiviate"));
    }

    /// Navigates the sidebar to the archived sub-page (no-op if already there).
    pub fn open_archived(&self) {
        let on_archived = self
            .sidebar_nav
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == "archived");
        if !on_archived {
            self.sidebar_nav.push_by_tag("archived");
        }
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
        // Focus the composer after the page switch so typing works immediately.
        let thread = self.thread.clone();
        gtk::glib::idle_add_local_once(move || thread.focus_composer());
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
