//! Query functions for loading indexed messages from SQLite.
//!
//! These bridge the gap between the raw `mail_ndx` rows and the
//! `thread::Message` trait — providing lightweight structs that
//! implement the trait and can be fed into `thread_messages()`.

use rusqlite::{Connection, Result as SqlResult};

use crate::thread;

/// A lightweight message loaded from `mail_ndx`, suitable for
/// threading and display. Implements [`thread::Message`].
///
/// For the full message body and all headers, load the raw file
/// from the maildir using [`read_raw_message`].
#[derive(Debug, Clone)]
pub struct DbMessage {
    pub message_id: Option<String>,
    pub references: Vec<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub date: Option<String>,
    pub received_ts: i64,
    pub filename: String,
}

impl thread::Message for DbMessage {
    fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    fn references(&self) -> &[String] {
        &self.references
    }

    fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    fn received_ts(&self) -> i64 {
        self.received_ts
    }
}

/// Load all indexed messages from `mail_ndx`, ordered by
/// `received_ts` ascending (oldest first).
///
/// This is the primary input to [`thread::thread_messages`].
pub fn load_all_messages(conn: &Connection) -> SqlResult<Vec<DbMessage>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, refs, subject, from_addr, date, received_ts, filename
         FROM mail_ndx
         ORDER BY received_ts ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        let message_id: Option<String> = row.get(0)?;
        let refs_str: Option<String> = row.get(1)?;
        let references: Vec<String> = refs_str
            .as_deref()
            .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
            .unwrap_or_default();
        Ok(DbMessage {
            message_id,
            references,
            subject: row.get(2)?,
            from_addr: row.get(3)?,
            date: row.get(4)?,
            received_ts: row.get(5)?,
            filename: row.get(6)?,
        })
    })?;

    rows.collect()
}

/// A message loaded from `mail_ndx` including the columns the GTK query
/// worker filters on in memory (subject/from/to/cc/list_id/date/refs/...).
///
/// This is a superset of [`DbMessage`] and implements [`thread::Message`]
/// identically, so it can be threaded directly. The extra fields
/// (`to_addr`, `cc_addr`, `list_id`) are only populated for rows indexed
/// after the schema migration; pre-migration rows leave them `None`.
#[derive(Debug, Clone)]
pub struct CachedMessage {
    pub message_id: Option<String>,
    pub references: Vec<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addr: Option<String>,
    pub cc_addr: Option<String>,
    /// Normalised inner list id (e.g. `linux-block.vger.kernel.org`).
    pub list_id: Option<String>,
    pub date: Option<String>,
    pub received_ts: i64,
    pub filename: String,
}

impl thread::Message for CachedMessage {
    fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }
    fn references(&self) -> &[String] {
        &self.references
    }
    fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }
    fn received_ts(&self) -> i64 {
        self.received_ts
    }
}

/// Outcome of loading the recent working set for in-memory filtering.
pub struct RecentCache {
    pub messages: Vec<CachedMessage>,
    /// Whether the in-memory filter path is usable: the schema has the
    /// filter columns AND a meaningful fraction of the loaded rows have a
    /// populated `list_id`. When `false`, the caller must fall back to the
    /// FTS `search` path (the rows predate the schema migration / re-index).
    pub fast_path_ok: bool,
}

/// Load the most recent `limit` messages with the in-memory-filter columns.
///
/// Detects whether the fast (in-memory) path is viable:
/// - the migrated columns (`list_id`/`to_addr`/`cc_addr`) must exist, and
/// - at least one loaded row must have a non-NULL `list_id` (otherwise the
///   rows predate the re-index and list/addr filters would silently match
///   nothing — better to fall back to FTS).
///
/// On the legacy schema this loads the available columns and reports
/// `fast_path_ok = false`.
pub fn load_recent_cached(conn: &Connection, limit: usize) -> SqlResult<RecentCache> {
    let has_cols = crate::schema::mail_ndx_has_column(conn, "list_id")?
        && crate::schema::mail_ndx_has_column(conn, "to_addr")?
        && crate::schema::mail_ndx_has_column(conn, "cc_addr")?;

    let sql = if has_cols {
        "SELECT message_id, refs, subject, from_addr, date, received_ts, filename,
                list_id, to_addr, cc_addr
         FROM mail_ndx
         ORDER BY received_ts DESC
         LIMIT ?1"
    } else {
        "SELECT message_id, refs, subject, from_addr, date, received_ts, filename
         FROM mail_ndx
         ORDER BY received_ts DESC
         LIMIT ?1"
    };

    let mut stmt = conn.prepare(sql)?;
    let mut any_list_id = false;
    let rows = stmt.query_map([limit as i64], |row| {
        let refs_str: Option<String> = row.get(1)?;
        let references: Vec<String> = refs_str
            .as_deref()
            .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
            .unwrap_or_default();
        let (list_id, to_addr, cc_addr) = if has_cols {
            (row.get(7)?, row.get(8)?, row.get(9)?)
        } else {
            (None, None, None)
        };
        Ok(CachedMessage {
            message_id: row.get(0)?,
            references,
            subject: row.get(2)?,
            from_addr: row.get(3)?,
            to_addr,
            cc_addr,
            list_id,
            date: row.get(4)?,
            received_ts: row.get(5)?,
            filename: row.get(6)?,
        })
    })?;

    let mut messages = Vec::new();
    for m in rows {
        let m = m?;
        if m.list_id.is_some() {
            any_list_id = true;
        }
        messages.push(m);
    }

    let fast_path_ok = has_cols && any_list_id;
    Ok(RecentCache { messages, fast_path_ok })
}

/// Load the most recent `limit` indexed messages from `mail_ndx`,
/// ordered by `received_ts` descending (newest first).
///
/// This relies on the `idx_mail_ndx_received_ts` index so it reads only
/// `limit` rows rather than scanning the whole table — used by the GTK
/// query worker to cap its in-memory threading working set on very large
/// indexes.  The returned order does not matter for JWZ threading (which
/// is order-independent; siblings are sorted afterward).
pub fn load_recent_messages(conn: &Connection, limit: usize) -> SqlResult<Vec<DbMessage>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, refs, subject, from_addr, date, received_ts, filename
         FROM mail_ndx
         ORDER BY received_ts DESC
         LIMIT ?1",
    )?;

    let rows = stmt.query_map([limit as i64], |row| {
        let message_id: Option<String> = row.get(0)?;
        let refs_str: Option<String> = row.get(1)?;
        let references: Vec<String> = refs_str
            .as_deref()
            .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
            .unwrap_or_default();
        Ok(DbMessage {
            message_id,
            references,
            subject: row.get(2)?,
            from_addr: row.get(3)?,
            date: row.get(4)?,
            received_ts: row.get(5)?,
            filename: row.get(6)?,
        })
    })?;

    rows.collect()
}

/// Load messages matching a list of message IDs (e.g. from a search).
///
/// Results are ordered by `received_ts` ascending.
pub fn load_messages_by_ids(
    conn: &Connection,
    ids: &[String],
) -> SqlResult<Vec<DbMessage>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // Build a parameterised IN clause
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
    let sql = format!(
        "SELECT message_id, refs, subject, from_addr, date, received_ts, filename
         FROM mail_ndx
         WHERE message_id IN ({})
         ORDER BY received_ts ASC",
        placeholders.join(",")
    );

    let mut stmt = conn.prepare(&sql)?;
    let params = ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect::<Vec<_>>();
    let rows = stmt.query_map(params.as_slice(), |row| {
        let message_id: Option<String> = row.get(0)?;
        let refs_str: Option<String> = row.get(1)?;
        let references: Vec<String> = refs_str
            .as_deref()
            .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
            .unwrap_or_default();
        Ok(DbMessage {
            message_id,
            references,
            subject: row.get(2)?,
            from_addr: row.get(3)?,
            date: row.get(4)?,
            received_ts: row.get(5)?,
            filename: row.get(6)?,
        })
    })?;

    rows.collect()
}

/// Read a raw message file from the maildir, parse it, and return
/// the full [`crate::message::MailMessage`].
///
/// `filename` is the relative path stored in `mail_ndx.filename`,
/// rooted at the maildir directory.  The indexer strips maildir flags
/// (`:2,S` etc.) to produce a stable basename, but the actual file on
/// disk includes those flags.  This function handles that by doing
/// prefix matching when the exact name isn't found.
pub fn read_raw_message(
    maildir_path: &std::path::Path,
    filename: &str,
) -> Option<crate::message::MailMessage> {
    let full_path = maildir_path.join(filename);

    // The filename in mail_ndx is the stable base name without flags.
    // The actual file might be in cur/ or new/ with flags appended
    // (e.g. "0029...:2,S").  Try exact match first, then prefix search.
    let base = full_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(filename);

    for subdir in &["cur", "new"] {
        let candidate = maildir_path.join(subdir).join(base);

        // 1. Try the exact stable name (works if no flags are present)
        if candidate.exists() {
            if let Ok(raw) = std::fs::read(&candidate) {
                return crate::message::MailMessage::from_bytes(&raw);
            }
        }

        // 2. The actual file may have ":2,FLAGS" appended.  Scan the
        //    directory for files whose base name starts with `base`.
        if let Ok(entries) = std::fs::read_dir(maildir_path.join(subdir)) {
            for entry in entries.flatten() {
                let entry_name = entry.file_name();
                let entry_str = entry_name.to_string_lossy();
                if entry_str.starts_with(base) {
                    if let Ok(raw) = std::fs::read(&entry.path()) {
                        return crate::message::MailMessage::from_bytes(&raw);
                    }
                }
            }
        }
    }

    // Fallback: try the path as-is (might already include subdir + flags)
    if full_path.exists() {
        if let Ok(raw) = std::fs::read(&full_path) {
            return crate::message::MailMessage::from_bytes(&raw);
        }
    }

    None
}

/// Read a raw message from the maildir and extract ALL headers
/// as a `HashMap<String, String>`.
///
/// This is used by the reply compose flow to give the `on_reply`
/// hook access to every header from the original message — no
/// blocklisting, no filtering.
pub fn read_raw_headers(
    maildir_path: &std::path::Path,
    filename: &str,
) -> Option<std::collections::HashMap<String, String>> {
    let raw = read_raw_bytes(maildir_path, filename)?;
    let msg = mail_parser::MessageParser::default().parse(&raw)?;
    let headers: std::collections::HashMap<String, String> = msg
        .headers_raw()
        .map(|(name, value)| (name.to_string(), value.trim().to_string()))
        .collect();
    Some(headers)
}

/// Read the raw bytes of a message file from the maildir.
///
/// Handles the same filename-resolution logic as `read_raw_message`.
pub fn read_raw_bytes(maildir_path: &std::path::Path, filename: &str) -> Option<Vec<u8>> {
    let full_path = maildir_path.join(filename);
    let base = full_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(filename);

    for subdir in &["cur", "new"] {
        let candidate = maildir_path.join(subdir).join(base);

        if candidate.exists() {
            if let Ok(raw) = std::fs::read(&candidate) {
                return Some(raw);
            }
        }

        if let Ok(entries) = std::fs::read_dir(maildir_path.join(subdir)) {
            for entry in entries.flatten() {
                let entry_name = entry.file_name();
                let entry_str = entry_name.to_string_lossy();
                if entry_str.starts_with(base) {
                    if let Ok(raw) = std::fs::read(&entry.path()) {
                        return Some(raw);
                    }
                }
            }
        }
    }

    if full_path.exists() {
        std::fs::read(&full_path).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_all_messages_empty() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_db(&conn).unwrap();
        let msgs = load_all_messages(&conn).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn load_messages_by_ids_empty() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_db(&conn).unwrap();
        let msgs = load_messages_by_ids(&conn, &[]).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn load_recent_cached_reports_columns_and_fast_path() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_db(&conn).unwrap();
        // Row with a list_id → fast path becomes viable.
        conn.execute(
            "INSERT INTO mail_ndx
                (message_id, refs, subject, from_addr, date, received_ts, filename,
                 list_id, to_addr, cc_addr)
             VALUES ('a@b', '', 'Hi', 'x@y', NULL, 100, 'f1',
                     'linux-block.vger.kernel.org', 'to@z', NULL)",
            [],
        )
        .unwrap();
        let rc = load_recent_cached(&conn, 100).unwrap();
        assert_eq!(rc.messages.len(), 1);
        assert!(rc.fast_path_ok);
        assert_eq!(rc.messages[0].list_id.as_deref(), Some("linux-block.vger.kernel.org"));
    }

    #[test]
    fn load_recent_cached_no_list_id_disables_fast_path() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO mail_ndx
                (message_id, refs, subject, from_addr, date, received_ts, filename)
             VALUES ('a@b', '', 'Hi', 'x@y', NULL, 100, 'f1')",
            [],
        )
        .unwrap();
        let rc = load_recent_cached(&conn, 100).unwrap();
        assert_eq!(rc.messages.len(), 1);
        // Columns exist (init_db added them) but no row has a list_id.
        assert!(!rc.fast_path_ok);
    }

    #[test]
    fn load_recent_cached_legacy_schema_falls_back() {
        let conn = Connection::open_in_memory().unwrap();
        // Pre-migration schema (no filter columns), and do NOT migrate.
        conn.execute_batch(
            "CREATE TABLE mail_ndx (
                 message_id TEXT PRIMARY KEY,
                 refs TEXT, subject TEXT, from_addr TEXT, date TEXT,
                 received_ts INTEGER, filename TEXT NOT NULL UNIQUE
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_ndx
                (message_id, refs, subject, from_addr, date, received_ts, filename)
             VALUES ('a@b', '', 'Hi', 'x@y', NULL, 100, 'f1')",
            [],
        )
        .unwrap();
        let rc = load_recent_cached(&conn, 100).unwrap();
        assert_eq!(rc.messages.len(), 1);
        assert!(!rc.fast_path_ok);
    }
}