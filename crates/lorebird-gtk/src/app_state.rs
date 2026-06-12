//! Application state — database connection, config, thread data.
//!
//! `AppState` is the shared mutable state that the GUI reads and writes.
//! It holds the SQLite connection (main-thread reads), the root
//! list-store of `ThreadNode`s, and a handle to the background Lua
//! thread.  See `specs/threading.md` for the full architecture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use gio::ListStore;
use rusqlite::Connection;

use crate::lua_thread::{InitResult, LuaCommand, LuaResult, LuaThread};
use crate::thread_node::ThreadNode;
use lorebird_core::store::DbMessage;
use lorebird_core::thread::{self, Thread};
use lorebird_lua::ResolvedProfile;

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
            profiles: init.profiles,
            has_on_reply: init.has_on_reply,
            has_on_send: init.has_on_send,
            theme: init.theme,
            ui_scale: init.ui_scale,
        }
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

    /// Run a search query and rebuild the thread tree.
    pub fn search(&self, query: &str) -> Result<usize, String> {
        let maildir = self.active_maildir.borrow().clone();
        if maildir.as_os_str().is_empty() {
            return Err("no profile selected".to_string());
        }
        if self.db.borrow().is_none() {
            return Err("no database open — click Index first".to_string());
        }
        self.rebuild_thread_tree_searched(&maildir, query)
    }

    /// Clear search results and show all messages.
    pub fn show_all(&self) -> Result<(), String> {
        let maildir = self.active_maildir.borrow().clone();
        if maildir.as_os_str().is_empty() {
            return Err("no profile selected".to_string());
        }
        if self.db.borrow().is_none() {
            return Err("no database open — click Index first".to_string());
        }
        let db = self.db.borrow();
        let conn = db.as_ref().ok_or("database not open")?;
        self.rebuild_thread_tree(conn, &maildir)
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

    /// Process a completed fetch result on the main thread.
    /// Re-opens the DB and rebuilds the thread tree.
    pub fn handle_fetch_result(&self, result: &LuaResult) -> Result<String, String> {
        match result {
            LuaResult::FetchDone { profile_label: _, indexed_count, error } => {
                if let Some(e) = error {
                    return Err(e.clone());
                }

                let maildir = self.active_maildir.borrow().clone();
                if maildir.as_os_str().is_empty() {
                    return Err("no profile selected".to_string());
                }

                // Re-open DB to pick up new data indexed by the Lua thread
                *self.db.borrow_mut() = None;
                self.open_db(&maildir)?;

                {
                    let db = self.db.borrow();
                    let conn = db.as_ref().ok_or("database not open")?;
                    let query_str = self.active_query.borrow();
                    if let Some(ref q) = *query_str {
                        self.rebuild_thread_tree_searched(&maildir, q)?;
                    } else {
                        self.rebuild_thread_tree(conn, &maildir)?;
                    }
                }

                if *indexed_count == 0 {
                    Ok("Fetch succeeded — no new mail".to_string())
                } else {
                    Ok(format!("Fetched & indexed {} new messages", indexed_count))
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

    /// Rebuild the thread tree from the database (all messages).
    pub fn rebuild_thread_tree(
        &self,
        conn: &Connection,
        maildir: &std::path::Path,
    ) -> Result<(), String> {
        let messages = lorebird_core::store::load_all_messages(conn)
            .map_err(|e| format!("query failed: {}", e))?;
        let threads = thread::thread_messages(messages);

        self.root_model.remove_all();
        let node_tree = ThreadNodeTree::from_threads(&threads, maildir);
        for root_node in &node_tree.roots {
            self.root_model.append(root_node);
        }
        Ok(())
    }

    /// Rebuild thread tree filtered by search query.
    fn rebuild_thread_tree_searched(
        &self,
        maildir: &std::path::Path,
        query: &str,
    ) -> Result<usize, String> {
        let parsed = lorebird_core::query::parse_query(query)
            .map_err(|e| format!("bad query '{}': {:?}", query, e))?;
        let pq = lorebird_core::query::ParsedQuery::from_ast(&parsed, 5000);

        let db = self.db.borrow();
        let conn = db.as_ref().ok_or("database not open")?;

        let matched_ids: Vec<String> = lorebird_core::query::search(conn, &pq)
            .map_err(|e| format!("search failed: {}", e))?;
        let match_count = matched_ids.len();

        let all_messages = lorebird_core::store::load_all_messages(conn)
            .map_err(|e| format!("query failed: {}", e))?;
        let threads = thread::thread_messages(all_messages);

        let thread_index = thread::build_thread_index(&threads);
        let mut seen_threads: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for id in &matched_ids {
            if let Some(&ndx) = thread_index.get(id) {
                seen_threads.insert(ndx);
            }
        }

        self.root_model.remove_all();
        let node_tree = ThreadNodeTree::from_threads_filtered(&threads, maildir, &seen_threads);
        for root_node in &node_tree.roots {
            self.root_model.append(root_node);
        }

        Ok(match_count)
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Intermediate tree of `ThreadNode` GObjects.
struct ThreadNodeTree {
    roots: Vec<ThreadNode>,
}

impl ThreadNodeTree {
    fn from_threads(threads: &[Thread<DbMessage>], maildir: &std::path::Path) -> Self {
        let roots: Vec<ThreadNode> = threads
            .iter()
            .map(|t| Self::build_node(t, maildir))
            .collect();
        Self { roots }
    }

    fn from_threads_filtered(
        threads: &[Thread<DbMessage>],
        maildir: &std::path::Path,
        seen_threads: &std::collections::HashSet<usize>,
    ) -> Self {
        let roots: Vec<ThreadNode> = threads
            .iter()
            .enumerate()
            .filter(|(i, _)| seen_threads.contains(i))
            .map(|(_, t)| Self::build_node(t, maildir))
            .collect();
        Self { roots }
    }

    fn build_node(t: &Thread<DbMessage>, maildir: &std::path::Path) -> ThreadNode {
        let msg = t.message.as_ref();

        // Try to read the full message from disk for rich data.
        // Fall back to the DB row data when the file is missing.
        let parsed = msg.as_ref().and_then(|m| {
            lorebird_core::store::read_raw_message(maildir, &m.filename)
        });

        let (from, to, cc, body, parsed_refs, parsed_date, parsed_mid, parsed_irt) =
            if let Some(ref p) = parsed {
                (
                    p.from_addr.clone().unwrap_or_default(),
                    p.to_addr.clone().unwrap_or_default(),
                    p.cc_addr.clone().unwrap_or_default(),
                    p.body_text.clone().unwrap_or_default(),
                    p.references.join(" "),
                    p.date_rfc3339.clone().unwrap_or_default(),
                    p.message_id.clone().unwrap_or_default(),
                    p.in_reply_to.clone().unwrap_or_default(),
                )
            } else {
                (
                    msg.as_ref()
                        .and_then(|m| m.from_addr.clone())
                        .unwrap_or_default(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                )
            };

        // Fall back to the DB row for fields that parsed may not have.
        let message_id = if parsed_mid.is_empty() {
            msg.as_ref()
                .and_then(|m| m.message_id.clone())
                .unwrap_or_default()
        } else {
            parsed_mid
        };

        let references_str = if parsed_refs.is_empty() {
            msg.as_ref()
                .map(|m| m.references.join(" "))
                .unwrap_or_default()
        } else {
            parsed_refs
        };

        let date_str = if parsed_date.is_empty() {
            msg.as_ref()
                .and_then(|m| m.date.clone())
                .unwrap_or_default()
        } else {
            parsed_date
        };

        let subject = msg
            .as_ref()
            .and_then(|m| m.subject.clone())
            .unwrap_or_else(|| "(no subject)".to_string());

        // started_ts = root message timestamp (or i64::MAX for ghost nodes)
        let started_ts = msg
            .as_ref()
            .map(|m| m.received_ts)
            .unwrap_or(i64::MAX);
        let started = format_relative_time(started_ts);

        // last_reply_ts = most recent timestamp in the whole subtree
        let last_reply_ts = Self::max_ts(t);
        let last_reply = format_relative_time(last_reply_ts);

        let in_reply_to = if parsed_irt.is_empty() {
            String::new()
        } else {
            parsed_irt
        };

        let filename = msg
            .as_ref()
            .map(|m| m.filename.clone())
            .unwrap_or_default();

        let node = ThreadNode::new(
            &subject, &from, &to, &cc,
            &started, &last_reply, started_ts, last_reply_ts,
            &message_id, &references_str, &in_reply_to, &date_str, &filename,
        );
        node.set_body_preview(body);

        for child in &t.children {
            let child_node = Self::build_node(child, maildir);
            node.add_child(&child_node);
        }

        node
    }

    /// Find the most recent timestamp in a thread subtree.
    fn max_ts(t: &Thread<DbMessage>) -> i64 {
        let own = t.message.as_ref().map(|m| m.received_ts).unwrap_or(0);
        t.children
            .iter()
            .fold(own, |acc, child| acc.max(Self::max_ts(child)))
    }
}

fn format_relative_time(ts: i64) -> String {
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