//! Background query worker thread.
//!
//! The heavy read work — loading the recent messages and JWZ-threading them —
//! runs here on a dedicated thread that owns its own read-only SQLite
//! connection, keeping the GTK main thread responsive.
//!
//! The threaded view is cached per maildir (rebuilt after a re-index) so only
//! the first query pays the load+thread cost; later view clicks reuse it.
//! Results are ordered newest-first and streamed to the UI in batches so the
//! latest threads render immediately on large result sets.
//!
//! The worker produces `Send`-able [`PlainNode`] trees built purely from the
//! indexed `CachedMessage` rows (no per-message disk reads). Rich fields
//! (body) are read lazily from disk when a message is previewed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use rusqlite::{Connection, OpenFlags};

use crate::app_state::format_relative_time;
use lorebird_core::store::CachedMessage;
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
    /// Drop the cached threaded view (e.g. after a re-index) so the next
    /// query rebuilds it from the updated database.
    InvalidateCache,
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
        /// `false` for the first batch (replace the list), `true` for
        /// subsequent batches (append).
        append: bool,
        /// `true` on the final batch of a result.
        done: bool,
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

/// Cached, fully-threaded view of one maildir's recent messages.
struct Cache {
    maildir: std::path::PathBuf,
    threads: Vec<Thread<CachedMessage>>,
    index: HashMap<String, usize>,
    /// Whether the in-memory filter path is viable for this cache (schema
    /// migrated and `list_id` populated). When `false`, searches fall back
    /// to the slow FTS `query::search` path so views still work pre-re-index.
    fast_path_ok: bool,
}

/// Threads are streamed to the UI in batches of this size so the first
/// (newest) results render immediately even on a large result set.
const BATCH_SIZE: usize = 300;

/// How many of the most recent messages are loaded and threaded. Threading
/// itself is cheap (~tens of ms for 100k); this bounds the row-load time from
/// the on-disk index (~1s for 50k). Tune up for more history at the cost of a
/// slower first query. Older mail stays on disk but isn't shown in views.
const WORKING_SET_LIMIT: usize = 50_000;

fn query_thread_main(cmd_rx: mpsc::Receiver<QueryCommand>, result_tx: mpsc::Sender<QueryResult>) {
    let mut cache: Option<Cache> = None;
    // A command pulled off the channel while streaming a previous result
    // (so we can abandon stale work and process the newer request).
    let mut pending: Option<QueryCommand> = None;

    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => match cmd_rx.recv() {
                Ok(c) => c,
                Err(_) => break, // main thread gone
            },
        };

        match cmd {
            QueryCommand::Shutdown => break,
            QueryCommand::InvalidateCache => cache = None,
            QueryCommand::LoadAll { generation, maildir } => {
                if let Err(message) = prepare_cache(&mut cache, &maildir) {
                    let _ = result_tx.send(QueryResult::Error { generation, message });
                    continue;
                }
                let c = cache.as_ref().unwrap();
                let order = ordered_roots(&c.threads, None);
                stream_batches(&result_tx, &cmd_rx, &mut pending, generation, &c.threads, &order, None);
            }
            QueryCommand::Search { generation, maildir, query } => {
                let parsed = match lorebird_core::query::parse_query(&query) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = result_tx.send(QueryResult::Error {
                            generation,
                            message: format!("bad query '{}': {:?}", query, e),
                        });
                        continue;
                    }
                };
                if let Err(message) = prepare_cache(&mut cache, &maildir) {
                    let _ = result_tx.send(QueryResult::Error { generation, message });
                    continue;
                }
                let c = cache.as_ref().unwrap();
                match matched_roots(&maildir, &parsed, c) {
                    Ok((order, match_count)) => stream_batches(
                        &result_tx, &cmd_rx, &mut pending, generation, &c.threads, &order, Some(match_count),
                    ),
                    Err(message) => {
                        let _ = result_tx.send(QueryResult::Error { generation, message });
                    }
                }
            }
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

/// (Re)build the threaded cache for `maildir` if it isn't already current.
fn prepare_cache(cache: &mut Option<Cache>, maildir: &Path) -> Result<(), String> {
    let current = matches!(cache, Some(c) if c.maildir == maildir);
    if !current {
        let conn = open_ro(maildir)?;
        let recent = lorebird_core::store::load_recent_cached(&conn, WORKING_SET_LIMIT)
            .map_err(|e| format!("query failed: {}", e))?;
        let fast_path_ok = recent.fast_path_ok;
        let threads = lorebird_core::thread::thread_messages(recent.messages);
        let index = lorebird_core::thread::build_thread_index(&threads);
        *cache = Some(Cache {
            maildir: maildir.to_path_buf(),
            threads,
            index,
            fast_path_ok,
        });
    }
    Ok(())
}

/// Newest activity timestamp anywhere in a thread (used for ordering).
fn thread_latest_ts(t: &Thread<CachedMessage>) -> i64 {
    let mut ts = t.message.as_ref().map(|m| m.received_ts).unwrap_or(i64::MIN);
    for child in &t.children {
        ts = ts.max(thread_latest_ts(child));
    }
    ts
}

/// Root thread indices, newest-first. `keep`, when given, restricts to the
/// threads that contain a matched message.
///
/// Uses `sort_by_cached_key` so `thread_latest_ts` (a recursive subtree walk)
/// is computed once per thread instead of on every comparison.
fn ordered_roots(threads: &[Thread<CachedMessage>], keep: Option<&HashSet<usize>>) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..threads.len())
        .filter(|i| keep.is_none_or(|k| k.contains(i)))
        .collect();
    // Negate the key to get newest-first while keeping the cached-key fast path.
    idx.sort_by_cached_key(|&i| std::cmp::Reverse(thread_latest_ts(&threads[i])));
    idx
}

/// Collect a cached message's filterable fields for in-memory evaluation.
fn filter_fields(m: &CachedMessage) -> lorebird_core::query::FilterFields<'_> {
    lorebird_core::query::FilterFields {
        subject: m.subject.as_deref(),
        from: m.from_addr.as_deref(),
        to: m.to_addr.as_deref(),
        cc: m.cc_addr.as_deref(),
        list_id: m.list_id.as_deref(),
        received_ts: m.received_ts,
    }
}

/// Walk a thread subtree, calling `f` for every present message.
fn for_each_message(t: &Thread<CachedMessage>, f: &mut impl FnMut(&CachedMessage)) {
    if let Some(m) = t.message.as_ref() {
        f(m);
    }
    for child in &t.children {
        for_each_message(child, f);
    }
}

/// Resolve matching root indices (newest-first) plus the true number of
/// matched messages.
///
/// Fast path (default): evaluate the parsed query against the cached working
/// set entirely in memory — no per-view-switch FTS. Archived messages are
/// excluded by membership in a once-loaded id set (All Mail, which doesn't
/// reach here, still shows archived).
///
/// Fallback path: when the cache predates the schema migration / re-index
/// (`fast_path_ok == false`) or the query needs body text (`b:`/`body:`),
/// run the original FTS `query::search` so results stay correct, just slow.
fn matched_roots(
    maildir: &Path,
    parsed: &lorebird_core::query::Query,
    cache: &Cache,
) -> Result<(Vec<usize>, usize), String> {
    let use_fast_path = cache.fast_path_ok && !lorebird_core::query::needs_body(parsed);

    if use_fast_path {
        let conn = open_ro(maildir)?;
        let archived = lorebird_core::archive::load_archived_ids(&conn)
            .map_err(|e| format!("loading archived set failed: {}", e))?;

        let mut seen: HashSet<usize> = HashSet::new();
        let mut match_count = 0usize;
        for (i, thread) in cache.threads.iter().enumerate() {
            let mut thread_has_match = false;
            for_each_message(thread, &mut |m| {
                // Exclude archived (filtered views hide them).
                if let Some(id) = m.message_id.as_deref() {
                    if archived.contains(id) {
                        return;
                    }
                }
                if lorebird_core::query::matches(parsed, &filter_fields(m)) {
                    match_count += 1;
                    thread_has_match = true;
                }
            });
            if thread_has_match {
                seen.insert(i);
            }
        }
        return Ok((ordered_roots(&cache.threads, Some(&seen)), match_count));
    }

    // ── Fallback: FTS search (pre-migration, or body-text query) ──
    let mq = lorebird_core::query::ParsedQuery::from_ast(parsed, 5000);
    let conn = open_ro(maildir)?;
    let matched_ids = lorebird_core::query::search(&conn, &mq)
        .map_err(|e| format!("search failed: {}", e))?;
    let match_count = matched_ids.len();

    let mut seen: HashSet<usize> = HashSet::new();
    for id in &matched_ids {
        if let Some(&ndx) = cache.index.get(id) {
            seen.insert(ndx);
        }
    }
    Ok((ordered_roots(&cache.threads, Some(&seen)), match_count))
}

/// Build `PlainNode`s for `order` and stream them in batches. Stops early if a
/// newer command arrives (stashing it in `pending`) since the UI discards
/// results from superseded generations anyway.
fn stream_batches(
    result_tx: &mpsc::Sender<QueryResult>,
    cmd_rx: &mpsc::Receiver<QueryCommand>,
    pending: &mut Option<QueryCommand>,
    generation: u64,
    threads: &[Thread<CachedMessage>],
    order: &[usize],
    match_count: Option<usize>,
) {
    let total = order.len();
    if total == 0 {
        let _ = result_tx.send(QueryResult::Tree {
            generation, roots: Vec::new(), match_count, append: false, done: true,
        });
        return;
    }

    let mut sent = 0;
    let mut first = true;
    while sent < total {
        if let Ok(next) = cmd_rx.try_recv() {
            *pending = Some(next);
            return;
        }
        let end = (sent + BATCH_SIZE).min(total);
        let roots: Vec<PlainNode> = order[sent..end]
            .iter()
            .map(|&i| build_plain(&threads[i]))
            .collect();
        sent = end;
        let _ = result_tx.send(QueryResult::Tree {
            generation,
            roots,
            match_count,
            append: !first,
            done: sent >= total,
        });
        first = false;
    }
}

// ── PlainNode construction (no disk reads) ──────────────────────────

fn build_plain(t: &Thread<CachedMessage>) -> PlainNode {
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
fn max_ts(t: &Thread<CachedMessage>) -> i64 {
    let own = t.message.as_ref().map(|m| m.received_ts).unwrap_or(0);
    t.children
        .iter()
        .fold(own, |acc, child| acc.max(max_ts(child)))
}

/// Earliest message timestamp in a subtree, if any message is present.
fn min_ts(t: &Thread<CachedMessage>) -> Option<i64> {
    let own = t.message.as_ref().map(|m| m.received_ts);
    let child_min = t.children.iter().filter_map(min_ts).min();
    match (own, child_min) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Subject of the first message found in a subtree (depth-first).
fn subtree_subject(t: &Thread<CachedMessage>) -> Option<String> {
    if let Some(m) = t.message.as_ref() {
        if let Some(s) = m.subject.as_ref() {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    t.children.iter().find_map(subtree_subject)
}
