//! JID string helpers shared by the backend and the UI.

/// Rejects placeholder/invalid user JIDs (empty or `0@…`) so we don't fire
/// pointless avatar/usync requests for them (e.g. a stray `0@s.whatsapp.net`).
pub fn is_real_user_jid(jid: &str) -> bool {
    match jid.split_once('@') {
        Some((user, _)) => !user.is_empty() && user != "0",
        None => false,
    }
}

/// Strips a JID's device suffix (`user:NN@server` → `user@server`) so all of a
/// person's linked devices collapse to one sender identity. A group member posting
/// from multiple devices otherwise appears as several "users", and the
/// device-suffixed JID misses the contact-name join (showing a bare number).
pub fn base_jid(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, server)) => {
            let user = user.split(':').next().unwrap_or(user);
            format!("{user}@{server}")
        }
        None => jid.to_string(),
    }
}
