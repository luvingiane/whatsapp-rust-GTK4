//! The main application window. Its content is a top-level `gtk::Stack` with two
//! pages: `login` (QR / connection status) and `main` (the chat split view). The
//! content side of the split view is itself a stack: an empty placeholder until a
//! chat is selected, then the conversation [`ThreadView`].

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use libadwaita as adw;

use super::chat_list::ChatList;
use super::login::LoginView;
use super::thread::ThreadView;
use crate::model::MessageRow;

type OpenProfileCb = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;

#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub login: LoginView,
    pub chat_list: ChatList,
    /// The archived-chats list, shown on a pushed sub-page of the sidebar.
    pub archived_list: ChatList,
    pub thread: ThreadView,
    stack: gtk::Stack,
    content_stack: gtk::Stack,
    sidebar_title: adw::WindowTitle,
    /// Sidebar navigation stack: the active list, with the archived list pushed
    /// on top when the "Archiviate" entry is clicked.
    sidebar_nav: adw::NavigationView,
    /// "N chat archiviate" header, shown only inside the archived sub-page.
    archived_count: gtk::Label,
    /// Clickable conversation header (avatar + name) that opens the profile panel.
    header_name: gtk::Label,
    header_avatar: adw::Avatar,
    /// Status/description line under the header name.
    header_subtitle: gtk::Label,
    /// JID of the open chat (target of the profile panel).
    profile_jid: Rc<RefCell<String>>,
    on_open_profile: OpenProfileCb,
    /// Shared avatar texture cache (read by the profile panel for participants).
    pub avatars: super::AvatarCache,
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

        // Clickable conversation header: avatar + name → opens the profile panel.
        let header_avatar = adw::Avatar::new(28, None, true);
        let header_name = gtk::Label::new(Some("WhatsApp"));
        header_name.add_css_class("heading");
        header_name.set_xalign(0.0);
        let header_subtitle = gtk::Label::new(None);
        header_subtitle.add_css_class("caption");
        header_subtitle.add_css_class("dim-label");
        header_subtitle.set_xalign(0.0);
        header_subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header_subtitle.set_visible(false);
        let header_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        header_text.set_valign(gtk::Align::Center);
        header_text.append(&header_name);
        header_text.append(&header_subtitle);
        let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header_box.append(&header_avatar);
        header_box.append(&header_text);
        let header_btn = gtk::Button::builder().child(&header_box).build();
        header_btn.add_css_class("flat");
        let profile_jid: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let on_open_profile: OpenProfileCb = Rc::new(RefCell::new(None));
        {
            let profile_jid = profile_jid.clone();
            let on_open_profile = on_open_profile.clone();
            header_btn.connect_clicked(move |_| {
                let jid = profile_jid.borrow().clone();
                if jid.is_empty() {
                    return;
                }
                if let Some(cb) = on_open_profile.borrow().as_ref() {
                    cb(jid);
                }
            });
        }
        let content_header = adw::HeaderBar::new();
        // Left-align the chat/group name: an empty centered title widget suppresses
        // the page title, and the clickable name button is packed at the start.
        // The right side is intentionally left empty for now.
        content_header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
        content_header.pack_start(&header_btn);
        // With a plain Paned (no NavigationSplitView coordinating the two header
        // bars) each HeaderBar would draw its own window controls, duplicating the
        // traffic-light buttons in the middle of the window. Keep them on the
        // sidebar header only.
        content_header.set_show_start_title_buttons(false);

        let content_tb = adw::ToolbarView::new();
        content_tb.add_top_bar(&content_header);
        content_tb.set_content(Some(&content_stack));

        // Resizable split: a Paned (instead of NavigationSplitView) so the user can
        // drag the divider to widen/narrow the chat list. The width is persisted and
        // restored across restarts. Trade-off: no adaptive collapse on narrow windows.
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&sidebar_nav));
        paned.set_end_child(Some(&content_tb));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        sidebar_nav.set_size_request(220, -1);
        paned.set_position(crate::config::read_sidebar_width().unwrap_or(300));
        {
            // Persist the divider position, debounced so a drag doesn't hammer the disk.
            let pending = Rc::new(std::cell::Cell::new(false));
            let paned_w = paned.clone();
            paned.connect_position_notify(move |_| {
                if pending.replace(true) {
                    return;
                }
                let pending = pending.clone();
                let paned_w = paned_w.clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(400),
                    move || {
                        crate::config::write_sidebar_width(paned_w.position());
                        pending.set(false);
                    },
                );
            });
        }

        // --- top-level stack ---------------------------------------------------
        let stack = gtk::Stack::new();
        stack.add_named(&login_tb, Some("login"));
        stack.add_named(&paned, Some("main"));
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
            content_stack,
            sidebar_title,
            sidebar_nav,
            archived_count,
            header_name,
            header_avatar,
            header_subtitle,
            profile_jid,
            on_open_profile,
            avatars,
        }
    }

    /// Sets the status/description line under the conversation header name
    /// (hidden when empty).
    pub fn set_header_subtitle(&self, text: &str) {
        self.header_subtitle.set_text(text);
        self.header_subtitle.set_visible(!text.is_empty());
    }

    /// Renders the online-presence line under the header name. 1:1: "online" when
    /// the contact is online (empty otherwise). Group: "{k} online · names…" with
    /// as many member names as fit the header width, then "e altri N partecipanti"
    /// or "(total)" when the list is complete. (Typing status will replace this.)
    pub fn set_presence(&self, is_group: bool, online_names: &[String], total: usize) {
        let text = if !is_group {
            if online_names.is_empty() {
                String::new()
            } else {
                "online".to_string()
            }
        } else {
            render_group_presence(&self.header_subtitle, online_names, total)
        };
        self.set_header_subtitle(&text);
    }

    /// Registers the callback invoked (with the open chat's JID) when the
    /// conversation header is clicked.
    pub fn connect_open_profile<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_open_profile.borrow_mut() = Some(Box::new(f));
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
        self.header_name.set_label(name);
        self.set_header_subtitle("");
        self.header_avatar.set_text(Some(name));
        self.header_avatar
            .set_custom_image(self.thread.avatar_texture(jid).as_ref());
        *self.profile_jid.borrow_mut() = jid.to_string();
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
        self.header_name.set_label("WhatsApp");
        self.set_header_subtitle("");
        self.thread.clear();
        self.content_stack.set_visible_child_name("empty");
    }
}

/// Builds the group-presence line, fitting as many online member names as the
/// header width allows. Uses the label's Pango layout to estimate the average
/// character width, falling back to a fixed budget before the label is allocated.
fn render_group_presence(label: &gtk::Label, names: &[String], total: usize) -> String {
    let k = names.len();
    if k == 0 {
        return if total > 0 {
            format!("{total} partecipanti")
        } else {
            String::new()
        };
    }
    // Approximate how many characters fit on the single header line.
    let avail = label.width().max(0) as usize;
    let char_w = {
        let layout = label.create_pango_layout(Some("MMMMMMMMMM"));
        (layout.pixel_size().0.max(1) as usize / 10).max(6)
    };
    let budget = if avail > 0 { avail / char_w } else { 40 };
    let prefix_len = format!("{k} online · ").chars().count();
    let mut used = prefix_len;
    let mut shown: Vec<&str> = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let add = n.chars().count() + if i > 0 { 2 } else { 0 };
        // Reserve room for the trailing "(total)" / "e altri N…" suffix.
        if !shown.is_empty() && used + add + 18 > budget {
            break;
        }
        used += add;
        shown.push(n);
    }
    let joined = shown.join(", ");
    if shown.len() == k {
        format!("{k} online · {joined} ({total})")
    } else {
        format!(
            "{k} online · {joined} e altri {} partecipanti",
            total - shown.len()
        )
    }
}
