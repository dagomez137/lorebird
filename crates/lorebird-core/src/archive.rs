//! Archiving: hide a whole series from filtered views while keeping it in
//! All Mail.
//!
//! Archiving records message ids in the `archived` table (see [`schema`]).
//! The query layer excludes them from filtered searches (`query::search`),
//! but `store::load_all_messages` (used by All Mail) ignores the table, so
//! archived mail stays observable there.
//!
//! Archiving operates on a *series*: every message whose subject matches the
//! series key, across all editions. This is how a list like the inbox gets
//! cleared without per-thread multi-select.
//!
//! [`schema`]: crate::schema

use rusqlite::{params, Connection, Result as SqlResult};

/// FTS5 phrase match for a series key, scoped to the subject column.
fn subject_match(series_key: &str) -> String {
    format!("subject:\"{}\"", series_key.replace('"', "\"\""))
}

/// Archive every message whose subject matches `series_key`.
///
/// Returns the number of newly archived messages (already-archived ones are
/// ignored).
pub fn archive_series(conn: &Connection, series_key: &str) -> SqlResult<usize> {
    let m = subject_match(series_key);
    conn.execute(
        "INSERT OR IGNORE INTO archived (message_id)
         SELECT message_id FROM mail_fts WHERE mail_fts MATCH ?1",
        params![m],
    )
}

/// Load the full set of archived message ids.
///
/// The `archived` table is small (only explicitly archived series), so the
/// in-memory query path loads it once and excludes matches by membership —
/// cheaper and equivalent to the `NOT IN (SELECT ... FROM archived)` SQL.
pub fn load_archived_ids(conn: &Connection) -> SqlResult<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT message_id FROM archived")?;
    let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut set = std::collections::HashSet::new();
    for id in ids {
        set.insert(id?);
    }
    Ok(set)
}

/// Unarchive every message whose subject matches `series_key`.
///
/// Returns the number of messages removed from the archive.
pub fn unarchive_series(conn: &Connection, series_key: &str) -> SqlResult<usize> {
    let m = subject_match(series_key);
    conn.execute(
        "DELETE FROM archived WHERE message_id IN
         (SELECT message_id FROM mail_fts WHERE mail_fts MATCH ?1)",
        params![m],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn seed(conn: &Connection, id: &str, subject: &str) {
        conn.execute(
            "INSERT INTO mail_fts (message_id, subject) VALUES (?1, ?2)",
            params![id, subject],
        )
        .unwrap();
    }

    #[test]
    fn archive_and_unarchive_series() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        seed(&conn, "a@x", "[GIT PULL] nvme updates for Linux 7.2");
        seed(&conn, "b@x", "Re: [GIT PULL] nvme updates for Linux 7.1");
        seed(&conn, "c@x", "[PATCH] something unrelated");

        let n = archive_series(&conn, "nvme updates for Linux").unwrap();
        assert_eq!(n, 2);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM archived", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let removed = unarchive_series(&conn, "nvme updates for Linux").unwrap();
        assert_eq!(removed, 2);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM archived", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
