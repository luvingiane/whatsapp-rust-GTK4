//! The application state database (our own SQLite, separate from whatsapp.db).
//!
//! whatsapp-rust does not keep a full chat/message model, so we are the source
//! of truth for the UI. This store is owned entirely by the backend (Tokio)
//! side; the UI never touches it. The `rusqlite` connection is synchronous, so
//! every operation runs inside `spawn_blocking`, serialized by a `Mutex`.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};

use crate::model::ChatSummary;
use crate::util::preview;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chats (
  jid          TEXT PRIMARY KEY,
  name         TEXT NOT NULL DEFAULT '',
  last_message TEXT NOT NULL DEFAULT '',
  last_ts      INTEGER NOT NULL DEFAULT 0,
  last_from_me INTEGER NOT NULL DEFAULT 0,
  unread       INTEGER NOT NULL DEFAULT 0,
  is_group     INTEGER NOT NULL DEFAULT 0,
  archived     INTEGER NOT NULL DEFAULT 0,
  pinned       INTEGER NOT NULL DEFAULT 0,
  muted_until  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS contacts (
  jid  TEXT PRIMARY KEY,
  name TEXT NOT NULL DEFAULT ''
);
-- App-state metadata keyed by chat JID, written by app-state events
-- independently of whether the chat row exists yet (avoids ordering races and
-- the history-sync overwrite). Resolved via LEFT JOIN at read time.
CREATE TABLE IF NOT EXISTS chat_meta (
  jid         TEXT PRIMARY KEY,
  archived    INTEGER NOT NULL DEFAULT 0,
  pinned      INTEGER NOT NULL DEFAULT 0,
  muted_until INTEGER NOT NULL DEFAULT 0,
  saved_name  TEXT NOT NULL DEFAULT ''
);
";

/// An owned chat row to upsert. We extract these from the (borrowed) protobuf on
/// the async side so the values can move into `spawn_blocking`.
pub struct ChatUpsert {
    pub jid: String,
    pub name: String,
    pub last_message: String,
    pub last_ts: i64,
    pub last_from_me: bool,
    pub unread: u32,
    pub is_group: bool,
    pub archived: bool,
    pub pinned: bool,
    pub muted_until: i64,
}

/// Handle to the application DB. Cheap to clone (shares one connection).
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (creating if needed) the DB at `path`, enabling WAL and applying
    /// the schema.
    pub async fn open(path: String) -> Result<Self> {
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&path)?;
            // WAL keeps reads/writes from blocking each other and is crash-safe.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.execute_batch(SCHEMA)?;
            Ok(conn)
        })
        .await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Bulk upsert from a history sync. Newer authoritative server fields win,
    /// but we never regress `last_*` below a more recent live message we already
    /// stored (guarded by `last_ts`).
    pub async fn upsert_chats(&self, rows: Vec<ChatUpsert>) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let tx = guard.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO chats
                       (jid,name,last_message,last_ts,last_from_me,unread,is_group,archived,pinned,muted_until)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                     ON CONFLICT(jid) DO UPDATE SET
                       name=CASE WHEN excluded.name<>'' THEN excluded.name ELSE chats.name END,
                       is_group=excluded.is_group,
                       archived=excluded.archived,
                       pinned=excluded.pinned,
                       muted_until=excluded.muted_until,
                       unread=excluded.unread,
                       last_message=CASE WHEN excluded.last_ts>=chats.last_ts
                                         THEN excluded.last_message ELSE chats.last_message END,
                       last_from_me=CASE WHEN excluded.last_ts>=chats.last_ts
                                         THEN excluded.last_from_me ELSE chats.last_from_me END,
                       last_ts=MAX(excluded.last_ts, chats.last_ts)",
                )?;
                for r in &rows {
                    stmt.execute(params![
                        r.jid,
                        r.name,
                        r.last_message,
                        r.last_ts,
                        r.last_from_me as i64,
                        r.unread,
                        r.is_group as i64,
                        r.archived as i64,
                        r.pinned as i64,
                        r.muted_until,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    /// Upsert contact pushnames (the profile names contacts set for themselves),
    /// keyed by JID. A non-empty incoming name overwrites the stored one; empty
    /// names are ignored so we never clobber a known name with a blank.
    pub async fn upsert_contacts(&self, rows: Vec<(String, String)>) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let tx = guard.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO contacts (jid, name) VALUES (?1, ?2)
                     ON CONFLICT(jid) DO UPDATE SET
                       name=CASE WHEN excluded.name<>'' THEN excluded.name ELSE contacts.name END",
                )?;
                for (jid, name) in &rows {
                    if name.is_empty() {
                        continue;
                    }
                    stmt.execute(params![jid, name])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    /// Apply a single live message (sent or received) to its chat: refresh the
    /// preview/timestamp and bump unread for incoming messages.
    pub async fn apply_message(
        &self,
        jid: String,
        text: String,
        ts: i64,
        from_me: bool,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let is_group = jid.ends_with("@g.us");
            let inc: i64 = if from_me { 0 } else { 1 };
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // We deliberately store an empty name here: the display name is
            // resolved at read time (chat name → contact pushname → number).
            guard.execute(
                "INSERT INTO chats (jid,name,last_message,last_ts,last_from_me,unread,is_group)
                 VALUES (?1,'',?2,?3,?4,?5,?6)
                 ON CONFLICT(jid) DO UPDATE SET
                   last_message=CASE WHEN excluded.last_ts>=chats.last_ts
                                     THEN excluded.last_message ELSE chats.last_message END,
                   last_from_me=CASE WHEN excluded.last_ts>=chats.last_ts
                                     THEN excluded.last_from_me ELSE chats.last_from_me END,
                   last_ts=MAX(excluded.last_ts, chats.last_ts),
                   unread=chats.unread + ?5",
                params![jid, text, ts, from_me as i64, inc, is_group as i64],
            )?;
            Ok(())
        })
        .await?
    }

    /// Sets the saved (address-book) name for a chat, from a `ContactAction`.
    pub async fn set_saved_name(&self, jid: String, name: String) -> Result<()> {
        if name.is_empty() {
            return Ok(());
        }
        self.set_meta(
            "INSERT INTO chat_meta (jid, saved_name) VALUES (?1, ?2)
             ON CONFLICT(jid) DO UPDATE SET saved_name=?2",
            jid,
            name,
        )
        .await
    }

    /// Sets the archived flag for a chat, from an `ArchiveChatAction`.
    pub async fn set_archived(&self, jid: String, archived: bool) -> Result<()> {
        self.set_meta(
            "INSERT INTO chat_meta (jid, archived) VALUES (?1, ?2)
             ON CONFLICT(jid) DO UPDATE SET archived=?2",
            jid,
            archived as i64,
        )
        .await
    }

    /// Sets the pinned flag for a chat, from a `PinAction`.
    pub async fn set_pinned(&self, jid: String, pinned: bool) -> Result<()> {
        self.set_meta(
            "INSERT INTO chat_meta (jid, pinned) VALUES (?1, ?2)
             ON CONFLICT(jid) DO UPDATE SET pinned=?2",
            jid,
            pinned as i64,
        )
        .await
    }

    /// Sets the mute end timestamp for a chat (0 = not muted), from a `MuteAction`.
    pub async fn set_muted(&self, jid: String, muted: bool, until: i64) -> Result<()> {
        let value = if muted { until.max(1) } else { 0 };
        self.set_meta(
            "INSERT INTO chat_meta (jid, muted_until) VALUES (?1, ?2)
             ON CONFLICT(jid) DO UPDATE SET muted_until=?2",
            jid,
            value,
        )
        .await
    }

    /// Upserts one `chat_meta` column for `jid` (?1) with value `?2`. Works even
    /// if the chat row doesn't exist yet — metadata is keyed independently.
    async fn set_meta<V>(&self, sql: &'static str, jid: String, value: V) -> Result<()>
    where
        V: rusqlite::ToSql + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(sql, params![jid, value])?;
            Ok(())
        })
        .await?
    }

    /// Returns the non-archived chats, pinned first then most-recent first.
    pub async fn list_chats(&self) -> Result<Vec<ChatSummary>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<ChatSummary>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // Resolve the display name: group subject → saved address-book name
            // → contact pushname → (number, filled in below). Archive/pin come
            // from chat_meta (app-state), independent of the chat row.
            let mut stmt = guard.prepare(
                "SELECT c.jid,
                        COALESCE(NULLIF(c.name,''), NULLIF(m.saved_name,''), NULLIF(ct.name,''), '') AS name,
                        c.last_message, c.last_ts, c.last_from_me, c.unread, c.is_group,
                        COALESCE(m.pinned,0) AS pinned
                 FROM chats c
                 LEFT JOIN contacts ct ON ct.jid = c.jid
                 LEFT JOIN chat_meta m ON m.jid = c.jid
                 WHERE COALESCE(m.archived,0)=0
                 ORDER BY pinned DESC, c.last_ts DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    let jid: String = r.get(0)?;
                    let raw_name: String = r.get(1)?;
                    let name = if raw_name.is_empty() {
                        preview::pretty_number(&jid)
                    } else {
                        raw_name
                    };
                    Ok(ChatSummary {
                        jid,
                        name,
                        last_message: r.get(2)?,
                        last_ts: r.get(3)?,
                        last_from_me: r.get::<_, i64>(4)? != 0,
                        unread: r.get::<_, i64>(5)? as u32,
                        is_group: r.get::<_, i64>(6)? != 0,
                        pinned: r.get::<_, i64>(7)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }
}
