//! Background query worker thread.
//!
//! The heavy read work — loading every indexed message, JWZ-threading
//! the whole set, and running search queries — runs here on a dedicated
//! thread that owns its own read-only SQLite connection.  This keeps the
//! GTK main thread responsive: profile switches, view clicks, and search
//! never block the UI.
//!
//! The worker produces a `Send`-able [`PlainNode`] tree built purely from
//! the indexed `DbMessage` rows (no per-message disk reads).  The main
//! thread converts those into `ThreadNode` GObjects.  Rich fields
//! (To/Cc/body) are read lazily from disk when a message is previewed.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use rusqlite::{Connection, OpenFlags};

use crate::app_state::format_relative_time;
use lorebird_core::store::DbMessage;
use lorebird_core::thread::Thread;

/// Plain, `Send`-able representation of a thread node produced by the
/// worker.  Rich fields (To/Cc/body/in-reply-to) are intentionally
/// omitted — they are read lazily from disk on selection.
pub struct PlainNode {
    pub subject: String,
    pub from: String,
    pub started: String,
    pub last_reply: String,
    pub started_ts: i64,
    pub last_reply_ts: i64,
    pub message_id: String,
    pub references_str: String,
    pub date_str: String,
    pub filename: String,
    pub children: Vec<PlainNode>,
}

/// A request for the worker.  Each carries a `generation` so the main
/// thread can discard results from superseded requests.
pub enum QueryCommand {
    LoadAll {
        generation: u64,
        maildir: std::path::PathBuf,
    },
    Search {
        generation: u64,
        maildir: std::path::PathBuf,
        query: String,
    },
    Shutdown,
}

/// A result from the worker.
pub enum QueryResult {
    Tree {
        generation: u64,
        roots: Vec<PlainNode>,
        /// `Some(n)` for a search (number of matched messages),
        /// `None` for a plain load-all.
        match_count: Option<usize>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

impl QueryResult {
    pub fn generation(&self) -> u64 {
        match self {
            QueryResult::Tree { generation, .. } => *generation,
            QueryResult::Error { generation, .. } => *generation,
        }
    }
}

/// Handle to the background query thread.
pub struct QueryThread {
    cmd_tx: mpsc::Sender<QueryCommand>,
    result_rx: mpsc::Receiver<QueryResult>,
    handle: Option<thread::JoinHandle<()>>,
}

impl QueryThread {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<QueryCommand>();
        let (result_tx, result_rx) = mpsc::channel::<QueryResult>();
        let handle = thread::Builder::new()
            .name("lorebird-query".to_string())
            .spawn(move || query_thread_main(cmd_rx, result_tx))
            .expect("failed to spawn query thread");
        Self {
            cmd_tx,
            result_rx,
            handle: Some(handle),
        }
    }

    pub fn send(&self, cmd: QueryCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|e| format!("query thread unavailable: {}", e))
    }

    pub fn try_recv(&self) -> Option<QueryResult> {
        self.result_rx.try_recv().ok()
    }

    fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(QueryCommand::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for QueryThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Worker loop ─────────────────────────────────────────────────────

fn query_thread_main(cmd_rx: mpsc::Receiver<QueryCommand>, result_tx: mpsc::Sender<QueryResult>) {
    while let Ok(cmd) = cmd_rx.recv() {
        let out = match cmd {
            QueryCommand::Shutdown => break,
            QueryCommand::LoadAll {
                generation,
                maildir,
            } => match load_all(&maildir) {
                Ok(roots) => QueryResult::Tree {
                    generation,
                    roots,
                    match_count: None,
                },
                Err(message) => QueryResult::Error {
                    generation,
                    message,
                },
            },
            QueryCommand::Search {
                generation,
                maildir,
                query,
            } => match search(&maildir, &query) {
                Ok((roots, n)) => QueryResult::Tree {
                    generation,
                    roots,
                    match_count: Some(n),
                },
                Err(message) => QueryResult::Error {
                    generation,
                    message,
                },
            },
        };
        if result_tx.send(out).is_err() {
            break; // main thread is gone
        }
    }
}

// ── Query implementations ───────────────────────────────────────────

fn open_ro(maildir: &Path) -> Result<Connection, String> {
    let db_path = maildir.join(".lorebird.db");
    Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("cannot open database: {}", e))
}

fn load_all(maildir: &Path) -> Result<Vec<PlainNode>, String> {
    let conn = open_ro(maildir)?;
    let messages = lorebird_core::store::load_all_messages(&conn)
        .map_err(|e| format!("query failed: {}", e))?;
    let threads = lorebird_core::thread::thread_messages(messages);
    Ok(threads.iter().map(build_plain).collect())
}

fn search(maildir: &Path, query: &str) -> Result<(Vec<PlainNode>, usize), String> {
    let parsed = lorebird_core::query::parse_query(query)
        .map_err(|e| format!("bad query '{}': {:?}", query, e))?;
    let pq = lorebird_core::query::ParsedQuery::from_ast(&parsed, 5000);

    let conn = open_ro(maildir)?;
    let matched_ids: Vec<String> =
        lorebird_core::query::search(&conn, &pq).map_err(|e| format!("search failed: {}", e))?;
    let match_count = matched_ids.len();

    let all_messages = lorebird_core::store::load_all_messages(&conn)
        .map_err(|e| format!("query failed: {}", e))?;
    let threads = lorebird_core::thread::thread_messages(all_messages);

    let thread_index = lorebird_core::thread::build_thread_index(&threads);
    let mut seen_threads: HashSet<usize> = HashSet::new();
    for id in &matched_ids {
        if let Some(&ndx) = thread_index.get(id) {
            seen_threads.insert(ndx);
        }
    }

    let roots: Vec<PlainNode> = threads
        .iter()
        .enumerate()
        .filter(|(i, _)| seen_threads.contains(i))
        .map(|(_, t)| build_plain(t))
        .collect();

    Ok((roots, match_count))
}

// ── PlainNode construction (no disk reads) ──────────────────────────

fn build_plain(t: &Thread<DbMessage>) -> PlainNode {
    let msg = t.message.as_ref();

    // Ghost roots borrow the first real descendant's subject.
    let subject = msg
        .and_then(|m| m.subject.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| subtree_subject(t))
        .unwrap_or_else(|| "(no subject)".to_string());

    let from = msg.and_then(|m| m.from_addr.clone()).unwrap_or_default();
    let message_id = msg.and_then(|m| m.message_id.clone()).unwrap_or_default();
    let references_str = msg.map(|m| m.references.join(" ")).unwrap_or_default();
    let date_str = msg.and_then(|m| m.date.clone()).unwrap_or_default();
    let filename = msg.map(|m| m.filename.clone()).unwrap_or_default();

    // Ghost roots sort by their earliest descendant rather than to the end.
    let started_ts = msg
        .map(|m| m.received_ts)
        .or_else(|| min_ts(t))
        .unwrap_or(i64::MAX);
    let started = format_relative_time(started_ts);

    let last_reply_ts = max_ts(t);
    let last_reply = format_relative_time(last_reply_ts);

    let children = t.children.iter().map(build_plain).collect();

    PlainNode {
        subject,
        from,
        started,
        last_reply,
        started_ts,
        last_reply_ts,
        message_id,
        references_str,
        date_str,
        filename,
        children,
    }
}

/// Most recent timestamp in a subtree.
fn max_ts(t: &Thread<DbMessage>) -> i64 {
    let own = t.message.as_ref().map(|m| m.received_ts).unwrap_or(0);
    t.children
        .iter()
        .fold(own, |acc, child| acc.max(max_ts(child)))
}

/// Earliest message timestamp in a subtree, if any message is present.
fn min_ts(t: &Thread<DbMessage>) -> Option<i64> {
    let own = t.message.as_ref().map(|m| m.received_ts);
    let child_min = t.children.iter().filter_map(min_ts).min();
    match (own, child_min) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Subject of the first message found in a subtree (depth-first).
fn subtree_subject(t: &Thread<DbMessage>) -> Option<String> {
    if let Some(m) = t.message.as_ref() {
        if let Some(s) = m.subject.as_ref() {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    t.children.iter().find_map(subtree_subject)
}
