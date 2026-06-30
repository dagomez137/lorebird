//! Lua thread — runs the VM in a dedicated background thread.
//!
//! The Lua thread *creates* and *owns* `Vm` and `LoadedConfig`.  Neither
//! ever crosses a thread boundary.  The main (GTK) thread communicates
//! with it via `mpsc` channels, sending plain-data `LuaCommand`s and
//! receiving `LuaResult`s — all `Send`-compatible.
//!
//! See `specs/threading.md` for the full architecture.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

use lorebird_lua::{LoadedConfig, Mail, ResolvedProfile, Vm};

// ── Command protocol (main → Lua thread) ────────────────────────────

/// Commands sent from the main thread to the Lua thread.
#[derive(Debug)]
pub enum LuaCommand {
    /// Call `on_fetch` for the given profile.
    /// If the hook returns truthy, also index the maildir.
    Fetch {
        profile_label: String,
        maildir: PathBuf,
    },

    /// Call `on_reply` hook (if present) with parent and pre-filled mail.
    /// Returns the possibly-modified mail, or None if the hook returned nil.
    Reply {
        profile_label: String,
        parent: Mail,
        mail: Mail,
    },

    /// Call `on_send` hook with the composed mail.
    Send {
        profile_label: String,
        mail: Mail,
    },

    /// Shut down the Lua thread.
    Shutdown,
}

/// Results sent from the Lua thread back to the main thread.
#[derive(Debug)]
pub enum LuaResult {
    /// Config loaded successfully; here are the resolved profiles.
    InitDone {
        profiles: HashMap<String, ResolvedProfile>,
        theme: String,
        ui_scale: f64,
        working_set_limit: usize,
        expand_headers: bool,
        reading_pane_columns: usize,
        has_on_reply: bool,
        has_on_send: bool,
    },

    /// Config loading failed.
    InitFailed {
        error: String,
    },

    /// Fetch + index operation completed.
    #[allow(dead_code)]
    FetchDone {
        profile_label: String,
        indexed_count: usize,
        error: Option<String>,
    },

    /// Reply hook completed. Carries the possibly-modified mail (or
    /// None if the hook was absent / returned nil).
    ReplyDone {
        mail: Option<Mail>,
        error: Option<String>,
    },

    /// Send hook completed.
    SendDone {
        error: Option<String>,
    },
}

// ── Lua thread state ────────────────────────────────────────────────

/// State kept inside the Lua thread (not `Send`, stays in that thread).
struct LuaState {
    vm: Vm,
    config: LoadedConfig,
    profiles: HashMap<String, ResolvedProfile>,
}

/// Initialisation result from the Lua thread.
pub struct InitResult {
    pub profiles: HashMap<String, ResolvedProfile>,
    pub theme: String,
    pub ui_scale: f64,
    pub working_set_limit: usize,
    pub expand_headers: bool,
    pub reading_pane_columns: usize,
    pub has_on_reply: bool,
    pub has_on_send: bool,
}

// ── Lua thread handle ──────────────────────────────────────────────

/// Handle to the background Lua thread.
pub struct LuaThread {
    cmd_tx: mpsc::Sender<LuaCommand>,
    result_rx: mpsc::Receiver<LuaResult>,
}

impl LuaThread {
    /// Spawn the Lua thread.  It creates the VM, loads config, and
    /// sends `InitDone` (or `InitFailed`) back via the result channel.
    pub fn spawn(lorebird_conf_path: Option<PathBuf>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<LuaCommand>();
        let (result_tx, result_rx) = mpsc::channel::<LuaResult>();

        std::thread::Builder::new()
            .name("lorebird-lua".into())
            .spawn(move || {
                lua_thread_main(lorebird_conf_path, cmd_rx, result_tx);
            })
            .expect("failed to spawn Lua thread");

        Self { cmd_tx, result_rx }
    }

    /// Block until the Lua thread has loaded config.
    pub fn recv_init(&self) -> Result<InitResult, String> {
        match self.result_rx.recv() {
            Ok(LuaResult::InitDone { profiles, theme, ui_scale, working_set_limit, expand_headers, reading_pane_columns, has_on_reply, has_on_send }) => Ok(InitResult { profiles, theme, ui_scale, working_set_limit, expand_headers, reading_pane_columns, has_on_reply, has_on_send }),
            Ok(LuaResult::InitFailed { error }) => Err(error),
            Ok(_) => Err("unexpected result from Lua thread".to_string()),
            Err(_) => Err("Lua thread disconnected during init".to_string()),
        }
    }

    /// Send a command to the Lua thread.
    pub fn send(&self, cmd: LuaCommand) -> Result<(), mpsc::SendError<LuaCommand>> {
        self.cmd_tx.send(cmd)
    }

    /// Try to receive a result (non-blocking).
    pub fn try_recv(&self) -> Result<LuaResult, mpsc::TryRecvError> {
        self.result_rx.try_recv()
    }

    /// Tell the Lua thread to shut down.
    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(LuaCommand::Shutdown);
    }
}

// ── Lua thread main loop ───────────────────────────────────────────

fn lua_thread_main(
    lorebird_conf_path: Option<PathBuf>,
    cmd_rx: mpsc::Receiver<LuaCommand>,
    result_tx: mpsc::Sender<LuaResult>,
) {
    // 1. Create VM and load config (entirely within this thread)
    let state = match load_config(lorebird_conf_path) {
        Ok(state) => {
            eprintln!("[lorebird-lua] loaded {} profile(s)", state.profiles.len());
            let profiles = state.profiles.clone();
            let theme = state.config.config.theme.clone();
            let ui_scale = state.config.config.ui_scale;
            let working_set_limit = state.config.config.working_set_limit;
            let expand_headers = state.config.config.expand_headers;
            let reading_pane_columns = state.config.config.reading_pane_columns;
            let has_on_reply = state.config.global_hooks.on_reply.is_some();
            let has_on_send = state.config.global_hooks.on_send.is_some();
            let _ = result_tx.send(LuaResult::InitDone { profiles, theme, ui_scale, working_set_limit, expand_headers, reading_pane_columns, has_on_reply, has_on_send });
            state
        }
        Err(e) => {
            eprintln!("[lorebird-lua] config load failed: {}", e);
            let _ = result_tx.send(LuaResult::InitFailed { error: e });
            let vm = Vm::new().expect("failed to create Lua VM");
            LuaState {
                vm,
                config: empty_config(),
                profiles: HashMap::new(),
            }
        }
    };

    // 2. Command loop
    loop {
        match cmd_rx.recv() {
            Ok(LuaCommand::Fetch { profile_label, maildir }) => {
                eprintln!(
                    "[lorebird-lua] Fetch: profile='{}' maildir='{}'",
                    profile_label,
                    maildir.display()
                );
                let t = std::time::Instant::now();
                let result = handle_fetch(&state, &profile_label, &maildir);
                eprintln!("[lorebird-lua] Fetch complete in {:?}", t.elapsed());
                let _ = result_tx.send(result);
            }
            Ok(LuaCommand::Reply { profile_label, parent, mail }) => {
                eprintln!(
                    "[lorebird-lua] Reply: profile='{}'",
                    profile_label
                );
                let result = handle_reply(&state, &profile_label, &parent, &mail);
                let _ = result_tx.send(result);
            }
            Ok(LuaCommand::Send { profile_label, mail }) => {
                eprintln!(
                    "[lorebird-lua] Send: profile='{}'",
                    profile_label
                );
                let result = handle_send(&state, &profile_label, &mail);
                let _ = result_tx.send(result);
            }
            Ok(LuaCommand::Shutdown) | Err(_) => {
                eprintln!("[lorebird-lua] thread shutting down");
                return;
            }
        }
    }
}

/// Load config inside the Lua thread.  Returns `LuaState`.
fn load_config(
    lorebird_conf_path: Option<PathBuf>,
) -> Result<LuaState, String> {
    let vm = Vm::new().map_err(|e| format!("failed to create Lua VM: {}", e))?;

    let loaded = match lorebird_conf_path {
        Some(ref path) => vm.load_config_file(path)
            .map_err(|e| format!("failed to load config from {}: {}", path.display(), e))?,
        None => {
            let default = dirs_for_lorebird();
            let cfg_file = default.join("config.lua");
            if cfg_file.exists() {
                vm.load_config_file(&cfg_file)
                    .map_err(|e| format!("failed to load config from {}: {}", cfg_file.display(), e))?
            } else {
                return Err(format!("no config found at {}", cfg_file.display()));
            }
        }
    };

    let profiles = loaded.config.resolve_all();
    Ok(LuaState { vm, config: loaded, profiles })
}

/// Handle a Fetch command: call on_fetch, then index if truthy.
fn handle_fetch(
    state: &LuaState,
    profile_label: &str,
    maildir: &std::path::Path,
) -> LuaResult {
    // 1. Look up the profile hooks
    let hooks = state.config.profile_hooks.get(profile_label);

    // 2. Call on_fetch hook (if defined)
    if let Some(hooks) = hooks {
        eprintln!("[lorebird-lua]   calling on_fetch for '{}'...", profile_label);
        match state.vm.call_on_fetch(profile_label, maildir.to_str().unwrap_or(""), hooks) {
            Ok(true) => {
                eprintln!("[lorebird-lua]   on_fetch returned true — indexing");
            }
            Ok(false) => {
                eprintln!("[lorebird-lua]   on_fetch returned false — no new mail");
                return LuaResult::FetchDone {
                    profile_label: profile_label.to_string(),
                    indexed_count: 0,
                    error: None,
                };
            }
            Err(e) => {
                eprintln!("[lorebird-lua]   on_fetch error: {}", e);
                return LuaResult::FetchDone {
                    profile_label: profile_label.to_string(),
                    indexed_count: 0,
                    error: Some(format!("on_fetch hook failed: {}", e)),
                };
            }
        }
    } else {
        eprintln!(
            "[lorebird-lua]   no on_fetch hook for '{}' — indexing directly",
            profile_label
        );
    }

    // 3. Index the maildir
    eprintln!("[lorebird-lua]   indexing '{}'...", maildir.display());
    let db_path = maildir.join(".lorebird.db");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            return LuaResult::FetchDone {
                profile_label: profile_label.to_string(),
                indexed_count: 0,
                error: Some(format!("cannot open database: {}", e)),
            };
        }
    };

    if let Err(e) = lorebird_core::schema::init_db(&conn) {
        return LuaResult::FetchDone {
            profile_label: profile_label.to_string(),
            indexed_count: 0,
            error: Some(format!("cannot init schema: {}", e)),
        };
    }

    // Enable WAL for concurrent readers
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");

    match lorebird_core::indexer::index_maildir(&conn, maildir) {
        Ok(n) => {
            eprintln!("[lorebird-lua]   indexed {} message(s)", n);
            LuaResult::FetchDone {
                profile_label: profile_label.to_string(),
                indexed_count: n,
                error: None,
            }
        }
        Err(e) => LuaResult::FetchDone {
            profile_label: profile_label.to_string(),
            indexed_count: 0,
            error: Some(format!("indexing failed: {}", e)),
        },
    }
}

fn empty_config() -> LoadedConfig {
    use lorebird_lua::{AppConfig, GlobalHooks, DEFAULT_READING_PANE_COLUMNS, DEFAULT_WORKING_SET_LIMIT};
    LoadedConfig {
        config: AppConfig { user: None, theme: "light".to_string(), ui_scale: 1.0, working_set_limit: DEFAULT_WORKING_SET_LIMIT, expand_headers: false, reading_pane_columns: DEFAULT_READING_PANE_COLUMNS, profiles: HashMap::new() },
        profile_hooks: HashMap::new(),
        global_hooks: GlobalHooks { on_reply: None, on_send: None },
    }
}

fn dirs_for_lorebird() -> PathBuf {
    lorebird_core::config_dir::lorebird_confdir()
        .unwrap_or_else(|| PathBuf::from("/tmp/lorebird"))
}

/// Handle a Reply command: call on_reply hook if present.
///
/// If the hook is absent, return the pre-filled mail unchanged.
/// If the hook is present, it may modify `mail` in-place — we
/// always extract the (potentially modified) result.
fn handle_reply(
    state: &LuaState,
    profile_label: &str,
    parent: &Mail,
    mail: &Mail,
) -> LuaResult {
    let func = match state.config.global_hooks.on_reply.as_ref() {
        Some(f) => f,
        None => {
            eprintln!("[lorebird-lua]   no on_reply hook — using default");
            return LuaResult::ReplyDone {
                mail: None,
                error: None,
            };
        }
    };

    eprintln!("[lorebird-lua]   calling on_reply for '{}'...", profile_label);
    match state.vm.call_on_reply(func, profile_label, parent, mail) {
        Ok(modified) => {
            eprintln!("[lorebird-lua]   on_reply returned modified mail");
            LuaResult::ReplyDone {
                mail: Some(modified),
                error: None,
            }
        }
        Err(e) => {
            eprintln!("[lorebird-lua]   on_reply error: {}", e);
            LuaResult::ReplyDone {
                mail: None,
                error: Some(format!("on_reply hook failed: {}", e)),
            }
        }
    }
}

/// Handle a Send command: call on_send hook.
///
/// The on_send hook is responsible for delivering the mail.
/// It can use `send_smtp()` for built-in SMTP, or `sh()` to pipe
/// to an external MTA. `on_send` is always required — there is no
/// automatic send path. The hook receives the profile's SMTP config
/// via the `_lorebird_smtp` Lua global so that `send_smtp()` works.
fn handle_send(
    state: &LuaState,
    profile_label: &str,
    mail: &Mail,
) -> LuaResult {
    // Look up the profile's resolved SMTP config (for _lorebird_smtp global)
    let smtp = state.profiles.get(profile_label).and_then(|p| p.smtp.as_ref());

    let func = match state.config.global_hooks.on_send.as_ref() {
        Some(f) => f,
        None => {
            eprintln!("[lorebird-lua]   no on_send hook");
            return LuaResult::SendDone {
                error: Some("no on_send hook — define on_send in your config".to_string()),
            };
        }
    };

    // Per-profile Drafts/Sent maildirs for backup-before-send.
    let dirs = state
        .profiles
        .get(profile_label)
        .map(|p| (p.maildir.join("Drafts"), p.maildir.join("Sent")));
    let raw = mail.to_rfc2822();
    let draft_id = mail.draft_id();

    // Autosave the draft BEFORE invoking the hook so it survives a failed send.
    if let Some((ref drafts_dir, _)) = dirs
        && let Err(e) = lorebird_core::maildir::save_draft(drafts_dir, &draft_id, raw.as_bytes())
    {
        eprintln!("[lorebird-lua]   warning: could not save draft: {}", e);
    }

    eprintln!("[lorebird-lua]   calling on_send for '{}'...", profile_label);
    match state.vm.call_on_send(func, profile_label, mail, smtp) {
        Ok(true) => {
            eprintln!("[lorebird-lua]   on_send completed");
            if let Some((ref drafts_dir, ref sent_dir)) = dirs {
                if let Err(e) = lorebird_core::maildir::append_sent(sent_dir, raw.as_bytes()) {
                    eprintln!("[lorebird-lua]   warning: could not save to Sent: {}", e);
                }
                if let Err(e) = lorebird_core::maildir::delete_draft(drafts_dir, &draft_id) {
                    eprintln!("[lorebird-lua]   warning: could not delete draft: {}", e);
                }
            }
            LuaResult::SendDone { error: None }
        }
        Ok(false) => {
            eprintln!("[lorebird-lua]   on_send hook returned false — delivery failed");
            LuaResult::SendDone {
                error: Some("on_send hook returned false — delivery failed".to_string()),
            }
        }
        Err(e) => {
            eprintln!("[lorebird-lua]   on_send error: {}", e);
            LuaResult::SendDone {
                error: Some(format!("on_send hook failed: {}", e)),
            }
        }
    }
}