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
",
    )?;

    // ── Migration: add the in-memory-filter columns to mail_ndx ─────────
    // These let the GTK query worker filter the cached working set entirely
    // in memory (list_id / to / cc previously lived only in mail_fts, which
    // cannot be looked up by message_id efficiently). `ALTER TABLE ... ADD
    // COLUMN` is not idempotent (it errors if the column already exists), so
    // probe the existing columns first. Existing rows keep NULL values until
    // a re-index repopulates them; the query worker degrades to the FTS path
    // when these columns are unpopulated (see `query::filterable_columns`).
    add_column_if_missing(conn, "list_id")?;
    add_column_if_missing(conn, "to_addr")?;
    add_column_if_missing(conn, "cc_addr")?;

    conn.execute_batch(
        "\
        CREATE INDEX IF NOT EXISTS idx_mail_ndx_list_id
            ON mail_ndx(list_id);

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

        -- archived message ids: hidden from filtered views (inbox, follows,
        -- saved searches) but still shown in All Mail.
        CREATE TABLE IF NOT EXISTS archived (
            message_id TEXT PRIMARY KEY
        );
        ",
    )?;

    Ok(())
}

/// Whether `mail_ndx` has a column named `col`.
pub fn mail_ndx_has_column(conn: &Connection, col: &str) -> SqlResult<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(mail_ndx)")?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == col {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add `col TEXT` to `mail_ndx` if it isn't already present.
///
/// `ALTER TABLE ... ADD COLUMN` errors if the column exists, so this checks
/// first — keeping `init_db` idempotent across schema versions.
fn add_column_if_missing(conn: &Connection, col: &str) -> SqlResult<()> {
    if !mail_ndx_has_column(conn, col)? {
        conn.execute_batch(&format!("ALTER TABLE mail_ndx ADD COLUMN {} TEXT", col))?;
    }
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

    #[test]
    fn init_db_adds_filter_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        assert!(mail_ndx_has_column(&conn, "list_id").unwrap());
        assert!(mail_ndx_has_column(&conn, "to_addr").unwrap());
        assert!(mail_ndx_has_column(&conn, "cc_addr").unwrap());
    }

    #[test]
    fn init_db_migrates_old_schema() {
        // Simulate the pre-migration schema (no list_id/to_addr/cc_addr).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE mail_ndx (
                 message_id TEXT PRIMARY KEY,
                 refs TEXT, subject TEXT, from_addr TEXT, date TEXT,
                 received_ts INTEGER,
                 filename TEXT NOT NULL UNIQUE
             );",
        )
        .unwrap();
        assert!(!mail_ndx_has_column(&conn, "list_id").unwrap());
        init_db(&conn).unwrap();
        assert!(mail_ndx_has_column(&conn, "list_id").unwrap());
        assert!(mail_ndx_has_column(&conn, "to_addr").unwrap());
        assert!(mail_ndx_has_column(&conn, "cc_addr").unwrap());
        // Idempotent: second call must not fail on already-present columns.
        init_db(&conn).unwrap();
    }
}
