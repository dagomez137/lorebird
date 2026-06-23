//! SQLite schema and migrations for the mail index.
//!
//! Two tables:
//! - `mail_ndx` — ordinary table: message metadata for threading and file
//!   location.  Inserted incrementally as new mail arrives.
//! - `mail_fts` — standalone FTS5 virtual table: full-text search across
//!   subject, body, addresses, and date.  No `content=` / no triggers —
//!   rows are inserted directly by the indexer.

use rusqlite::{Connection, Result as SqlResult};

/// Initialise (or migrate) the mail-index schema in `db`.
///
/// Idempotent — safe to call on an existing database.
pub fn init_db(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "\
        -- message metadata for threading and file location
        CREATE TABLE IF NOT EXISTS mail_ndx (
            message_id  TEXT PRIMARY KEY,
            refs         TEXT,   -- space-separated Message-IDs (In-Reply-To appended)
            subject     TEXT,
            from_addr   TEXT,          -- from From: header, for display
            date        TEXT,          -- from Date: header, display only
            received_ts INTEGER,       -- effective Unix epoch (Received, with Date fallback)
            filename    TEXT NOT NULL UNIQUE  -- path relative to maildir root (base name, no flags)
        );

        CREATE INDEX IF NOT EXISTS idx_mail_ndx_received_ts
            ON mail_ndx(received_ts);

        -- standalone FTS5 index (no content=, no triggers)
        CREATE VIRTUAL TABLE IF NOT EXISTS mail_fts USING fts5(
            message_id,
            date,       -- from Date: header
            \"from\",
            subject,
            \"to\",
            cc,
            list_id,    -- from List-Id: header, for per-list views
            body
        );
        ",
    )?;

    Ok(())
}

/// Return the count of messages currently in the database.
pub fn message_count(conn: &Connection) -> SqlResult<i64> {
    conn.query_row("SELECT COUNT(*) FROM mail_ndx", [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_db_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        init_db(&conn).unwrap(); // double-init must not fail
    }

    #[test]
    fn message_count_starts_at_zero() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        assert_eq!(message_count(&conn).unwrap(), 0);
    }
}
