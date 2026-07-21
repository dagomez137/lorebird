//! Application state — database connection, config, thread data.
//!
//! `AppState` is the shared mutable state that the GUI reads and writes.
//! It holds the SQLite connection (main-thread reads), the root
//! list-store of `ThreadNode`s, and a handle to the background Lua
//! thread.  See `specs/threading.md` for the full architecture.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

use gio::prelude::ListModelExt;
use gio::ListStore;
use rusqlite::Connection;

use crate::lua_thread::{InitResult, LuaCommand, LuaResult, LuaThread};
use crate::query_thread::{PlainNode, QueryCommand, QueryResult, QueryThread};
use crate::thread_node::ThreadNode;
use lorebird_core::follows::Follow;
use lorebird_lua::ResolvedProfile;

/// Describes an in-flight query request so the poller can produce the
/// right status text once the result arrives.
#[derive(Clone)]
pub enum PendingDesc {
    AllMail { profile: String },
    View { name: String, profile: String },
    Search,
    ShowAll,
    Fetch { indexed_count: usize },
}

/// Outcome of applying one (possibly partial) query result to the list.
pub struct ApplyOutcome {
    /// Status-bar text to show.
    pub status: String,
    /// True for the first batch (the list was replaced) — scroll to top.
    pub first: bool,
    /// True for the final batch — the query is complete.
    pub done: bool,
}

/// Central application state, shared between the window and action callbacks.
pub struct AppState {
    /// SQLite connection for reads (main thread only).
    pub db: RefCell<Option<Connection>>,

    /// Path to the DB's associated maildir (empty = no DB open).
    pub db_maildir: RefCell<PathBuf>,

    /// Root list-store backing the `ListView` tree.
    pub root_model: ListStore,

    /// The profile label currently active in the sidebar.
    pub active_profile: RefCell<String>,

    /// Path to the maildir of the currently active profile.
    pub active_maildir: RefCell<PathBuf>,

    /// Optional active view query.
    pub active_query: RefCell<Option<String>>,

    /// Handle to the background Lua thread (owns Vm + LoadedConfig).
    pub lua_thread: LuaThread,

    /// Handle to the background query thread (owns a read-only DB connection).
    pub query_thread: QueryThread,

    /// Monotonic generation counter for query requests.  Results whose
    /// generation differs from this are stale and discarded.
    query_generation: Cell<u64>,

    /// Describes the most recently dispatched query (for status text).
    pending_desc: RefCell<Option<PendingDesc>>,

    /// Resolved profiles (keyed by label), snapshot from config.
    pub profiles: HashMap<String, ResolvedProfile>,

    /// Whether the global on_reply hook is present.
    pub has_on_reply: bool,

    /// Whether the global on_send hook is present.
    pub has_on_send: bool,

    /// Theme preference from config: "light" or "dark".
    pub theme: String,

    /// UI scale factor from config (default 1.0).  Multiplied against
    /// the GTK Xft DPI to adjust for HiDPI / broken environments.
    pub ui_scale: f64,

    /// Show full From/To/Cc headers by default (config `expand_headers`).
    pub expand_headers: bool,

    /// Default reading-pane width in monospace columns (config
    /// `reading_pane_columns`).
    pub reading_pane_columns: usize,

    /// Tighten the thread list rows (config `compact_list`).
    pub compact_list: bool,

    /// Address groups coloured as pills in the recipient fields
    /// (config `contact_groups`).
    pub contact_groups: Vec<lorebird_lua::ContactGroup>,

    /// Optional external editor for the compose body (config `editor`).
    pub editor: Option<lorebird_lua::EditorConfig>,

    /// Followed series (persisted to follows.json), shown in the sidebar
    /// and optionally merged into the inbox view.
    pub follows: RefCell<Vec<Follow>>,

    /// Whether the currently active view is the inbox (so follow changes
    /// can re-run it).
    active_is_inbox: Cell<bool>,
}

impl AppState {
    /// Create a new `AppState` by spawning the Lua thread and
    /// receiving the resolved profiles from it.
    pub fn new(config_path: Option<&std::path::Path>) -> Self {
        let lua_thread = LuaThread::spawn(config_path.map(|p| p.to_path_buf()));

        let init = match lua_thread.recv_init() {
            Ok(init) => init,
            Err(e) => {
                eprintln!("[lorebird] warning: {}", e);
                InitResult {
                    profiles: HashMap::new(),
                    theme: "light".to_string(),
                    ui_scale: 1.0,
                    working_set_limit: lorebird_lua::DEFAULT_WORKING_SET_LIMIT,
                    expand_headers: false,
                    reading_pane_columns: lorebird_lua::DEFAULT_READING_PANE_COLUMNS,
                    compact_list: false,
                    contact_groups: Vec::new(),
                    editor: None,
                    has_on_reply: false,
                    has_on_send: false,
                }
            }
        };

        Self {
            db: RefCell::new(None),
            db_maildir: RefCell::new(PathBuf::new()),
            root_model: ListStore::new::<ThreadNode>(),
            active_profile: RefCell::new(String::new()),
            active_maildir: RefCell::new(PathBuf::new()),
            active_query: RefCell::new(None),
            lua_thread,
            query_thread: QueryThread::spawn(init.working_set_limit),
            query_generation: Cell::new(0),
            pending_desc: RefCell::new(None),
            profiles: init.profiles,
            has_on_reply: init.has_on_reply,
            has_on_send: init.has_on_send,
            theme: init.theme,
            ui_scale: init.ui_scale,
            expand_headers: init.expand_headers,
            reading_pane_columns: init.reading_pane_columns,
            compact_list: init.compact_list,
            contact_groups: init.contact_groups,
            editor: init.editor,
            follows: RefCell::new(lorebird_core::follows::load()),
            active_is_inbox: Cell::new(false),
        }
    }

    /// Add a followed series and persist.
    pub fn add_follow(&self, follow: Follow) {
        self.follows.borrow_mut().push(follow);
        if let Err(e) = lorebird_core::follows::save(&self.follows.borrow()) {
            eprintln!("[lorebird] could not save follows: {}", e);
        }
    }

    /// Remove the followed series with the given query and persist.
    pub fn remove_follow(&self, query: &str) {
        self.follows.borrow_mut().retain(|f| f.query != query);
        if let Err(e) = lorebird_core::follows::save(&self.follows.borrow()) {
            eprintln!("[lorebird] could not save follows: {}", e);
        }
    }

    /// Archive the series of `subject` (all editions) plus every message id in
    /// `thread_ids` (the selected thread). The subject match alone misses a
    /// patch series' siblings, whose subjects differ, so the thread's own ids
    /// are archived too. Returns the number of newly archived messages.
    pub fn archive_series(&self, subject: &str, thread_ids: &[String]) -> Result<usize, String> {
        let key = lorebird_core::series::series_key(subject);
        if key.is_empty() && thread_ids.is_empty() {
            return Err("could not derive a series from this subject".to_string());
        }
        let db = self.db.borrow();
        let conn = db.as_ref().ok_or("no index open")?;
        let mut n = 0;
        if !key.is_empty() {
            n += lorebird_core::archive::archive_series(conn, &key).map_err(|e| e.to_string())?;
        }
        if !thread_ids.is_empty() {
            n += lorebird_core::archive::archive_message_ids(conn, thread_ids)
                .map_err(|e| e.to_string())?;
        }
        Ok(n)
    }

    /// Unarchive the series of `subject` plus `thread_ids`. Returns the number
    /// removed.
    pub fn unarchive_series(&self, subject: &str, thread_ids: &[String]) -> Result<usize, String> {
        let key = lorebird_core::series::series_key(subject);
        if key.is_empty() && thread_ids.is_empty() {
            return Err("could not derive a series from this subject".to_string());
        }
        let db = self.db.borrow();
        let conn = db.as_ref().ok_or("no index open")?;
        let mut n = 0;
        if !key.is_empty() {
            n += lorebird_core::archive::unarchive_series(conn, &key).map_err(|e| e.to_string())?;
        }
        if !thread_ids.is_empty() {
            n += lorebird_core::archive::unarchive_message_ids(conn, thread_ids)
                .map_err(|e| e.to_string())?;
        }
        Ok(n)
    }

    /// Archive many threads' series in one DB transaction. `items` is
    /// (subject, thread_ids) per selected top-level thread. Each subject is
    /// mapped to its series key; entries whose key and ids are both empty are
    /// skipped. Returns the total newly archived count. Callers run a single
    /// [`rerun_active_view`](Self::rerun_active_view) afterwards.
    // Wired to the multi-select bulk action UI in a follow-up commit.
    #[allow(dead_code)]
    pub fn archive_series_bulk(
        &self,
        items: &[(String, Vec<String>)],
    ) -> Result<usize, String> {
        let prepared = prepare_bulk_items(items);
        if prepared.is_empty() {
            return Ok(0);
        }
        let mut db = self.db.borrow_mut();
        let conn = db.as_mut().ok_or("no index open")?;
        lorebird_core::archive::archive_series_bulk(conn, &prepared).map_err(|e| e.to_string())
    }

    /// Unarchive many threads' series in one DB transaction. Symmetric to
    /// [`archive_series_bulk`](Self::archive_series_bulk).
    #[allow(dead_code)]
    pub fn unarchive_series_bulk(
        &self,
        items: &[(String, Vec<String>)],
    ) -> Result<usize, String> {
        let prepared = prepare_bulk_items(items);
        if prepared.is_empty() {
            return Ok(0);
        }
        let mut db = self.db.borrow_mut();
        let conn = db.as_mut().ok_or("no index open")?;
        lorebird_core::archive::unarchive_series_bulk(conn, &prepared).map_err(|e| e.to_string())
    }

    /// Re-dispatch the currently active view/search so the list reflects a
    /// change (e.g. after archiving). Falls back to All Mail.
    pub fn rerun_active_view(&self) -> Result<(), String> {
        let q = self.active_query.borrow().clone();
        match q {
            Some(q) => self.request_search(q, PendingDesc::Search),
            None => self.request_load_all(PendingDesc::ShowAll),
        }
    }

    /// Whether `query` is already followed.
    pub fn is_followed(&self, query: &str) -> bool {
        self.follows.borrow().iter().any(|f| f.query == query)
    }

    /// OR the inbox-flagged follows into a base inbox query.
    pub fn augment_inbox_query(&self, base: &str) -> String {
        let extra: Vec<String> = self
            .follows
            .borrow()
            .iter()
            .filter(|f| f.in_inbox)
            .map(|f| format!("({})", f.query))
            .collect();
        if extra.is_empty() {
            base.to_string()
        } else {
            format!("({}) OR {}", base, extra.join(" OR "))
        }
    }

    pub fn set_active_is_inbox(&self, v: bool) {
        self.active_is_inbox.set(v);
    }

    pub fn active_is_inbox(&self) -> bool {
        self.active_is_inbox.get()
    }

    /// The base query of the inbox view in the active profile, if any.
    pub fn inbox_base_query(&self) -> Option<String> {
        let profile = self.active_profile.borrow().clone();
        let profile = self.profiles.get(&profile)?;
        profile
            .views
            .iter()
            .find(|v| v.inbox)
            .map(|v| v.query.clone())
    }

    /// Select a profile by label.
    pub fn select_profile(&self, label: &str) {
        if let Some(profile) = self.profiles.get(label) {
            *self.active_profile.borrow_mut() = label.to_string();
            *self.active_maildir.borrow_mut() = profile.maildir.clone();
            *self.active_query.borrow_mut() = None;

            let current_db_dir = self.db_maildir.borrow().clone();
            if current_db_dir != profile.maildir {
                *self.db.borrow_mut() = None;
            }
        }
    }

    /// Select a view within the current profile.
    pub fn select_view(&self, query: String) {
        *self.active_query.borrow_mut() = Some(query);
    }

    /// Bump and return the current query generation.
    fn bump_generation(&self) -> u64 {
        let g = self.query_generation.get().wrapping_add(1);
        self.query_generation.set(g);
        g
    }

    /// Dispatch a "load all messages" request to the query worker.
    /// Returns immediately; the result is delivered via `poll_query_result`.
    pub fn request_load_all(&self, desc: PendingDesc) -> Result<(), String> {
        let maildir = self.active_maildir.borrow().clone();
        if maildir.as_os_str().is_empty() {
            return Err("no profile selected".to_string());
        }
        let generation = self.bump_generation();
        *self.pending_desc.borrow_mut() = Some(desc);
        self.query_thread
            .send(QueryCommand::LoadAll { generation, maildir })
    }

    /// Dispatch a search request to the query worker.
    /// Returns immediately; the result is delivered via `poll_query_result`.
    pub fn request_search(&self, query: String, desc: PendingDesc) -> Result<(), String> {
        let maildir = self.active_maildir.borrow().clone();
        if maildir.as_os_str().is_empty() {
            return Err("no profile selected".to_string());
        }
        let generation = self.bump_generation();
        *self.pending_desc.borrow_mut() = Some(desc);
        self.query_thread.send(QueryCommand::Search {
            generation,
            maildir,
            query,
        })
    }

    /// Poll the query worker for a completed result (non-blocking).
    pub fn poll_query_result(&self) -> Option<QueryResult> {
        self.query_thread.try_recv()
    }

    /// Apply a query result on the main thread.  Stale results (from a
    /// superseded request) are ignored.  Returns `Some(status_text)` when
    /// the result was current and applied, `None` when it was stale.
    pub fn apply_query_result(&self, result: &QueryResult) -> Option<ApplyOutcome> {
        if result.generation() != self.query_generation.get() {
            return None;
        }
        let desc = self.pending_desc.borrow().clone();
        match result {
            QueryResult::Error { message, .. } => Some(ApplyOutcome {
                status: format!("Error: {}", message),
                first: true,
                done: true,
            }),
            QueryResult::Tree { roots, match_count, append, done, .. } => {
                // First batch replaces the list; later batches append, so the
                // newest results stay visible while the rest stream in.
                if !append {
                    self.root_model.remove_all();
                }
                for p in roots {
                    self.root_model.append(&build_node_from_plain(p));
                }
                let status = if *done {
                    format_status(desc.as_ref(), *match_count)
                } else {
                    format!("Loading\u{2026} {} so far", self.root_model.n_items())
                };
                Some(ApplyOutcome { status, first: !append, done: *done })
            }
        }
    }

    /// List saved drafts for the active profile into the thread model.
    pub fn show_drafts(&self) -> Result<usize, String> {
        let maildir = self.active_maildir.borrow().clone();
        if maildir.as_os_str().is_empty() {
            return Err("no profile selected".to_string());
        }
        let drafts_dir = maildir.join("Drafts");

        self.root_model.remove_all();
        let mut count = 0;
        for path in lorebird_core::maildir::list_drafts(&drafts_dir) {
            let Ok(raw) = std::fs::read(&path) else {
                continue;
            };
            let Some(m) = lorebird_core::message::MailMessage::from_bytes(&raw) else {
                continue;
            };

            let subject = m.subject.unwrap_or_else(|| "(no subject)".to_string());
            let from = m.from_addr.unwrap_or_default();
            let to = m.to_addr.unwrap_or_default();
            let cc = m.cc_addr.unwrap_or_default();
            let message_id = m.message_id.unwrap_or_default();
            let refs = m.references.join(" ");
            let irt = m.in_reply_to.unwrap_or_default();
            let date = m.date_rfc3339.unwrap_or_default();
            let ts = m.received_ts;
            let rel = format_relative_time(ts);
            let rel_filename = path
                .strip_prefix(&maildir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            let node = ThreadNode::new(
                &subject, &from, &to, &cc, &rel, &rel, ts, ts, &message_id, &refs, &irt, &date,
                &rel_filename,
            );
            node.set_body_preview(m.body_text.unwrap_or_default());
            self.root_model.append(&node);
            count += 1;
        }
        Ok(count)
    }

    /// Open (or create) the index database for `maildir`.
    pub fn open_db(&self, maildir: &std::path::Path) -> Result<(), String> {
        let db_path = maildir.join(".lorebird.db");
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("cannot open database: {}", e))?;
        lorebird_core::schema::init_db(&conn)
            .map_err(|e| format!("cannot init schema: {}", e))?;
        *self.db.borrow_mut() = Some(conn);
        *self.db_maildir.borrow_mut() = maildir.to_path_buf();
        Ok(())
    }

    /// Index the active maildir and rebuild the thread tree.
    /// Used by the **Index** button (synchronous, main thread).
    /// Dispatch a Fetch command to the Lua thread (non-blocking).
    pub fn request_fetch(&self) -> Result<(), String> {
        let profile = self.active_profile.borrow().clone();
        let maildir = self.active_maildir.borrow().clone();
        if profile.is_empty() {
            return Err("no profile selected".to_string());
        }
        self.lua_thread
            .send(LuaCommand::Fetch { profile_label: profile, maildir })
            .map_err(|e| format!("failed to send fetch command: {}", e))
    }

    /// Check if the Lua thread has a result (non-blocking).
    pub fn poll_fetch_result(&self) -> Option<LuaResult> {
        self.lua_thread.try_recv().ok()
    }

    /// Handle a completed fetch result on the main thread.  Re-opens the
    /// DB (cheap) and dispatches a worker request to rebuild the list with
    /// the freshly-indexed data.  The list itself is repopulated
    /// asynchronously by the query poller.
    pub fn handle_fetch_result(&self, result: &LuaResult) -> Result<(), String> {
        match result {
            LuaResult::FetchDone { profile_label: _, indexed_count, error } => {
                if let Some(e) = error {
                    return Err(e.clone());
                }

                let maildir = self.active_maildir.borrow().clone();
                if maildir.as_os_str().is_empty() {
                    return Err("no profile selected".to_string());
                }

                // Re-open the main-thread DB to pick up newly-indexed data.
                *self.db.borrow_mut() = None;
                self.open_db(&maildir)?;

                // The worker's cached threading is now stale — drop it so the
                // post-fetch query rebuilds from the updated index.
                let _ = self.query_thread.send(QueryCommand::InvalidateCache);

                let desc = PendingDesc::Fetch { indexed_count: *indexed_count };
                let query = self.active_query.borrow().clone();
                if let Some(q) = query {
                    self.request_search(q, desc)
                } else {
                    self.request_load_all(desc)
                }
            }
            LuaResult::InitDone { .. } | LuaResult::InitFailed { .. } => {
                // Init results are handled synchronously in AppState::new()
                Err("unexpected init result in fetch handler".to_string())
            }
            LuaResult::FetchProgress { .. }
            | LuaResult::ReplyDone { .. }
            | LuaResult::SendDone { .. } => {
                // FetchProgress is non-terminal and drained by the poller; it
                // must never reach the terminal handler.
                Err("unexpected result in fetch handler".to_string())
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Map each `(subject, ids)` to `(series_key, ids)`, dropping entries whose
/// key and ids are both empty (nothing to act on).
#[allow(dead_code)]
fn prepare_bulk_items(items: &[(String, Vec<String>)]) -> Vec<(String, Vec<String>)> {
    items
        .iter()
        .filter_map(|(subject, ids)| {
            let key = lorebird_core::series::series_key(subject);
            if key.is_empty() && ids.is_empty() {
                None
            } else {
                Some((key, ids.clone()))
            }
        })
        .collect()
}

/// Build a `ThreadNode` GObject tree from a worker-produced `PlainNode`.
/// Rich fields (To/Cc/body/in-reply-to) are left empty and filled lazily
/// from disk when the message is previewed.
fn build_node_from_plain(p: &PlainNode) -> ThreadNode {
    let node = ThreadNode::new(
        &p.subject,
        &p.from,
        "",
        "",
        &p.started,
        &p.last_reply,
        p.started_ts,
        p.last_reply_ts,
        &p.message_id,
        &p.references_str,
        "",
        &p.date_str,
        &p.filename,
    );
    for child in &p.children {
        node.add_child(&build_node_from_plain(child));
    }
    node
}

/// Format the status-bar text for a completed query, given the pending
/// description and (for searches) the match count.
fn format_status(desc: Option<&PendingDesc>, match_count: Option<usize>) -> String {
    match desc {
        Some(PendingDesc::AllMail { profile }) => format!("All mail for: {}", profile),
        Some(PendingDesc::View { name, profile }) => format!(
            "View \u{2018}{}\u{2019} in: {} \u{2014} {} match(es)",
            name,
            profile,
            match_count.unwrap_or(0)
        ),
        Some(PendingDesc::Search) => format!(
            "Found {} matching message(s) in thread(s)",
            match_count.unwrap_or(0)
        ),
        Some(PendingDesc::ShowAll) => "Showing all threads".to_string(),
        Some(PendingDesc::Fetch { indexed_count }) => {
            if *indexed_count == 0 {
                "Fetch succeeded \u{2014} no new mail".to_string()
            } else {
                format!("Fetched & indexed {} new messages", indexed_count)
            }
        }
        None => "Done".to_string(),
    }
}

/// Map a fetch progress event to a progress-bar fraction and text.
///
/// The bar is partitioned into three weighted segments: fetch [0.0, 0.7],
/// index [0.7, 0.9], and the query-worker rebuild [0.9, 1.0] (driven
/// separately by the query poller). Within a segment the fraction is `None`
/// when the denominator is unknown (bare `lorefetch`), signalling the caller
/// to pulse the bar instead.
pub fn fetch_progress_fraction(
    phase: crate::lua_thread::FetchPhase,
    step: usize,
    total: Option<usize>,
) -> Option<f64> {
    use crate::lua_thread::FetchPhase;
    match phase {
        FetchPhase::Fetch => total.and_then(|n| {
            (n > 0).then(|| 0.7 * (step.min(n) as f64) / n as f64)
        }),
        FetchPhase::Index => total.and_then(|n| {
            (n > 0).then(|| 0.7 + 0.2 * (step.min(n) as f64) / n as f64)
        }),
    }
}

/// Build the status-bar text for one fetch progress event: a short
/// description of the work plus the step counter. The index phase already
/// carries a friendly label; only the fetch phase gets a query description.
pub fn describe_fetch_progress(
    phase: crate::lua_thread::FetchPhase,
    step: usize,
    total: Option<usize>,
    label: &str,
) -> String {
    use crate::lua_thread::FetchPhase;
    match phase {
        FetchPhase::Fetch => {
            let what = describe_query(label);
            match total {
                Some(n) => format!("{what} ({step}/{n})\u{2026}"),
                None => format!("{what} (step {step})\u{2026}"),
            }
        }
        FetchPhase::Index => label.to_string(),
    }
}

/// Turn a fetch query into a short human description for the status bar.
///
/// Classifies by the leading field prefix: `l:` yields the list name, and the
/// author (`f:`) and correspondence (`a:`) feeds get a generic label. Anything
/// unrecognised falls back to a truncated echo of the raw query so the text
/// stays bounded even without a description.
pub fn describe_query(query: &str) -> String {
    let q = query.trim().trim_start_matches('(').trim_start();
    let lower = q.to_ascii_lowercase();

    if let Some(rest) = lower.strip_prefix("l:").or_else(|| lower.strip_prefix("list:")) {
        let list = rest.split_whitespace().next().unwrap_or("");
        let short = list.split('.').next().unwrap_or(list);
        if !short.is_empty() {
            return format!("Fetching {short}");
        }
    }
    if lower.starts_with("f:") || lower.starts_with("from:") {
        return "Fetching author mail".to_string();
    }
    if lower.starts_with("a:") || lower.starts_with("addr:") {
        return "Fetching correspondence".to_string();
    }

    const MAX: usize = 40;
    if q.chars().count() > MAX {
        let mut s: String = q.chars().take(MAX).collect();
        s.push('\u{2026}');
        s
    } else {
        q.to_string()
    }
}

pub(crate) fn format_relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let diff = now - ts;
    if diff < 0 { return "just now".to_string(); }
    let mins = diff / 60;
    let hours = diff / 3600;
    let days = diff / 86400;
    let weeks = diff / (7 * 86400);
    if weeks > 0 { format!("{}w ago", weeks) }
    else if days > 0 { format!("{}d ago", days) }
    else if hours > 0 { format!("{}h ago", hours) }
    else if mins > 0 { format!("{}m ago", mins) }
    else { "just now".to_string() }
}

#[cfg(test)]
mod tests {
    use super::describe_query;

    #[test]
    fn describe_query_names_the_list() {
        assert_eq!(
            describe_query("l:linux-modules.vger.kernel.org AND rt:6.months.ago.."),
            "Fetching linux-modules"
        );
    }

    #[test]
    fn describe_query_generic_author_and_addr() {
        assert_eq!(describe_query("f:(hch@lst.de OR hch@sgi.com)"), "Fetching author mail");
        assert_eq!(describe_query("a:(da.gomez@kernel.org)"), "Fetching correspondence");
    }

    #[test]
    fn describe_query_truncates_unrecognised() {
        let long = "s:some very long subject phrase that keeps going well past the limit";
        let got = describe_query(long);
        assert!(got.chars().count() <= 41, "truncated to about 40 chars plus ellipsis");
        assert!(got.ends_with('\u{2026}'));
    }
}