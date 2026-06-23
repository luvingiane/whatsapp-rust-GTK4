//! The application state database (our own SQLite, separate from whatsapp.db).
//!
//! whatsapp-rust does not keep a full chat/message model, so we are the source
//! of truth for the UI. This store is owned entirely by the backend (Tokio)
//! side; the UI never touches it. The `rusqlite` connection is synchronous, so
//! every operation runs inside `spawn_blocking`, serialized by a `Mutex`.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{ChatSummary, MediaItem, MessageRow};
use crate::util::preview;

/// A `chat_meta` row: (jid, archived, pinned, muted_until, saved_name).
type ChatMetaRow = (String, bool, bool, i64, String);

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
  muted_until  INTEGER NOT NULL DEFAULT 0,
  -- Delivery status of the last message when we sent it (0 none, 1 sent,
  -- 2 delivered, 3 read), for the ✓/✓✓ glyph in the chat-list preview.
  last_status  INTEGER NOT NULL DEFAULT 0
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
-- Learned LID(@lid) ↔ PN(@s.whatsapp.net) JID pairs. App-state events (archive,
-- pin, …) arrive keyed by @lid while 1:1 chats are keyed by PN; this map lets us
-- re-key the metadata onto the chat row. Learned from ContactUpdate (pn/lid jids)
-- and message source alt-forms, which the LID↔PN library map fills only slowly.
CREATE TABLE IF NOT EXISTS lid_map (
  lid TEXT PRIMARY KEY,
  pn  TEXT NOT NULL
);
-- Per-chat message history for the conversation view. (chat_jid,id) is unique so
-- repeated history syncs / reconnects don't duplicate rows.
CREATE TABLE IF NOT EXISTS messages (
  chat_jid   TEXT NOT NULL,
  id         TEXT NOT NULL,
  sender_jid TEXT NOT NULL DEFAULT '',
  from_me    INTEGER NOT NULL DEFAULT 0,
  ts         INTEGER NOT NULL DEFAULT 0,
  body       TEXT NOT NULL DEFAULT '',
  -- Delivery status for our own messages: 0 none/incoming, 1 sent (✓),
  -- 2 delivered (✓✓), 3 read/played (✓✓ blue).
  status     INTEGER NOT NULL DEFAULT 0,
  -- Serialized wa::Message proto for downloadable media (audio/voice notes), so
  -- the note can be fetched + decrypted on play. NULL for non-media messages.
  media      BLOB,
  -- Voice-note duration (seconds) and amplitude waveform (0..100 per bar) for the
  -- player UI. NULL for non-audio messages.
  audio_secs INTEGER,
  audio_waveform BLOB,
  -- If this message quotes another: the quoted preview text + the quoted author's
  -- JID (resolved to a name at read time). NULL when it's not a reply.
  reply_text TEXT,
  reply_sender TEXT,
  -- Media metadata (the proto itself is in `media`): kind 0 none/1 image/2 video/
  -- 3 audio/4 document/5 sticker, MIME, document name, and a small JPEG thumbnail.
  media_kind INTEGER NOT NULL DEFAULT 0,
  media_mime TEXT,
  media_name TEXT,
  media_thumb BLOB,
  media_size INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (chat_jid, id)
);
CREATE INDEX IF NOT EXISTS idx_messages_chat_ts ON messages(chat_jid, ts);
";

/// Additive, idempotent column migrations for DBs created before a column
/// existed. `ALTER TABLE ADD COLUMN` errors if the column is already present, so
/// each is run independently and a duplicate-column error is ignored — we never
/// recreate or drop the existing session DB.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE chats ADD COLUMN last_status INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE messages ADD COLUMN status INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE messages ADD COLUMN media BLOB",
    "ALTER TABLE messages ADD COLUMN audio_secs INTEGER",
    "ALTER TABLE messages ADD COLUMN audio_waveform BLOB",
    "ALTER TABLE messages ADD COLUMN reply_text TEXT",
    "ALTER TABLE messages ADD COLUMN reply_sender TEXT",
    "ALTER TABLE messages ADD COLUMN media_kind INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE messages ADD COLUMN media_mime TEXT",
    "ALTER TABLE messages ADD COLUMN media_name TEXT",
    "ALTER TABLE messages ADD COLUMN media_thumb BLOB",
    "ALTER TABLE messages ADD COLUMN media_size INTEGER NOT NULL DEFAULT 0",
];

/// Run on every open. (1) Baseline ✓ for our own messages that lack a status.
/// (2) Derive each chat's `last_status` from its latest own message — so the
/// list-preview tick is correct on a plain relaunch (no fresh history sync, which
/// only happens at pairing) using the per-message status already stored.
const BACKFILL: &[&str] = &[
    "UPDATE messages SET status=1 WHERE from_me=1 AND status=0",
    "UPDATE chats SET last_status = COALESCE((
        SELECT m.status FROM messages m
        WHERE m.chat_jid=chats.jid AND m.from_me=1
        ORDER BY m.ts DESC, m.id DESC LIMIT 1
     ), last_status)
     WHERE last_from_me=1",
];

/// Restricts a SQLite DB (and its `-wal`/`-shm` sidecars) to owner-only (0600).
/// SQLite stores everything in plaintext, so this keeps other local users from
/// reading the session keys / chat history. Best-effort; Unix-only.
#[cfg(unix)]
pub fn restrict_db_perms(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let p = format!("{path}{suffix}");
        if let Ok(meta) = std::fs::metadata(&p) {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = std::fs::set_permissions(&p, perm);
        }
    }
}

#[cfg(not(unix))]
pub fn restrict_db_perms(_path: &str) {}

/// An owned chat row to upsert. We extract these from the (borrowed) protobuf on
/// the async side so the values can move into `spawn_blocking`.
pub struct ChatUpsert {
    pub jid: String,
    pub name: String,
    pub last_message: String,
    pub last_ts: i64,
    pub last_from_me: bool,
    pub last_status: i32,
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
            // Bring older DBs up to date without recreating them. A
            // duplicate-column error just means the migration already ran.
            for sql in MIGRATIONS {
                if let Err(e) = conn.execute(sql, []) {
                    let msg = e.to_string();
                    if !msg.contains("duplicate column name") {
                        return Err(e.into());
                    }
                }
            }
            // Baseline ✓ for our own messages imported before `status` existed
            // (they'd otherwise show no tick). Idempotent: only touches status 0.
            // Live receipts / re-syncs later upgrade these to ✓✓ / read.
            for sql in BACKFILL {
                conn.execute(sql, [])?;
            }
            // The DB holds chat history + media in plaintext (SQLite is not
            // encrypted at rest); restrict it to the owner so other local users
            // can't read it. Best-effort, Unix-only.
            restrict_db_perms(&path);
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
                       (jid,name,last_message,last_ts,last_from_me,unread,is_group,archived,pinned,muted_until,last_status)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
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
                       last_status=CASE WHEN excluded.last_ts>=chats.last_ts
                                        THEN excluded.last_status ELSE chats.last_status END,
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
                        r.last_status,
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
    /// preview/timestamp/status and bump unread for incoming messages. `status` is
    /// the delivery state of an outgoing message (0 for incoming).
    pub async fn apply_message(
        &self,
        jid: String,
        text: String,
        ts: i64,
        from_me: bool,
        status: i32,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let is_group = jid.ends_with("@g.us");
            let inc: i64 = if from_me { 0 } else { 1 };
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // We deliberately store an empty name here: the display name is
            // resolved at read time (chat name → contact pushname → number).
            guard.execute(
                "INSERT INTO chats (jid,name,last_message,last_ts,last_from_me,unread,is_group,last_status)
                 VALUES (?1,'',?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(jid) DO UPDATE SET
                   last_message=CASE WHEN excluded.last_ts>=chats.last_ts
                                     THEN excluded.last_message ELSE chats.last_message END,
                   last_from_me=CASE WHEN excluded.last_ts>=chats.last_ts
                                     THEN excluded.last_from_me ELSE chats.last_from_me END,
                   last_status=CASE WHEN excluded.last_ts>=chats.last_ts
                                    THEN excluded.last_status ELSE chats.last_status END,
                   last_ts=MAX(excluded.last_ts, chats.last_ts),
                   unread=chats.unread + ?5",
                params![jid, text, ts, from_me as i64, inc, is_group as i64, status],
            )?;
            Ok(())
        })
        .await?
    }

    /// Upgrades an outgoing message's delivery status (never downgrades), and
    /// keeps the chat's `last_status` in sync when the updated row is the latest
    /// message of the chat. Driven by `Event::Receipt`. Returns the number of
    /// message rows actually changed (0 = no matching/forward-moving row), for
    /// diagnostics.
    pub async fn update_message_status(
        &self,
        chat_jid: String,
        id: String,
        status: i32,
    ) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // Only ever move forward (sent → delivered → read).
            let changed = guard.execute(
                "UPDATE messages SET status=?3
                 WHERE chat_jid=?1 AND id=?2 AND status<?3",
                params![chat_jid, id, status],
            )?;
            // Reflect on the chat preview if this is the most recent message.
            guard.execute(
                "UPDATE chats SET last_status=?2
                 WHERE jid=?1 AND last_from_me=1
                   AND last_ts=(SELECT MAX(ts) FROM messages WHERE chat_jid=?1)
                   AND ?2>last_status
                   AND ?3=(SELECT id FROM messages
                           WHERE chat_jid=?1 ORDER BY ts DESC, id DESC LIMIT 1)",
                params![chat_jid, status, id],
            )?;
            Ok(changed)
        })
        .await?
    }

    /// Sets a chat's unread counter. Used to clear it (n=0) when the chat is read
    /// — either opened locally or marked read on another device (the phone) via
    /// an app-state `MarkChatAsReadAction`. Keyed by `jid`; a no-op if no row.
    pub async fn set_unread(&self, jid: String, n: i64) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "UPDATE chats SET unread=?2 WHERE jid=?1",
                params![jid, n],
            )?;
            Ok(())
        })
        .await?
    }

    /// Convenience: clears a chat's unread counter (marks it read).
    pub async fn clear_unread(&self, jid: String) -> Result<()> {
        self.set_unread(jid, 0).await
    }

    /// Records a learned LID↔PN pair (both full JIDs). Ignored unless `lid` is an
    /// `@lid` JID and `pn` an `@s.whatsapp.net` JID.
    pub async fn learn_lid_pn(&self, lid: String, pn: String) -> Result<()> {
        if !lid.ends_with("@lid") || !pn.ends_with("@s.whatsapp.net") {
            return Ok(());
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "INSERT INTO lid_map (lid, pn) VALUES (?1, ?2)
                 ON CONFLICT(lid) DO UPDATE SET pn=excluded.pn",
                params![lid, pn],
            )?;
            Ok(())
        })
        .await?
    }

    /// Re-keys app-state metadata that landed on an `@lid` JID onto the PN form of
    /// the same contact, using the learned [`lid_map`]. App-state archive/pin
    /// events arrive as `@lid` while 1:1 chats are keyed by PN, so without this the
    /// flags never match the chat row. The `@lid` value is authoritative (it is the
    /// event's own key). Returns the number of PN rows written, for diagnostics.
    pub async fn propagate_lid_meta(&self) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let n = guard.execute(
                "INSERT INTO chat_meta (jid, archived, pinned, muted_until, saved_name)
                 SELECT lm.pn, m.archived, m.pinned, m.muted_until, m.saved_name
                 FROM chat_meta m JOIN lid_map lm ON lm.lid = m.jid
                 WHERE m.jid LIKE '%@lid'
                 ON CONFLICT(jid) DO UPDATE SET
                   archived=excluded.archived,
                   pinned=excluded.pinned,
                   muted_until=excluded.muted_until,
                   saved_name=CASE WHEN excluded.saved_name<>''
                                   THEN excluded.saved_name ELSE chat_meta.saved_name END",
                [],
            )?;
            Ok(n)
        })
        .await?
    }

    /// Collapses duplicate chats: a contact addressed once by `@lid` (before the
    /// PN mapping was known) and once by phone number ends up as two rows. For
    /// every `@lid` chat whose PN is known (via `lid_map`), move its messages onto
    /// the PN chat (media follows the row), merge the chat row (newest `last_*`,
    /// summed unread), and delete the `@lid` row + its `chat_meta`. Returns the
    /// number of chats merged.
    pub async fn merge_lid_duplicates(&self) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let tx = guard.transaction()?;
            let pairs: Vec<(String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT lm.lid, lm.pn FROM lid_map lm JOIN chats c ON c.jid = lm.lid",
                )?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let mut merged = 0usize;
            for (lid, pn) in &pairs {
                // Move messages to the PN key (media column follows); drop any that
                // collide on (pn,id) and the now-orphaned @lid rows.
                tx.execute(
                    "UPDATE OR IGNORE messages SET chat_jid=?2 WHERE chat_jid=?1",
                    params![lid, pn],
                )?;
                tx.execute("DELETE FROM messages WHERE chat_jid=?1", params![lid])?;
                // Merge the chat row into the PN one.
                let lidrow = tx
                    .query_row(
                        "SELECT name,last_message,last_ts,last_from_me,unread,is_group,COALESCE(last_status,0)
                         FROM chats WHERE jid=?1",
                        params![lid],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, i64>(3)?,
                                r.get::<_, i64>(4)?,
                                r.get::<_, i64>(5)?,
                                r.get::<_, i64>(6)?,
                            ))
                        },
                    )
                    .optional()?;
                if let Some((name, last_message, last_ts, last_from_me, unread, is_group, last_status)) =
                    lidrow
                {
                    tx.execute(
                        "INSERT INTO chats (jid,name,last_message,last_ts,last_from_me,unread,is_group,last_status)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                         ON CONFLICT(jid) DO UPDATE SET
                           name=CASE WHEN chats.name='' AND excluded.name<>'' THEN excluded.name ELSE chats.name END,
                           unread=chats.unread + excluded.unread,
                           last_message=CASE WHEN excluded.last_ts>chats.last_ts THEN excluded.last_message ELSE chats.last_message END,
                           last_from_me=CASE WHEN excluded.last_ts>chats.last_ts THEN excluded.last_from_me ELSE chats.last_from_me END,
                           last_status=CASE WHEN excluded.last_ts>chats.last_ts THEN excluded.last_status ELSE chats.last_status END,
                           last_ts=MAX(excluded.last_ts, chats.last_ts)",
                        params![pn, name, last_message, last_ts, last_from_me, unread, is_group, last_status],
                    )?;
                    tx.execute("DELETE FROM chats WHERE jid=?1", params![lid])?;
                    tx.execute("DELETE FROM chat_meta WHERE jid=?1", params![lid])?;
                    merged += 1;
                }
            }
            tx.commit()?;
            Ok(merged)
        })
        .await?
    }

    /// Collapses device-suffixed sender JIDs (`user:NN@server`) on existing messages
    /// to their base (`user@server`), so a group member who posted from several
    /// devices stops showing as duplicate "users" and matches the contact-name join.
    /// Idempotent. Returns the number of rows rewritten.
    pub async fn normalize_message_senders(&self) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // For `user:NN@server`, keep `user` (before ':') + `@server` (from '@').
            let n = guard.execute(
                "UPDATE messages
                 SET sender_jid = substr(sender_jid, 1, instr(sender_jid, ':') - 1)
                                  || substr(sender_jid, instr(sender_jid, '@'))
                 WHERE sender_jid LIKE '%:%@%'
                   AND instr(sender_jid, ':') < instr(sender_jid, '@')",
                [],
            )?;
            Ok(n)
        })
        .await?
    }

    /// If the chat has unread incoming messages, returns the latest incoming
    /// message's `(id, sender_jid)` — the target for a read receipt that tells our
    /// other devices the chat was read here (the same read-self fanout we receive
    /// when reading on WhatsApp Web). `None` if nothing unread.
    pub async fn read_receipt_target(&self, chat: String) -> Result<Option<(String, String)>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<(String, String)>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let unread: i64 = guard
                .query_row("SELECT unread FROM chats WHERE jid=?1", params![chat], |r| {
                    r.get(0)
                })
                .optional()?
                .unwrap_or(0);
            if unread <= 0 {
                return Ok(None);
            }
            let row = guard
                .query_row(
                    "SELECT id, sender_jid FROM messages
                     WHERE chat_jid=?1 AND from_me=0
                     ORDER BY ts DESC, id DESC LIMIT 1",
                    params![chat],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            Ok(row)
        })
        .await?
    }

    /// Returns the distinct chat JIDs that own any of the given message ids. Used to
    /// resolve which conversation a read-self receipt refers to (its `chat` field is
    /// our own reading device, not the conversation).
    pub async fn chats_for_messages(&self, ids: Vec<String>) -> Result<Vec<String>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql =
                format!("SELECT DISTINCT chat_jid FROM messages WHERE id IN ({placeholders})");
            let mut stmt = guard.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Returns the stored display name of a chat row (group subject / name), or an
    /// empty string if the row is absent or unnamed.
    pub async fn chat_name(&self, jid: String) -> Result<String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let name = guard
                .query_row("SELECT name FROM chats WHERE jid=?1", params![jid], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .unwrap_or_default();
            Ok(name)
        })
        .await?
    }

    /// Number of media messages (photos/videos/documents) in a chat.
    pub async fn chat_media_count(&self, jid: String) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let n: i64 = guard.query_row(
                "SELECT COUNT(*) FROM messages WHERE chat_jid=?1 AND media_kind IN (1,2,4)",
                params![jid],
                |r| r.get(0),
            )?;
            Ok(n as usize)
        })
        .await?
    }

    /// All media messages of a chat (photos/videos/documents), newest first.
    pub async fn chat_media(&self, jid: String) -> Result<Vec<MediaItem>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MediaItem>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let mut stmt = guard.prepare(
                "SELECT id, media_kind, media_mime, media_name, media_size, media_thumb
                 FROM messages WHERE chat_jid=?1 AND media_kind IN (1,2,4)
                 ORDER BY ts DESC, id DESC",
            )?;
            let rows = stmt
                .query_map(params![jid], |r| {
                    Ok(MediaItem {
                        id: r.get(0)?,
                        kind: r.get::<_, i64>(1)? as i32,
                        mime: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        name: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        size: r.get(4)?,
                        thumb: r.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Message bodies of a chat that likely contain a URL (newest first), for the
    /// profile's Links tab. URL extraction happens in the caller.
    pub async fn chat_link_bodies(&self, jid: String) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let mut stmt = guard.prepare(
                "SELECT body FROM messages
                 WHERE chat_jid=?1 AND (body LIKE '%http%' OR body LIKE '%www.%')
                 ORDER BY ts DESC, id DESC",
            )?;
            let rows = stmt
                .query_map(params![jid], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Stores a group's subject as its chat name (no-op for an empty subject so we
    /// never blank an existing name).
    pub async fn set_group_subject(&self, jid: String, subject: String) -> Result<()> {
        if subject.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "UPDATE chats SET name=?2 WHERE jid=?1",
                params![jid, subject],
            )?;
            Ok(())
        })
        .await?
    }

    /// Active (non-archived, non-empty-preview) group chats with no stored name —
    /// candidates for a one-shot subject fetch so the list shows their real name.
    pub async fn unnamed_active_groups(&self) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let mut stmt = guard.prepare(
                "SELECT c.jid FROM chats c
                 LEFT JOIN chat_meta m ON m.jid = c.jid
                 WHERE c.jid LIKE '%@g.us' AND c.name='' AND c.last_message<>''
                   AND COALESCE(m.archived,0)=0",
            )?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Returns the JIDs of every `@lid`-keyed chat row. Used by the reconcile loop
    /// to ask the library for each one's PN and learn the pair, so
    /// [`Self::merge_lid_duplicates`] can collapse the duplicate.
    pub async fn lid_chats(&self) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let mut stmt = guard.prepare("SELECT jid FROM chats WHERE jid LIKE '%@lid'")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
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
    ///
    /// Chats with no displayable preview (`last_message` empty — e.g. only
    /// system/protocol messages, or whose content sync stalled) are hidden here
    /// to match the wrapper's active list. NOTE: an empty preview is really a
    /// sync bug we should fix later; the filter is a deliberate stopgap.
    pub async fn list_chats(&self) -> Result<Vec<ChatSummary>> {
        self.query_chats(false).await
    }

    /// Returns the archived chats, pinned first then most-recent first. Empty
    /// previews are kept: this view simply lists everything that is archived.
    pub async fn list_archived_chats(&self) -> Result<Vec<ChatSummary>> {
        self.query_chats(true).await
    }

    /// Shared chat-list query. `archived` selects the archived set (`=1`) vs the
    /// active set (`=0`, with empty-preview chats hidden). Archive/pin come from
    /// `chat_meta` (app-state), independent of the chat row.
    async fn query_chats(&self, archived: bool) -> Result<Vec<ChatSummary>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<ChatSummary>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // Resolve the display name: group subject → saved address-book name
            // → contact pushname → (number, filled in below).
            let filter = if archived {
                "WHERE COALESCE(m.archived,0)=1"
            } else {
                "WHERE COALESCE(m.archived,0)=0 AND c.last_message<>''"
            };
            let sql = format!(
                "SELECT c.jid,
                        COALESCE(NULLIF(c.name,''), NULLIF(m.saved_name,''), NULLIF(ct.name,''), '') AS name,
                        c.last_message, c.last_ts, c.last_from_me, c.unread, c.is_group,
                        COALESCE(m.pinned,0) AS pinned, c.last_status
                 FROM chats c
                 LEFT JOIN contacts ct ON ct.jid = c.jid
                 LEFT JOIN chat_meta m ON m.jid = c.jid
                 {filter}
                 ORDER BY pinned DESC, c.last_ts DESC"
            );
            let mut stmt = guard.prepare(&sql)?;
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
                        last_status: r.get::<_, i64>(8)? as i32,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Bulk-inserts messages (from a history sync). Existing (chat_jid,id) rows
    /// are kept, so repeated syncs don't duplicate.
    pub async fn insert_messages(&self, rows: Vec<MessageRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let tx = guard.transaction()?;
            {
                // Keep existing rows, but let a re-sync UPGRADE the delivery
                // status (never regress it) — older imports stored status 0
                // before the column existed.
                let mut stmt = tx.prepare(
                    "INSERT INTO messages
                       (chat_jid,id,sender_jid,from_me,ts,body,status)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(chat_jid,id) DO UPDATE SET
                       status=MAX(messages.status, excluded.status)",
                )?;
                for m in &rows {
                    stmt.execute(params![
                        m.chat_jid,
                        m.id,
                        m.sender_jid,
                        m.from_me as i64,
                        m.ts,
                        m.body,
                        m.status
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    /// Inserts a single live message (idempotent on `(chat_jid,id)`). Returns
    /// `true` if the row was newly inserted, `false` if it already existed — used
    /// to suppress the self-fanout echo of a message we already inserted
    /// optimistically on send (avoids a duplicate bubble).
    pub async fn insert_message(&self, m: MessageRow) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let n = guard.execute(
                "INSERT OR IGNORE INTO messages (chat_jid,id,sender_jid,from_me,ts,body,status)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    m.chat_jid,
                    m.id,
                    m.sender_jid,
                    m.from_me as i64,
                    m.ts,
                    m.body,
                    m.status
                ],
            )?;
            Ok(n > 0)
        })
        .await?
    }

    /// Stores the serialized media proto for a message (audio/voice note), so it
    /// can be downloaded + decrypted on play. Idempotent.
    pub async fn set_media(&self, chat_jid: String, id: String, media: Vec<u8>) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "UPDATE messages SET media=?3 WHERE chat_jid=?1 AND id=?2",
                params![chat_jid, id, media],
            )?;
            Ok(())
        })
        .await?
    }

    /// Stores media metadata (kind/MIME/name/thumbnail) for a downloadable media
    /// message. The proto itself is stored separately via [`Self::set_media`].
    pub async fn set_media_meta(
        &self,
        chat_jid: String,
        id: String,
        kind: i32,
        mime: String,
        name: String,
        thumb: Vec<u8>,
        size: i64,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "UPDATE messages SET media_kind=?3, media_mime=?4, media_name=?5, media_thumb=?6, media_size=?7
                 WHERE chat_jid=?1 AND id=?2",
                params![chat_jid, id, kind, mime, name, thumb, size],
            )?;
            Ok(())
        })
        .await?
    }

    /// Stores a voice note's duration + waveform for the player UI. Idempotent.
    pub async fn set_audio_meta(
        &self,
        chat_jid: String,
        id: String,
        secs: u32,
        waveform: Vec<u8>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "UPDATE messages SET audio_secs=?3, audio_waveform=?4 WHERE chat_jid=?1 AND id=?2",
                params![chat_jid, id, secs, waveform],
            )?;
            Ok(())
        })
        .await?
    }

    /// Stores the quote info for a reply message (the quoted preview text + the
    /// quoted author's JID), for rendering the quote block. Idempotent.
    pub async fn set_reply(
        &self,
        chat_jid: String,
        id: String,
        sender: String,
        text: String,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            guard.execute(
                "UPDATE messages SET reply_text=?3, reply_sender=?4 WHERE chat_jid=?1 AND id=?2",
                params![chat_jid, id, text, sender],
            )?;
            Ok(())
        })
        .await?
    }

    /// Resolves a display name for `jid` (saved address-book name → contact
    /// pushname), or empty if unknown.
    pub async fn display_name(&self, jid: String) -> Result<String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let name = guard
                .query_row(
                    "SELECT COALESCE(NULLIF((SELECT saved_name FROM chat_meta WHERE jid=?1),''),
                                     NULLIF((SELECT name FROM contacts WHERE jid=?1),''), '')",
                    params![jid],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_default();
            Ok(name)
        })
        .await?
    }

    /// Returns the stored media proto for a message, if any.
    pub async fn get_media(&self, chat_jid: String, id: String) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let media = guard
                .query_row(
                    "SELECT media FROM messages WHERE chat_jid=?1 AND id=?2",
                    params![chat_jid, id],
                    |r| r.get::<_, Option<Vec<u8>>>(0),
                )
                .optional()?
                .flatten();
            Ok(media)
        })
        .await?
    }

    /// Loads the most recent `limit` messages of a chat, returned oldest-first.
    pub async fn load_messages(&self, chat_jid: String, limit: i64) -> Result<Vec<MessageRow>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MessageRow>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // Resolve the sender's display name (saved name → pushname) for group
            // author labels; empty → the UI falls back to the number. Order by
            // (ts,id) — the same key the backfill cursor pages on (load_messages_before).
            let mut stmt = guard.prepare(
                "SELECT x.id, COALESCE(slm.pn, x.sender_jid) AS sender_jid,
                        COALESCE(NULLIF(cm.saved_name,''), NULLIF(ct.name,''),
                                 NULLIF(scm2.saved_name,''), NULLIF(sct2.name,''), '') AS sender_name,
                        x.from_me, x.ts, x.body, x.status,
                        (x.media_kind=3 OR (x.media_kind=0 AND x.media IS NOT NULL)) AS audio,
                        x.audio_secs, x.audio_waveform, x.reply_text,
                        COALESCE(rlm.pn, x.reply_sender) AS reply_sender_canon,
                        COALESCE(NULLIF(rcm.saved_name,''), NULLIF(rct.name,''),
                                 NULLIF(rcm2.saved_name,''), NULLIF(rct2.name,''), '') AS reply_sender_name,
                        x.media_kind, x.media_mime, x.media_name, x.media_thumb, x.media_size
                 FROM (
                   SELECT id, sender_jid, from_me, ts, body, status, media, audio_secs, audio_waveform, reply_text, reply_sender,
                          media_kind, media_mime, media_name, media_thumb, media_size
                   FROM messages WHERE chat_jid=?1
                   ORDER BY ts DESC, id DESC LIMIT ?2
                 ) x
                 LEFT JOIN chat_meta cm ON cm.jid = x.sender_jid
                 LEFT JOIN contacts  ct ON ct.jid = x.sender_jid
                 LEFT JOIN lid_map  slm ON slm.lid = x.sender_jid
                 LEFT JOIN chat_meta scm2 ON scm2.jid = slm.pn
                 LEFT JOIN contacts  sct2 ON sct2.jid = slm.pn
                 LEFT JOIN lid_map  rlm ON rlm.lid = x.reply_sender
                 LEFT JOIN chat_meta rcm ON rcm.jid = x.reply_sender
                 LEFT JOIN contacts  rct ON rct.jid = x.reply_sender
                 LEFT JOIN chat_meta rcm2 ON rcm2.jid = rlm.pn
                 LEFT JOIN contacts  rct2 ON rct2.jid = rlm.pn
                 ORDER BY x.ts ASC, x.id ASC",
            )?;
            let rows = stmt
                .query_map(params![chat_jid, limit], |r| {
                    Ok(MessageRow {
                        id: r.get(0)?,
                        chat_jid: chat_jid.clone(),
                        sender_jid: r.get(1)?,
                        sender_name: r.get(2)?,
                        from_me: r.get::<_, i64>(3)? != 0,
                        ts: r.get(4)?,
                        body: r.get(5)?,
                        status: r.get::<_, i64>(6)? as i32,
                        audio: r.get::<_, i64>(7)? != 0,
                        audio_secs: r.get::<_, Option<i64>>(8)?.unwrap_or(0) as u32,
                        audio_waveform: r.get::<_, Option<Vec<u8>>>(9)?.unwrap_or_default(),
                        reply_text: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                        reply_sender_name: {
                            let has_quote = r
                                .get::<_, Option<String>>(10)?
                                .map(|s| !s.is_empty())
                                .unwrap_or(false);
                            if !has_quote {
                                String::new()
                            } else {
                                let resolved: String = r.get(12)?;
                                if resolved.is_empty() {
                                    r.get::<_, Option<String>>(11)?
                                        .map(|j| preview::pretty_number(&j))
                                        .unwrap_or_default()
                                } else {
                                    resolved
                                }
                            }
                        },
                        media_kind: r.get::<_, i64>(13)? as i32,
                        media_mime: r.get::<_, Option<String>>(14)?.unwrap_or_default(),
                        media_name: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                        media_thumb: r.get::<_, Option<Vec<u8>>>(16)?.unwrap_or_default(),
                        media_size: r.get::<_, i64>(17)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Loads the page of `limit` messages immediately older than the keyset cursor
    /// `(before_ts, before_id)`, returned oldest-first. Used for scroll-up backfill;
    /// an empty result means the local history has been exhausted.
    pub async fn load_messages_before(
        &self,
        chat_jid: String,
        before_ts: i64,
        before_id: String,
        limit: i64,
    ) -> Result<Vec<MessageRow>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MessageRow>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            // Keyset pagination on (ts,id): strictly older than the cursor, matching
            // the (ts DESC, id DESC) ordering used by load_messages.
            let mut stmt = guard.prepare(
                "SELECT x.id, COALESCE(slm.pn, x.sender_jid) AS sender_jid,
                        COALESCE(NULLIF(cm.saved_name,''), NULLIF(ct.name,''),
                                 NULLIF(scm2.saved_name,''), NULLIF(sct2.name,''), '') AS sender_name,
                        x.from_me, x.ts, x.body, x.status,
                        (x.media_kind=3 OR (x.media_kind=0 AND x.media IS NOT NULL)) AS audio,
                        x.audio_secs, x.audio_waveform, x.reply_text,
                        COALESCE(rlm.pn, x.reply_sender) AS reply_sender_canon,
                        COALESCE(NULLIF(rcm.saved_name,''), NULLIF(rct.name,''),
                                 NULLIF(rcm2.saved_name,''), NULLIF(rct2.name,''), '') AS reply_sender_name,
                        x.media_kind, x.media_mime, x.media_name, x.media_thumb, x.media_size
                 FROM (
                   SELECT id, sender_jid, from_me, ts, body, status, media, audio_secs, audio_waveform, reply_text, reply_sender,
                          media_kind, media_mime, media_name, media_thumb, media_size
                   FROM messages
                   WHERE chat_jid=?1 AND (ts < ?2 OR (ts = ?2 AND id < ?3))
                   ORDER BY ts DESC, id DESC LIMIT ?4
                 ) x
                 LEFT JOIN chat_meta cm ON cm.jid = x.sender_jid
                 LEFT JOIN contacts  ct ON ct.jid = x.sender_jid
                 LEFT JOIN lid_map  slm ON slm.lid = x.sender_jid
                 LEFT JOIN chat_meta scm2 ON scm2.jid = slm.pn
                 LEFT JOIN contacts  sct2 ON sct2.jid = slm.pn
                 LEFT JOIN lid_map  rlm ON rlm.lid = x.reply_sender
                 LEFT JOIN chat_meta rcm ON rcm.jid = x.reply_sender
                 LEFT JOIN contacts  rct ON rct.jid = x.reply_sender
                 LEFT JOIN chat_meta rcm2 ON rcm2.jid = rlm.pn
                 LEFT JOIN contacts  rct2 ON rct2.jid = rlm.pn
                 ORDER BY x.ts ASC, x.id ASC",
            )?;
            let rows = stmt
                .query_map(params![chat_jid, before_ts, before_id, limit], |r| {
                    Ok(MessageRow {
                        id: r.get(0)?,
                        chat_jid: chat_jid.clone(),
                        sender_jid: r.get(1)?,
                        sender_name: r.get(2)?,
                        from_me: r.get::<_, i64>(3)? != 0,
                        ts: r.get(4)?,
                        body: r.get(5)?,
                        status: r.get::<_, i64>(6)? as i32,
                        audio: r.get::<_, i64>(7)? != 0,
                        audio_secs: r.get::<_, Option<i64>>(8)?.unwrap_or(0) as u32,
                        audio_waveform: r.get::<_, Option<Vec<u8>>>(9)?.unwrap_or_default(),
                        reply_text: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                        reply_sender_name: {
                            let has_quote = r
                                .get::<_, Option<String>>(10)?
                                .map(|s| !s.is_empty())
                                .unwrap_or(false);
                            if !has_quote {
                                String::new()
                            } else {
                                let resolved: String = r.get(12)?;
                                if resolved.is_empty() {
                                    r.get::<_, Option<String>>(11)?
                                        .map(|j| preview::pretty_number(&j))
                                        .unwrap_or_default()
                                } else {
                                    resolved
                                }
                            }
                        },
                        media_kind: r.get::<_, i64>(13)? as i32,
                        media_mime: r.get::<_, Option<String>>(14)?.unwrap_or_default(),
                        media_name: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                        media_thumb: r.get::<_, Option<Vec<u8>>>(16)?.unwrap_or_default(),
                        media_size: r.get::<_, i64>(17)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }

    /// Returns every `chat_meta` row as (jid, archived, pinned, muted_until,
    /// saved_name). Used by the LID↔PN reconcile to re-key entries under both
    /// the phone-number and LID forms.
    pub async fn all_chat_meta(&self) -> Result<Vec<ChatMetaRow>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<ChatMetaRow>> {
            let guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            let mut stmt = guard
                .prepare("SELECT jid,archived,pinned,muted_until,saved_name FROM chat_meta")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, i64>(2)? != 0,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?
    }
}
