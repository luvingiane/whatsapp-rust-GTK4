//! `ChatObject`: a tiny GObject wrapping a [`ChatSummary`], so chat rows can live
//! in a `gio::ListStore` and be rendered by a `ListView` factory.

use gtk::glib;

use crate::model::ChatSummary;

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::glib;
    use gtk::glib::Properties;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    #[derive(Properties, Default)]
    #[properties(wrapper_type = super::ChatObject)]
    pub struct ChatObject {
        #[property(get, set)]
        pub jid: RefCell<String>,
        #[property(get, set)]
        pub name: RefCell<String>,
        #[property(get, set)]
        pub last_message: RefCell<String>,
        #[property(get, set)]
        pub timestamp: RefCell<String>,
        #[property(get, set)]
        pub unread: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChatObject {
        const NAME: &'static str = "WrgChatObject";
        type Type = super::ChatObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for ChatObject {}
}

glib::wrapper! {
    pub struct ChatObject(ObjectSubclass<imp::ChatObject>);
}

impl ChatObject {
    pub fn new(s: &ChatSummary) -> Self {
        glib::Object::builder()
            .property("jid", &s.jid)
            .property("name", &s.name)
            .property("last-message", &s.last_message)
            .property("timestamp", format_ts(s.last_ts))
            .property("unread", s.unread)
            .build()
    }
}

/// Formats a unix timestamp for the chat list: `HH:MM` if today, else `dd/mm/yy`.
fn format_ts(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let Ok(dt) = glib::DateTime::from_unix_local(ts) else {
        return String::new();
    };
    let same_day = glib::DateTime::now_local()
        .map(|now| now.year() == dt.year() && now.day_of_year() == dt.day_of_year())
        .unwrap_or(false);
    let fmt = if same_day { "%H:%M" } else { "%d/%m/%y" };
    dt.format(fmt).map(|g| g.to_string()).unwrap_or_default()
}
