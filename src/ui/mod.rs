//! The GTK4 / libadwaita user interface (hand-written for these early steps;
//! Blueprint will be introduced later).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub mod chat_list;
pub mod chat_object;
pub mod login;
pub mod media_grid;
pub mod media_viewer;
pub mod profile;
pub mod thread;
pub mod window;

/// Shared, in-memory cache of decoded profile-picture textures keyed by JID.
/// Lives on the GTK main thread; the chat list and conversation view hold clones
/// of the same map so a downloaded avatar is reused everywhere.
pub type AvatarCache = Rc<RefCell<HashMap<String, gtk::gdk::Texture>>>;

/// Creates an empty [`AvatarCache`].
pub fn new_avatar_cache() -> AvatarCache {
    Rc::new(RefCell::new(HashMap::new()))
}
