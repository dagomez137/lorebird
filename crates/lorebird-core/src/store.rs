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
}