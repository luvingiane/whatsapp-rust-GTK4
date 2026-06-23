//! A borderless full-size media viewer (one instance at a time): the window *is*
//! the media. Photos are shown with [`gtk::Picture`] and zoom (scroll wheel);
//! videos play with [`gtk::Video`]. Esc closes; right-click offers Apri/Salva.

use std::cell::Cell;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use libadwaita as adw;

/// Builds and presents the viewer over `parent`, returning the window so the
/// caller can keep a single instance. `kind` is 1 image / 2 video; `path` is the
/// decrypted file on disk.
pub fn present(parent: &adw::ApplicationWindow, kind: i32, path: &str) -> adw::Window {
    let window = adw::Window::builder()
        // Non-modal so clicking the main window doesn't dismiss the viewer;
        // transient keeps it above its parent.
        .modal(false)
        .transient_for(parent)
        .default_width(900)
        .default_height(680)
        .build();

    let content: gtk::Widget = if kind == 2 {
        let video = gtk::Video::for_filename(Some(path));
        video.set_autoplay(true);
        video.upcast()
    } else {
        build_image_view(path)
    };
    window.set_content(Some(&content));

    // Lightbox behaviour: clicking outside the viewer (it loses focus) closes it.
    // Guard so it doesn't self-close before it ever became active.
    {
        let seen_active = Rc::new(Cell::new(false));
        window.connect_is_active_notify(move |w| {
            if w.is_active() {
                seen_active.set(true);
            } else if seen_active.get() {
                w.close();
            }
        });
    }

    // Esc also closes the viewer.
    let keys = gtk::EventControllerKey::new();
    {
        let window = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(keys);

    // Right-click anywhere → Apri (esternamente) / Salva una copia.
    let menu = gtk::GestureClick::new();
    menu.set_button(gtk::gdk::BUTTON_SECONDARY);
    {
        let path = path.to_string();
        let parent = parent.clone();
        let window = window.clone();
        menu.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            let pop = gtk::Popover::new();
            pop.set_has_arrow(false);
            pop.set_parent(&window);
            pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let open = gtk::Button::with_label("Apri");
            open.add_css_class("flat");
            open.set_halign(gtk::Align::Start);
            {
                let path = path.clone();
                let pop = pop.clone();
                open.connect_clicked(move |_| {
                    let _ = gtk::gio::Subprocess::newv(
                        &[OsStr::new("xdg-open"), OsStr::new(&path)],
                        gtk::gio::SubprocessFlags::STDOUT_SILENCE
                            | gtk::gio::SubprocessFlags::STDERR_SILENCE,
                    );
                    pop.popdown();
                });
            }
            bx.append(&open);
            let save = gtk::Button::with_label("Salva");
            save.add_css_class("flat");
            save.set_halign(gtk::Align::Start);
            {
                let src = PathBuf::from(&path);
                let parent = parent.clone();
                let pop = pop.clone();
                save.connect_clicked(move |_| {
                    let dialog = gtk::FileDialog::builder()
                        .title("Salva media")
                        .initial_name(src.file_name().and_then(|n| n.to_str()).unwrap_or("media"))
                        .build();
                    let src = src.clone();
                    dialog.save(Some(&parent), gtk::gio::Cancellable::NONE, move |res| {
                        if let Ok(file) = res {
                            if let Some(dest) = file.path() {
                                if let Err(e) = std::fs::copy(&src, &dest) {
                                    log::warn!("save media failed: {e}");
                                }
                            }
                        }
                    });
                    pop.popdown();
                });
            }
            bx.append(&save);
            pop.set_child(Some(&bx));
            pop.connect_closed(|p| p.unparent());
            pop.popup();
        });
    }
    window.add_controller(menu);

    window.present();
    window
}

/// An image inside a scroller: scroll wheel zooms, left-drag pans.
fn build_image_view(path: &str) -> gtk::Widget {
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
    // Start fitted to the default window size.
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

    scroller.upcast()
}
