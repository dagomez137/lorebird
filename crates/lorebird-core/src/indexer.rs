//! Maildir indexer: walks a maildir, identifies new messages, parses them,
//! and inserts into the SQLite index.
//!
//! Uses `mail_parser` (Stalwart Labs) for parsing and walks the filesystem
//! directly — no `maildir` crate needed.
//!
//! The entire indexing operation runs inside a single SQLite transaction
//! for performance.  New-mail detection uses the `filename` UNIQUE
//! constraint on `mail_ndx` via `INSERT OR IGNORE`, avoiding a separate
//! SELECT per message.

use rusqlite::{params, Connection, Result as SqlResult};
use std::collections::HashSet;
use std::path::Path;

use crate::message::MailMessage;
use crate::schema;

/// Strip maildir flags from a filename, leaving the stable base name.
///
/// Maildir filenames look like `1700000000.M123P456.host:2,S`.
/// Everything after `:2` (the flags suffix) can change over time;
/// the part before `:2` is the stable identifier.
fn stable_basename(filename: &str) -> &str {
    filename.split(":2").next().unwrap_or(filename)
}

/// Recursively collect all regular files under `dir`, skipping dotfiles.
fn collect_mail_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            files.extend(collect_mail_files(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
}

/// Index every message in `maildir_path`, inserting rows into `conn`.
///
/// New mail is detected via a `UNIQUE` constraint on `mail_ndx.filename`:
/// `INSERT OR IGNORE` silently skips already-indexed files.  The mail
/// fetcher must guarantee stable base filenames and only write new mail
/// to disk.
///
/// Returns the number of newly inserted messages.
pub fn index_maildir(conn: &Connection, maildir_path: &Path) -> SqlResult<usize> {
    schema::init_db(conn)?;

    // Wrap all inserts in a single transaction — avoids per-row fsync.
    conn.execute_batch("BEGIN")?;
    let result = index_maildir_inner(conn, maildir_path);
    match &result {
        Ok(_) => conn.execute_batch("COMMIT")?,
        Err(_) => { let _ = conn.execute_batch("ROLLBACK"); }
    }
    result
}

fn index_maildir_inner(conn: &Connection, maildir_path: &Path) -> SqlResult<usize> {

    let mut inserted = 0usize;

    // Fast pre-filter: load every already-indexed filename into memory so we
    // can skip the expensive `fs::read` + `MailMessage::from_bytes` parse for
    // files we've seen before.  The `filename` column stores the stable
    // relative path (basename with the maildir `:2,FLAGS` suffix stripped),
    // which is exactly the `rel_path` we reconstruct per file below — so the
    // membership test matches what `INSERT OR IGNORE` would dedup on.
    //
    // This one query is cheap relative to parsing hundreds of thousands of
    // messages.  The `INSERT OR IGNORE` further down remains the authoritative
    // dedup (it still protects against races / correctness); this set is purely
    // a performance pre-filter.  Each file is visited at most once per run, so
    // we don't need to insert newly indexed paths back into the set.
    let mut indexed: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT filename FROM mail_ndx")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for f in rows {
            indexed.insert(f?);
        }
    }

    for subdir in &["cur", "new"] {
        let dir = maildir_path.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        for file_path in collect_mail_files(&dir) {
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let base = stable_basename(file_name);

            // Build relative path using the stable base name (no flags)
            let rel_path = file_path
                .with_file_name(base)
                .strip_prefix(maildir_path)
                .unwrap_or(&file_path)
                .to_string_lossy()
                .to_string();

            // Pre-filter: skip files already indexed under this stable rel_path
            // BEFORE reading + parsing them.  This turns re-indexing cost from
            // O(total maildir size) into O(new files).
            if indexed.contains(&rel_path) {
                continue;
            }

            let raw = match std::fs::read(&file_path) {
                Ok(b) => b,
                Err(_) => continue,
            };

            let Some(msg) = MailMessage::from_bytes(&raw) else {
                continue;
            };

            let Some(ref msg_id) = msg.message_id else {
                continue;
            };

            let refs = msg.references.join(" ");

            // ── Effective timestamp ─────────────────────────────────────
            // Received-TS (from Received: header, set by MTA, UTC) is the
            // primary timestamp.  However, mail_parser sometimes produces
            // garbage (e.g. negative values).  When that happens, fall
            // back to date_ts (from Date: header, user-provided).
            let effective_ts = if msg.received_ts > 946_684_800 {
                // After 2000-01-01 — sane, use it
                msg.received_ts
            } else if msg.date_ts > 946_684_800 {
                // Received is garbage but Date header looks sane
                msg.date_ts
            } else {
                // Both are broken — keep received_ts (better than 0)
                msg.received_ts
            };

            // Normalise List-Id to its inner id (e.g. `linux-block.vger.kernel.org`)
            // so the in-memory query evaluator can do clean equality/contains
            // comparisons. Same normalisation is applied on the query side.
            let list_id_ndx = msg
                .list_id
                .as_deref()
                .map(crate::message::normalize_list_id);

            // ── mail_ndx (INSERT OR IGNORE — filename UNIQUE catches dupes) ──
            // to_addr/cc_addr/list_id are stored here too (not just in mail_fts)
            // so the GTK worker can filter its cached working set in memory.
            let ndx_changes = conn.execute(
                "INSERT OR IGNORE INTO mail_ndx (message_id, refs, subject, from_addr, date, received_ts, filename, list_id, to_addr, cc_addr)\n                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    msg_id,
                    refs,
                    msg.subject,
                    msg.from_addr,
                    msg.date_rfc3339,
                    effective_ts,
                    rel_path,
                    list_id_ndx,
                    msg.to_addr,
                    msg.cc_addr,
                ],
            )?;

            if ndx_changes == 0 {
                continue; // already indexed (duplicate filename)
            }

            // ── mail_fts ──
            conn.execute(
                "INSERT INTO mail_fts (message_id, date, \"from\", subject, \"to\", cc, list_id, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    msg_id,
                    msg.date_rfc3339,
                    msg.from_addr,
                    msg.subject,
                    msg.to_addr,
                    msg.cc_addr,
                    msg.list_id,
                    msg.body_text,
                ],
            )?;

            inserted += 1;
        }
    }

    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_empty_maildir_returns_zero() {
        let conn = Connection::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        for sub in &["cur", "new", "tmp"] {
            std::fs::create_dir_all(tmp.path().join(sub)).unwrap();
        }
        let n = index_maildir(&conn, tmp.path()).unwrap();
        assert_eq!(n, 0);
    }

    /// Write a minimal valid message (needs a Message-ID) into `<maildir>/cur`.
    fn write_msg(maildir: &Path, basename: &str, flags: &str, msg_id: &str) {
        let name = format!("{basename}:2,{flags}");
        let body = format!(
            "From: a@b.com\r\nSubject: Hi\r\nMessage-ID: <{msg_id}>\r\n\r\nhello"
        );
        std::fs::write(maildir.join("cur").join(name), body).unwrap();
    }

    #[test]
    fn prefilter_only_indexes_new_files() {
        let conn = Connection::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        for sub in &["cur", "new", "tmp"] {
            std::fs::create_dir_all(tmp.path().join(sub)).unwrap();
        }

        // First pass: two messages, both new.
        write_msg(tmp.path(), "1700000000.aaa.host", "S", "m1@x");
        write_msg(tmp.path(), "1700000001.bbb.host", "S", "m2@x");
        let n1 = index_maildir(&conn, tmp.path()).unwrap();
        assert_eq!(n1, 2, "first run should index both new messages");

        // Add one genuinely new message, then re-index.
        write_msg(tmp.path(), "1700000002.ccc.host", "S", "m3@x");
        let n2 = index_maildir(&conn, tmp.path()).unwrap();
        assert_eq!(
            n2, 1,
            "second run must index exactly the 1 new file, skipping the 2 already-indexed"
        );

        // mail_ndx holds 3 rows; mail_fts likewise — no duplicates created.
        let ndx_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mail_ndx", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ndx_count, 3);
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mail_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 3);
    }

    /// A flag change on an already-indexed file (same stable basename, new
    /// `:2,FLAGS` suffix) must NOT be re-indexed: the stable rel_path is what
    /// the pre-filter and the UNIQUE constraint both key on.
    #[test]
    fn prefilter_ignores_flag_changes() {
        let conn = Connection::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        for sub in &["cur", "new", "tmp"] {
            std::fs::create_dir_all(tmp.path().join(sub)).unwrap();
        }

        write_msg(tmp.path(), "1700000000.aaa.host", "S", "m1@x");
        assert_eq!(index_maildir(&conn, tmp.path()).unwrap(), 1);

        // Rename to a different flag set (e.g. mark replied) — same stable base.
        std::fs::remove_file(tmp.path().join("cur").join("1700000000.aaa.host:2,S"))
            .unwrap();
        write_msg(tmp.path(), "1700000000.aaa.host", "RS", "m1@x");

        assert_eq!(
            index_maildir(&conn, tmp.path()).unwrap(),
            0,
            "flag-only change keeps the same stable rel_path and must be skipped"
        );
        let ndx_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mail_ndx", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ndx_count, 1);
    }
}
