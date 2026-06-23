//! Application state — database connection, config, thread data.
//!
//! `AppState` is the shared mutable state that the GUI reads and writes.
//! It holds the SQLite connection (main-thread reads), the root
//! list-store of `ThreadNode`s, and a handle to the background Lua
//! thread.  See `specs/threading.md` for the full architecture.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

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

/// Central application state, shared between the window and action callbacks.
pub struct AppState {
    /// SQLite connection for reads (main thread only).
    pub db: RefCell<Option<Connection>>,

    /// Path to the DB's associated maildir (empty = no DB open).
    pub db_maildir: RefCell<PathBuf>,

    /// Root list-store backing the `ColumnView` tree.
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
            query_thread: QueryThread::spawn(),
            query_generation: Cell::new(0),
            pending_desc: RefCell::new(None),
            profiles: init.profiles,
            has_on_reply: init.has_on_reply,
            has_on_send: init.has_on_send,
            theme: init.theme,
            ui_scale: init.ui_scale,
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

    /// Archive the entire series of `subject` (all editions). Returns the
    /// number of newly archived messages.
    pub fn archive_series(&self, subject: &str) -> Result<usize, String> {
        let key = lorebird_core::series::series_key(subject);
        if key.is_empty() {
            return Err("could not derive a series from this subject".to_string());
        }
        let db = self.db.borrow();
        let conn = db.as_ref().ok_or("no index open")?;
        lorebird_core::archive::archive_series(conn, &key).map_err(|e| e.to_string())
    }

    /// Unarchive the entire series of `subject`. Returns the number removed.
    pub fn unarchive_series(&self, subject: &str) -> Result<usize, String> {
        let key = lorebird_core::series::series_key(subject);
        if key.is_empty() {
            return Err("could not derive a series from this subject".to_string());
        }
        let db = self.db.borrow();
        let conn = db.as_ref().ok_or("no index open")?;
        lorebird_core::archive::unarchive_series(conn, &key).map_err(|e| e.to_string())
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
    pub fn apply_query_result(&self, result: &QueryResult) -> Option<String> {
        if result.generation() != self.query_generation.get() {
            return None;
        }
        let desc = self.pending_desc.borrow().clone();
        match result {
            QueryResult::Error { message, .. } => Some(format!("Error: {}", message)),
            QueryResult::Tree {
                roots, match_count, ..
            } => {
                self.root_model.remove_all();
                for p in roots {
                    self.root_model.append(&build_node_from_plain(p));
                }
                Some(format_status(desc.as_ref(), *match_count))
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
            LuaResult::ReplyDone { .. } | LuaResult::SendDone { .. } => {
                Err("unexpected result in fetch handler".to_string())
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

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