//! The right side pane hosting the profile panel and the media gallery — one
//! pane inside the main window instead of secondary windows. A mini header
//! (back / title / close) sits above a two-page stack: "profile" and "media";
//! the back button returns from media to profile.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use libadwaita as adw;

type CloseCb = Rc<RefCell<Option<Box<dyn Fn()>>>>;

#[derive(Clone)]
pub struct SidePane {
    pub root: gtk::Box,
    title: gtk::Label,
    back: gtk::Button,
    stack: gtk::Stack,
    profile_slot: adw::Bin,
    media_slot: adw::Bin,
    on_close: CloseCb,
}

impl SidePane {
    pub fn new() -> Self {
        // Mini header: a plain Box (not adw::HeaderBar — that would grow a second
        // set of window controls next to the content header's).
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.add_css_class("flat");
        back.set_visible(false);

        let title = gtk::Label::new(Some("Profilo"));
        title.add_css_class("heading");
        title.set_hexpand(true);
        title.set_xalign(0.0);

        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        header.append(&back);
        header.append(&title);
        header.append(&close);

        let profile_slot = adw::Bin::new();
        let media_slot = adw::Bin::new();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::SlideLeftRight)
            .vexpand(true)
            .build();
        stack.add_named(&profile_slot, Some("profile"));
        stack.add_named(&media_slot, Some("media"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&stack);

        let on_close: CloseCb = Rc::new(RefCell::new(None));
        {
            let on_close = on_close.clone();
            close.connect_clicked(move |_| {
                if let Some(f) = on_close.borrow().as_ref() {
                    f();
                }
            });
        }
        {
            let stack = stack.clone();
            let back_w = back.clone();
            let title = title.clone();
            back.connect_clicked(move |_| {
                stack.set_visible_child_name("profile");
                back_w.set_visible(false);
                title.set_label("Profilo");
            });
        }

        Self {
            root,
            title,
            back,
            stack,
            profile_slot,
            media_slot,
            on_close,
        }
    }

    /// Fills the profile page and shows it (media page is left as-is).
    pub fn show_profile(&self, w: &gtk::Widget, title: &str) {
        self.profile_slot.set_child(Some(w));
        self.stack.set_visible_child_name("profile");
        self.back.set_visible(false);
        self.title.set_label(title);
    }

    /// Fills the media page and navigates to it (back returns to the profile).
    pub fn show_media(&self, w: &gtk::Widget) {
        self.media_slot.set_child(Some(w));
        self.stack.set_visible_child_name("media");
        self.back.set_visible(true);
        self.title.set_label("Media");
    }

    /// Drops both pages' content (called when the pane is hidden, so stale
    /// widgets — and their textures — don't linger).
    pub fn clear(&self) {
        self.profile_slot.set_child(gtk::Widget::NONE);
        self.media_slot.set_child(gtk::Widget::NONE);
    }

    pub fn connect_close<F: Fn() + 'static>(&self, f: F) {
        *self.on_close.borrow_mut() = Some(Box::new(f));
    }
}
