//! Full-window media viewer, Telegram-style: a dark layer over the whole main
//! window with the photo/video centered, chat name + timestamp top-left, an X
//! top-right, and forward/copy/save/delete actions bottom-right. Esc, the X, or
//! a click outside the media close it.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use libadwaita as adw;

/// What the viewer is currently showing (needed by the action buttons).
struct Current {
    chat_jid: String,
    id: String,
    path: String,
    from_me: bool,
}

type ForwardCb = Rc<RefCell<Option<Box<dyn Fn(String, String)>>>>;
type DeleteCb = Rc<RefCell<Option<Box<dyn Fn(String, String, bool)>>>>;
type NavigateCb = Rc<RefCell<Option<Box<dyn Fn(String, String)>>>>;

#[derive(Clone)]
pub struct ViewerOverlay {
    /// The whole layer, added as an overlay child of the window root.
    pub root: gtk::Overlay,
    slot: adw::Bin,
    title: gtk::Label,
    date: gtk::Label,
    copy_btn: gtk::Button,
    prev_btn: gtk::Button,
    next_btn: gtk::Button,
    /// The media widget whose bounds decide "inside" for click-to-close.
    media: Rc<RefCell<Option<gtk::Widget>>>,
    current: Rc<RefCell<Option<Current>>>,
    /// The chat's visual media `(id, kind)`, oldest-first, for left/right
    /// navigation between the media of the same chat.
    siblings: Rc<RefCell<Vec<(String, i32)>>>,
    on_forward: ForwardCb,
    on_delete: DeleteCb,
    on_navigate: NavigateCb,
}

impl ViewerOverlay {
    pub fn new() -> Self {
        // Dim layer: fills everything, click closes (media/buttons sit in
        // separate overlay children so their clicks never reach it).
        let dim = gtk::Box::builder().hexpand(true).vexpand(true).build();
        dim.add_css_class("viewer-dim");

        let root = gtk::Overlay::new();
        root.set_child(Some(&dim));
        root.set_visible(false);

        // Media area: fills the window between the top and bottom bars; the
        // media widget centers inside, and clicks landing outside its bounds
        // close the viewer.
        let slot = adw::Bin::new();
        slot.set_margin_top(52);
        slot.set_margin_bottom(60);
        slot.set_margin_start(16);
        slot.set_margin_end(16);
        root.add_overlay(&slot);

        // Top bar: chat name + media timestamp on the left, X on the right.
        let title = gtk::Label::builder().xalign(0.0).build();
        title.add_css_class("heading");
        title.add_css_class("viewer-text");
        let date = gtk::Label::builder().xalign(0.0).build();
        date.add_css_class("caption");
        date.add_css_class("viewer-text");
        let head_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        head_text.set_hexpand(true);
        head_text.append(&title);
        head_text.append(&date);
        let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
        close_btn.add_css_class("viewer-osd");
        close_btn.add_css_class("circular");
        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .valign(gtk::Align::Start)
            .margin_top(10)
            .margin_start(14)
            .margin_end(14)
            .build();
        top.append(&head_text);
        top.append(&close_btn);
        root.add_overlay(&top);

        // Bottom-right action bar.
        let forward_btn = gtk::Button::with_label("➦");
        forward_btn.set_tooltip_text(Some("Inoltra"));
        let copy_btn = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy_btn.set_tooltip_text(Some("Copia"));
        let save_btn = gtk::Button::from_icon_name("document-save-symbolic");
        save_btn.set_tooltip_text(Some("Scarica"));
        let delete_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        delete_btn.set_tooltip_text(Some("Elimina"));
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_bottom(12)
            .margin_end(14)
            .build();
        for b in [&forward_btn, &copy_btn, &save_btn, &delete_btn] {
            b.add_css_class("viewer-osd");
            b.add_css_class("circular");
            actions.append(b);
        }
        root.add_overlay(&actions);

        // Prev / next arrows on the left/right edges, centered vertically.
        let prev_btn = gtk::Button::from_icon_name("go-previous-symbolic");
        prev_btn.set_halign(gtk::Align::Start);
        prev_btn.set_valign(gtk::Align::Center);
        prev_btn.set_margin_start(10);
        let next_btn = gtk::Button::from_icon_name("go-next-symbolic");
        next_btn.set_halign(gtk::Align::End);
        next_btn.set_valign(gtk::Align::Center);
        next_btn.set_margin_end(10);
        for b in [&prev_btn, &next_btn] {
            b.add_css_class("viewer-osd");
            b.add_css_class("circular");
            b.set_visible(false);
            root.add_overlay(b);
        }

        let media: Rc<RefCell<Option<gtk::Widget>>> = Rc::new(RefCell::new(None));
        let current: Rc<RefCell<Option<Current>>> = Rc::new(RefCell::new(None));
        let siblings: Rc<RefCell<Vec<(String, i32)>>> = Rc::new(RefCell::new(Vec::new()));
        let on_forward: ForwardCb = Rc::new(RefCell::new(None));
        let on_delete: DeleteCb = Rc::new(RefCell::new(None));
        let on_navigate: NavigateCb = Rc::new(RefCell::new(None));

        let this = Self {
            root,
            slot,
            title,
            date,
            copy_btn: copy_btn.clone(),
            prev_btn: prev_btn.clone(),
            next_btn: next_btn.clone(),
            media,
            current,
            siblings,
            on_forward,
            on_delete,
            on_navigate,
        };

        {
            let this2 = this.clone();
            close_btn.connect_clicked(move |_| this2.hide());
        }
        // Click on the dim background closes.
        {
            let this2 = this.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| this2.hide());
            dim.add_controller(click);
        }
        // Click inside the media area but outside the media widget closes too
        // (e.g. beside a portrait photo).
        {
            let this2 = this.clone();
            let slot_w = this.slot.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, x, y| {
                let inside = this2.media.borrow().as_ref().is_some_and(|m| {
                    m.compute_bounds(&slot_w)
                        .is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)))
                });
                if !inside {
                    this2.hide();
                }
            });
            this.slot.add_controller(click);
        }
        {
            let this2 = this.clone();
            forward_btn.connect_clicked(move |_| {
                let (chat_jid, id) = match this2.current.borrow().as_ref() {
                    Some(c) => (c.chat_jid.clone(), c.id.clone()),
                    None => return,
                };
                if let Some(f) = this2.on_forward.borrow().as_ref() {
                    f(chat_jid, id);
                }
            });
        }
        {
            let this2 = this.clone();
            copy_btn.connect_clicked(move |btn| {
                let path = match this2.current.borrow().as_ref() {
                    Some(c) => c.path.clone(),
                    None => return,
                };
                match gtk::gdk::Texture::from_filename(&path) {
                    Ok(tex) => btn.clipboard().set_texture(&tex),
                    Err(e) => log::warn!("copy image failed: {e}"),
                }
            });
        }
        {
            let this2 = this.clone();
            save_btn.connect_clicked(move |btn| {
                let src = match this2.current.borrow().as_ref() {
                    Some(c) => PathBuf::from(&c.path),
                    None => return,
                };
                let parent = btn.root().and_downcast::<gtk::Window>();
                let dialog = gtk::FileDialog::builder()
                    .title("Salva media")
                    .initial_name(src.file_name().and_then(|n| n.to_str()).unwrap_or("media"))
                    .build();
                dialog.save(parent.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(file) = res {
                        if let Some(dest) = file.path() {
                            if let Err(e) = std::fs::copy(&src, &dest) {
                                log::warn!("save media failed: {e}");
                            }
                        }
                    }
                });
            });
        }
        {
            let this2 = this.clone();
            delete_btn.connect_clicked(move |_| {
                let (chat_jid, id, can_revoke) = match this2.current.borrow().as_ref() {
                    Some(c) => (c.chat_jid.clone(), c.id.clone(), c.from_me),
                    None => return,
                };
                if let Some(f) = this2.on_delete.borrow().as_ref() {
                    f(chat_jid, id, can_revoke);
                }
            });
        }
        {
            let this2 = this.clone();
            prev_btn.connect_clicked(move |_| this2.navigate(-1));
        }
        {
            let this2 = this.clone();
            next_btn.connect_clicked(move |_| this2.navigate(1));
        }

        this
    }

    /// Moves to the previous (`-1`) or next (`+1`) media of the same chat, if
    /// one exists. The neighbor is requested through `on_navigate`; the new
    /// content swaps in when its `MediaReady` arrives.
    pub fn navigate(&self, delta: i32) {
        let (chat_jid, id) = match self.current.borrow().as_ref() {
            Some(c) => (c.chat_jid.clone(), c.id.clone()),
            None => return,
        };
        let siblings = self.siblings.borrow();
        let Some(pos) = siblings.iter().position(|(sid, _)| *sid == id) else {
            return;
        };
        let target = pos as i32 + delta;
        if target < 0 || target as usize >= siblings.len() {
            return;
        }
        let next_id = siblings[target as usize].0.clone();
        drop(siblings);
        if let Some(f) = self.on_navigate.borrow().as_ref() {
            f(chat_jid, next_id);
        }
    }

    /// Enables/disables the arrows based on the current position in `siblings`.
    fn update_arrows(&self, id: &str) {
        let siblings = self.siblings.borrow();
        let pos = siblings.iter().position(|(sid, _)| sid == id);
        match pos {
            Some(p) => {
                self.prev_btn.set_visible(p > 0);
                self.next_btn.set_visible(p + 1 < siblings.len());
            }
            None => {
                self.prev_btn.set_visible(false);
                self.next_btn.set_visible(false);
            }
        }
    }

    /// Shows `path` (kind 1 photo / 2 video / 6 gif) over the whole window.
    /// `siblings` is the chat's visual media `(id, kind)` oldest-first, driving
    /// left/right navigation (empty ⇒ no arrows).
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &self,
        chat_name: &str,
        chat_jid: String,
        id: String,
        kind: i32,
        path: String,
        ts: i64,
        from_me: bool,
        siblings: Vec<(String, i32)>,
    ) {
        *self.siblings.borrow_mut() = siblings;
        self.update_arrows(&id);
        self.title.set_label(chat_name);
        let stamp = gtk::glib::DateTime::from_unix_local(ts)
            .ok()
            .and_then(|d| d.format("%d/%m/%Y alle %H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.date.set_label(&stamp);
        self.date.set_visible(ts > 0 && !stamp.is_empty());
        // Copying makes sense for still images only.
        self.copy_btn.set_visible(kind == 1);

        // `bounds` is the widget whose rectangle counts as "inside" for the
        // click-to-close check: the picture itself for photos (so clicking the
        // letterbox beside it closes); the whole video for videos (it fills the
        // slot and letterboxes internally, and its clicks drive the controls).
        let (w, bounds): (gtk::Widget, gtk::Widget) = if kind == 2 || kind == 6 {
            let video = gtk::Video::for_filename(Some(path.as_str()));
            video.set_autoplay(true);
            // GIFs loop like the official clients; videos play once.
            video.set_loop(kind == 6);
            (video.clone().upcast(), video.upcast())
        } else {
            build_image_view(&path)
        };
        self.slot.set_child(Some(&w));
        *self.media.borrow_mut() = Some(bounds);
        *self.current.borrow_mut() = Some(Current {
            chat_jid,
            id,
            path,
            from_me,
        });
        self.root.set_visible(true);
    }

    /// Hides the viewer, dropping the media widget first so a playing
    /// video/GIF actually stops.
    pub fn hide(&self) {
        self.slot.set_child(gtk::Widget::NONE);
        *self.media.borrow_mut() = None;
        *self.current.borrow_mut() = None;
        self.siblings.borrow_mut().clear();
        self.root.set_visible(false);
    }

    pub fn visible(&self) -> bool {
        self.root.is_visible()
    }

    /// Whether the viewer is currently showing message `id` (used to close it
    /// when that message gets deleted).
    pub fn is_showing(&self, id: &str) -> bool {
        self.visible() && self.current.borrow().as_ref().is_some_and(|c| c.id == id)
    }

    /// `(chat_jid, id)` when "Inoltra" is pressed.
    pub fn connect_forward<F: Fn(String, String) + 'static>(&self, f: F) {
        *self.on_forward.borrow_mut() = Some(Box::new(f));
    }

    /// `(chat_jid, id, can_revoke)` when "Elimina" is pressed.
    pub fn connect_delete<F: Fn(String, String, bool) + 'static>(&self, f: F) {
        *self.on_delete.borrow_mut() = Some(Box::new(f));
    }

    /// `(chat_jid, target_id)` when the user navigates to another media (arrows
    /// or Left/Right keys). The handler should re-fetch that media, which lands
    /// back here via `show`.
    pub fn connect_navigate<F: Fn(String, String) + 'static>(&self, f: F) {
        *self.on_navigate.borrow_mut() = Some(Box::new(f));
    }
}

/// An image inside a scroller: scroll wheel zooms, left-drag pans. Returns the
/// scroller plus the inner Picture (the click-to-close bounds widget).
fn build_image_view(path: &str) -> (gtk::Widget, gtk::Widget) {
    let pic = gtk::Picture::for_filename(path);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_halign(gtk::Align::Center);
    pic.set_valign(gtk::Align::Center);

    let scroller = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&pic)
        .build();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);

    // Intrinsic size of the image; absolute scale (px) is `intrinsic * scale`.
    let (iw, ih) = pic
        .paintable()
        .map(|p| (p.intrinsic_width().max(1) as f64, p.intrinsic_height().max(1) as f64))
        .unwrap_or((900.0, 680.0));
    // Start fitted to a typical window size.
    let fit = (860.0 / iw).min(620.0 / ih).min(1.0);
    let scale = Rc::new(Cell::new(fit));
    pic.set_size_request((iw * fit) as i32, (ih * fit) as i32);

    // Scroll to zoom.
    let ctrl = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    {
        let pic = pic.clone();
        let scale = scale.clone();
        ctrl.connect_scroll(move |_, _dx, dy| {
            let next = (scale.get() * if dy < 0.0 { 1.12 } else { 1.0 / 1.12 }).clamp(0.05, 12.0);
            scale.set(next);
            pic.set_size_request((iw * next) as i32, (ih * next) as i32);
            gtk::glib::Propagation::Stop
        });
    }
    scroller.add_controller(ctrl);

    // Left-drag to pan when zoomed past the viewport.
    let drag = gtk::GestureDrag::new();
    let origin = Rc::new(Cell::new((0.0, 0.0)));
    {
        let hadj = scroller.hadjustment();
        let vadj = scroller.vadjustment();
        let origin = origin.clone();
        drag.connect_drag_begin(move |_, _, _| {
            origin.set((hadj.value(), vadj.value()));
        });
    }
    {
        let hadj = scroller.hadjustment();
        let vadj = scroller.vadjustment();
        let origin = origin.clone();
        drag.connect_drag_update(move |_, ox, oy| {
            let (h0, v0) = origin.get();
            hadj.set_value(h0 - ox);
            vadj.set_value(v0 - oy);
        });
    }
    scroller.add_controller(drag);
    scroller.set_cursor_from_name(Some("grab"));

    (scroller.upcast(), pic.upcast())
}
