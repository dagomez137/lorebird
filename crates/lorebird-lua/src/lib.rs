//! Lua VM integration for lorebird.
//!
//! Wraps mlua to provide:
//! - Configuration loading (profiles, views, hooks)
//! - `sh()` API helper for running external commands
//! - Hook dispatch (fetch, reply, send)

mod config;

pub use config::{
    AppConfig, ContactGroup, DEFAULT_READING_PANE_COLUMNS, DEFAULT_WORKING_SET_LIMIT, EditorConfig,
    GlobalHooks, LoadedConfig, ProfileData, ProfileHooks, ResolvedProfile, UserInfo, ViewConfig,
};
pub use lorebird_core::compose::Mail;
pub use lorebird_sendmail::{SendError, SmtpConfig};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{DeserializeOptions, IntoLua};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Table, Value};

/// Temp-file counter for unique filenames.
static TMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Fetch progress sink ─────────────────────────────────────────────
//
// A fetch hook (`on_fetch`) loops in Lua over N query strings, calling the
// `lorefetch` / `lorefetch_all` builtins. Those builtins run on the Lua
// thread (the VM is single-threaded) so we route per-query progress through
// a thread-local sink installed for the duration of one fetch. The GTK Lua
// thread installs a sink that forwards each event as a channel message.

/// One progress event emitted by a fetch builtin.
pub enum FetchProgress {
    /// A single query in a fetch batch is about to run. `total` is `Some(n)`
    /// when the batch size is known (`lorefetch_all`) and `None` for the bare
    /// `lorefetch` path, where only a running step counter is available.
    Query {
        step: usize,
        total: Option<usize>,
        label: String,
    },
}

/// A callback that consumes one fetch progress event.
type ProgressSink = Box<dyn Fn(FetchProgress)>;

thread_local! {
    static PROGRESS_SINK: RefCell<Option<ProgressSink>> = const { RefCell::new(None) };
    /// Running step counter for the bare `lorefetch` path, reset on install.
    static PROGRESS_STEP: Cell<usize> = const { Cell::new(0) };
}

/// RAII guard that clears the installed progress sink when dropped.
pub struct ProgressGuard {
    _priv: (),
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        PROGRESS_SINK.with(|s| *s.borrow_mut() = None);
        PROGRESS_STEP.with(|c| c.set(0));
    }
}

/// Install a progress sink for the current thread and reset the step counter.
/// The returned guard clears the sink on drop, so callers should scope it to
/// a single fetch. Only one sink may be installed at a time; installing a new
/// one replaces the old.
pub fn install_progress_sink(f: ProgressSink) -> ProgressGuard {
    PROGRESS_SINK.with(|s| *s.borrow_mut() = Some(f));
    PROGRESS_STEP.with(|c| c.set(0));
    ProgressGuard { _priv: () }
}

/// Emit a progress event to the installed sink, if any. No-op otherwise.
pub fn emit_progress(ev: FetchProgress) {
    PROGRESS_SINK.with(|s| {
        if let Some(sink) = s.borrow().as_ref() {
            sink(ev);
        }
    });
}

/// Increment and return the per-install step counter (1-based).
fn next_progress_step() -> usize {
    PROGRESS_STEP.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    })
}

/// A Lua VM configured with the lorebird API.
pub struct Vm {
    lua: Lua,
    /// Paths to temporary files created by `write_tmpfile`,
    /// cleaned up when the Vm is dropped.
    temp_files: std::cell::RefCell<Vec<std::path::PathBuf>>,
}

impl Vm {
    /// Create a new Lua VM and register the lorebird API surface.
    ///
    /// The following globals are available to Lua scripts:
    ///
    /// - `sh(cmd)` — run an external command, return result table
    /// - `read_file(path)` — read a file's contents as a string
    pub fn new() -> LuaResult<Self> {
        let lua = Lua::new();
        let vm = Self {
            lua,
            temp_files: std::cell::RefCell::new(Vec::new()),
        };
        vm.register_helpers()?;
        Ok(vm)
    }

    // ── API helpers ─────────────────────────────────────────────

    /// Register lorebird API functions as Lua globals.
    fn register_helpers(&self) -> LuaResult<()> {
        // ── sh(cmd [, opts]) → { ok, exit_code, stdout, stderr } ──
        //
        //   cmd:   table of strings  {"prog", "arg1", ...}
        //   opts:  optional table with keys:
        //       stdin       – string, piped to child's stdin
        //       stdin_file  – string path, file piped to child's stdin
        //       env         – table of extra environment variables
        //
        //   stdin takes priority over stdin_file if both are given.
        //
        let sh_fn = self.lua.create_function(
            |lua, (args, opts): (Table, Option<Table>)| {
                let cmd_args: Vec<String> = args
                    .sequence_values::<String>()
                    .collect::<Result<Vec<_>, _>>()?;

                // Extract optional parameters from opts table
                let stdin_data: Option<String> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("stdin").ok().flatten());
                let stdin_file: Option<String> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("stdin_file").ok().flatten());
                let env_table: Option<Table> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<Table>>("env").ok().flatten());

                if cmd_args.is_empty() {
                    let err_table = lua.create_table()?;
                    err_table.set("ok", false)?;
                    err_table.set("exit_code", -1)?;
                    err_table.set("stdout", String::new())?;
                    err_table.set("stderr", "sh: empty command".to_string())?;
                    return Ok(err_table);
                }

                let mut cmd = std::process::Command::new(&cmd_args[0]);
                cmd.args(&cmd_args[1..]);

                // Apply extra environment variables (inherited + overrides)
                if let Some(env_tbl) = &env_table {
                    for pair in env_tbl.pairs::<String, String>() {
                        let (key, value) = pair?;
                        cmd.env(key, value);
                    }
                }

                // Decide stdin mode
                //   stdin (string) > stdin_file (path) > inherit (default)
                if stdin_data.is_some() {
                    cmd.stdin(std::process::Stdio::piped());
                } else if let Some(ref path) = stdin_file {
                    let file = match std::fs::File::open(path) {
                        Ok(f) => f,
                        Err(e) => {
                            let err_table = lua.create_table()?;
                            err_table.set("ok", false)?;
                            err_table.set("exit_code", -1)?;
                            err_table.set("stdout", String::new())?;
                            err_table.set("stderr", format!("sh: cannot open stdin_file '{}': {}", path, e))?;
                            return Ok(err_table);
                        }
                    };
                    cmd.stdin(std::process::Stdio::from(file));
                }

                // Capture stdout + stderr
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());

                let output = match cmd.spawn() {
                    Ok(mut child) => {
                        // Write stdin data if provided
                        if let Some(ref data) = stdin_data {
                            use std::io::Write;
                            if let Some(mut pipe) = child.stdin.take() {
                                let _ = pipe.write_all(data.as_bytes());
                                // Drop pipe to send EOF
                                drop(pipe);
                            }
                        }
                        match child.wait_with_output() {
                            Ok(o) => o,
                            Err(e) => {
                                let err_table = lua.create_table()?;
                                err_table.set("ok", false)?;
                                err_table.set("exit_code", -1)?;
                                err_table.set("stdout", String::new())?;
                                err_table.set("stderr", format!("sh: {}", e))?;
                                return Ok(err_table);
                            }
                        }
                    }
                    Err(e) => {
                        let err_table = lua.create_table()?;
                        err_table.set("ok", false)?;
                        err_table.set("exit_code", -1)?;
                        err_table.set("stdout", String::new())?;
                        err_table.set("stderr", format!("sh: {}", e))?;
                        return Ok(err_table);
                    }
                };

                let result = lua.create_table()?;
                result.set("ok", output.status.success())?;
                result.set("exit_code", output.status.code().unwrap_or(-1))?;
                result.set(
                    "stdout",
                    String::from_utf8_lossy(&output.stdout).to_string(),
                )?;
                result.set(
                    "stderr",
                    String::from_utf8_lossy(&output.stderr).to_string(),
                )?;
                Ok(result)
            },
        )?;

        self.lua.globals().set("sh", sh_fn)?;

        // ── read_file(path) → string ────────────────────────────
        let read_file_fn = self.lua.create_function(|_, path: String| {
            std::fs::read_to_string(&path).map_err(|e| {
                mlua::Error::external(format!("read_file: cannot read '{}': {}", path, e))
            })
        })?;

        self.lua.globals().set("read_file", read_file_fn)?;

        // ── write_tmpfile(content) → path ──────────────────────────
        //   Writes `content` to a unique temporary file and returns
        //   the path.  Files are cleaned up when the Vm is dropped.
        let write_tmpfile_fn = self.lua.create_function(|_lua, content: String| {
            let count = TMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let name = format!("lorebird_{}_{}", pid, count);
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, &content)
                .map_err(|e| mlua::Error::external(format!("write_tmpfile: {}", e)))?;
            Ok(path.to_string_lossy().to_string())
        })?;

        self.lua.globals().set("write_tmpfile", write_tmpfile_fn)?;

        // ── mail_to_rfc2822(mail_table) → string ────────────────────
        //   Converts a mail table (same structure as passed to on_reply /
        //   on_send) to an RFC 2822 formatted string.
        let mail_to_rfc2822_fn = self.lua.create_function(|_lua, table: Table| {
            let mail = extract_mail_from_table(&table)?;
            Ok(mail.to_rfc2822())
        })?;

        self.lua
            .globals()
            .set("mail_to_rfc2822", mail_to_rfc2822_fn)?;

        // ── lorefetch(maildir, query) → { ok, count?, error? } ──────
        //   Fetch mail from lore.kernel.org into a maildir.
        let lorefetch_fn =
            self.lua
                .create_function(|lua, (maildir, query): (String, String)| {
                    // Bare-lorefetch path: the batch size is unknown to Rust,
                    // so we emit a running step counter with `total: None`.
                    let step = next_progress_step();
                    emit_progress(FetchProgress::Query {
                        step,
                        total: None,
                        label: query.clone(),
                    });

                    let result = lorebird_lorefetch::fetch_to_maildir(
                        &query,
                        None, // search /all/ by default
                        std::path::Path::new(&maildir),
                        false, // verbose off by default from Lua
                    );

                    let table = lua.create_table()?;
                    match result {
                        Ok(r) => {
                            table.set("ok", true)?;
                            table.set("count", r.total_messages)?;
                            table.set("new", r.new_messages)?;
                            if r.timed_out {
                                table.set("timed_out", true)?;
                            }
                        }
                        Err(e) => {
                            table.set("ok", false)?;
                            table.set("error", e.to_string())?;
                        }
                    }
                    Ok(table)
                })?;

        self.lua.globals().set("lorefetch", lorefetch_fn)?;

        // ── lorefetch_all(maildir, queries) → { ok, new, failures } ──
        //   Fetch a batch of queries into a maildir. Rust knows the batch
        //   size N up front, so it emits determinate `k/N` progress and a
        //   per-query label. Per-query failures are accumulated and the loop
        //   continues (matching the hand-rolled fetch_all semantics), so any
        //   arrived mail is still indexed. Returns truthy `ok` when new > 0.
        let lorefetch_all_fn =
            self.lua
                .create_function(|lua, (maildir, queries): (String, Value)| {
                    let queries = read_query_list(&queries)?;
                    let total = queries.len();
                    let mut new_total: usize = 0;
                    let mut failures: usize = 0;

                    for (i, query) in queries.iter().enumerate() {
                        emit_progress(FetchProgress::Query {
                            step: i + 1,
                            total: Some(total),
                            label: query.clone(),
                        });
                        match lorebird_lorefetch::fetch_to_maildir(
                            query,
                            None,
                            std::path::Path::new(&maildir),
                            false,
                        ) {
                            Ok(r) => new_total += r.new_messages,
                            Err(_) => failures += 1,
                        }
                    }

                    let table = lua.create_table()?;
                    table.set("ok", new_total > 0)?;
                    table.set("new", new_total)?;
                    table.set("failures", failures)?;
                    Ok(table)
                })?;

        self.lua.globals().set("lorefetch_all", lorefetch_all_fn)?;

        // ── send_smtp(rfc2822_text) → { ok, error? } ────────────────
        //   Send an RFC 2822 message via the profile's SMTP config.
        //   The SMTP config is set as a Lua global (_lorebird_smtp)
        //   by the send dispatch code before calling on_send.
        //   If no config is set, returns { ok=false, error="..." }.
        let send_smtp_fn = self.lua.create_function(|lua, rfc2822: String| {
            let globals = lua.globals();
            let smtp_val: mlua::Value = globals.get("_lorebird_smtp").unwrap_or(mlua::Value::Nil);

            let table = lua.create_table()?;
            match smtp_val {
                mlua::Value::Table(t) => {
                    // Deserialize the smtp table from Lua
                    let options = DeserializeOptions::new().deny_unsupported_types(false);
                    let smtp_config: lorebird_sendmail::SmtpConfig = lua
                        .from_value_with(t.into_lua(lua)?, options)
                        .map_err(|e| {
                            mlua::Error::external(format!("invalid smtp config: {}", e))
                        })?;

                    // Build envelope from the mail headers
                    let parsed = mail_parser::MessageParser::default()
                        .parse(rfc2822.as_bytes())
                        .ok_or_else(|| {
                            mlua::Error::external("send_smtp: failed to parse RFC 2822 message")
                        })?;

                    let from_addr = parsed
                        .from()
                        .and_then(|f| f.first())
                        .and_then(|m| m.address())
                        .map(|a| a.to_string())
                        .unwrap_or_default();

                    let to_addrs: Vec<String> = parsed
                        .to()
                        .map(|addrs| {
                            addrs
                                .iter()
                                .filter_map(|a| a.address())
                                .map(|a| a.to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    let cc_addrs: Vec<String> = parsed
                        .cc()
                        .map(|addrs| {
                            addrs
                                .iter()
                                .filter_map(|a| a.address())
                                .map(|a| a.to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    let bcc_addrs: Vec<String> = parsed
                        .bcc()
                        .map(|addrs| {
                            addrs
                                .iter()
                                .filter_map(|a| a.address())
                                .map(|a| a.to_string())
                                .collect()
                        })
                        .unwrap_or_default();

                    let all_recipients: Vec<&str> = to_addrs
                        .iter()
                        .chain(cc_addrs.iter())
                        .chain(bcc_addrs.iter())
                        .map(|s| s.as_str())
                        .collect();

                    if all_recipients.is_empty() {
                        table.set("ok", false)?;
                        table.set("error", "no recipients found in message")?;
                        return Ok(table);
                    }

                    match lorebird_sendmail::send(
                        &smtp_config,
                        &from_addr,
                        &all_recipients,
                        rfc2822.as_bytes(),
                    ) {
                        Ok(()) => {
                            table.set("ok", true)?;
                        }
                        Err(e) => {
                            table.set("ok", false)?;
                            table.set("error", e.to_string())?;
                        }
                    }
                }
                _ => {
                    table.set("ok", false)?;
                    table.set("error", "no smtp config for current profile")?;
                }
            }
            Ok(table)
        })?;

        self.lua.globals().set("send_smtp", send_smtp_fn)?;

        Ok(())
    }

    // ── Config loading ──────────────────────────────────────────

    /// Load a config file from disk, execute it, and return the
    /// parsed configuration (data) together with extracted hook handles.
    ///
    /// The Lua script must either **return** a config table or set a
    /// global `config` variable.
    pub fn load_config_file(&self, path: &Path) -> LuaResult<LoadedConfig> {
        let code = std::fs::read_to_string(path).map_err(|e| {
            mlua::Error::external(format!(
                "cannot read config file '{}': {}",
                path.display(),
                e
            ))
        })?;
        self.load_config_string(&code)
    }

    /// Execute a Lua config string and return the parsed configuration.
    ///
    /// The string must either **return** a config table (idiomatic Lua
    /// module pattern) or set a global `config` variable (legacy style).
    /// Both forms are supported:
    ///
    /// ```lua
    /// -- Modern style (return a table):
    /// return {
    ///   profiles = { ... },
    ///   on_reply = function(...) ... end,
    /// }
    ///
    /// -- Legacy style (set global):
    /// config = {
    ///   profiles = { ... },
    /// }
    /// ```
    pub fn load_config_string(&self, code: &str) -> LuaResult<LoadedConfig> {
        // ── Try return-style first, fall back to global ─────
        // Return-style: `return { profiles = { ... } }` evaluates to a table.
        // Statement-style: `config = { ... }` evaluates to nil or fails.
        // We try eval first, and only accept it if it gives us a table.
        // Otherwise we exec the script and read `config` from globals.
        let config_table: Table = {
            let attempt = self.lua.load(code).eval::<Value>();
            match attempt {
                Ok(Value::Table(t)) => t,
                _ => {
                    // Either eval failed, or it returned a non-table.
                    // Execute as a statement block and read from globals.
                    self.lua.load(code).exec()?;
                    self.lua.globals().get("config").map_err(|_| {
                        mlua::Error::external(
                            "Config file must either return a table or set a ".to_string()
                                + "global 'config' table.\n"
                                + "\n"
                                + "Modern style (return a table):\n"
                                + "  return {\n"
                                + "    profiles = { ... },\n"
                                + "  }\n"
                                + "\n"
                                + "Legacy style (set global):\n"
                                + "  config = {\n"
                                + "    profiles = { ... },\n"
                                + "  }",
                        )
                    })?
                }
            }
        };

        // ── Extract hook handles (before serde deserialisation) ──
        // Global hooks
        let on_reply: Option<mlua::Function> = config_table.get("on_reply")?;
        let on_send: Option<mlua::Function> = config_table.get("on_send")?;

        // Per-profile hooks
        let profiles_table: Table = config_table.get("profiles")?;
        let mut profile_hooks = HashMap::new();
        for pair in profiles_table.pairs::<String, Table>() {
            let (key, profile_table) = pair?;
            let on_fetch: Option<mlua::Function> = profile_table.get("on_fetch")?;
            profile_hooks.insert(key, ProfileHooks { on_fetch });
        }

        // ── Deserialise data portion ─────────────────────────
        // Lua tables may contain function values (hooks) which serde
        // cannot represent, so we must tell the deserialiser to skip
        // unsupported types.
        let config_value: Value = config_table.into_lua(&self.lua)?;
        let options = DeserializeOptions::new().deny_unsupported_types(false);
        let app_config: AppConfig = self.lua.from_value_with(config_value, options)?;

        // Note: eval: / sh: password prefixes in smtp.password are NOT
        // resolved here.  They are resolved at send time by
        // SmtpConfig::resolved_password(), so that rotating tokens
        // (e.g. from `pass`) are always fresh.

        Ok(LoadedConfig {
            config: app_config,
            profile_hooks,
            global_hooks: GlobalHooks { on_reply, on_send },
        })
    }

    // ── Hook calling ────────────────────────────────────────────

    /// Call the on_fetch hook for a profile.
    ///
    /// Returns `Ok(true)` if the hook executed and returned a truthy
    /// value, `Ok(false)` if the hook returned falsy, or `Err` if
    /// the hook does not exist or failed.
    pub fn call_on_fetch(
        &self,
        profile_label: &str,
        maildir: &str,
        hooks: &ProfileHooks,
    ) -> LuaResult<bool> {
        let func = hooks.on_fetch.as_ref().ok_or_else(|| {
            mlua::Error::external(format!("profile '{}' has no on_fetch hook", profile_label))
        })?;

        // Lua truthiness: nil/false → false, everything else → true
        let result: bool = func.call((profile_label, maildir))?;
        Ok(result)
    }

    /// Call the optional `on_reply` hook.
    ///
    /// Passes `(profile_label, parent_table, mail_table)`.
    /// The hook may modify `mail` in-place or return a new table.
    /// Either way, the (potentially modified) mail is extracted and
    /// returned.
    pub fn call_on_reply(
        &self,
        func: &mlua::Function,
        profile_label: &str,
        parent: &Mail,
        mail: &Mail,
    ) -> LuaResult<Mail> {
        let parent_table = build_mail_table(&self.lua, parent)?;
        let mail_table = build_mail_table(&self.lua, mail)?;

        // Call the hook; discard the return value.  The user can
        // modify `mail` in-place — we always extract from the table
        // argument after the call.
        let _: mlua::Value = func.call((profile_label, parent_table, &mail_table))?;

        let modified = extract_mail_from_table(&mail_table)?;
        Ok(modified)
    }

    /// Call the required `on_send` hook.
    ///
    /// Passes `(profile_label, mail_table)`.  The hook can use
    /// `mail_to_rfc2822()` and `write_tmpfile()` to format and
    /// deliver the mail, or call `send_smtp()` for built-in SMTP.
    ///
    /// Before calling the hook, the SMTP config (if any) is set as
    /// the Lua global `_lorebird_smtp` so that `send_smtp()` can
    /// access it.
    pub fn call_on_send(
        &self,
        func: &mlua::Function,
        profile_label: &str,
        mail: &Mail,
        smtp: Option<&SmtpConfig>,
    ) -> LuaResult<bool> {
        self.set_smtp_global(smtp)?;
        let mail_table = build_mail_table(&self.lua, mail)?;
        // Lua-truthiness: nil/false → failed delivery, anything else → success.
        let ok: bool = func.call((profile_label, mail_table))?;
        Ok(ok)
    }

    /// Set or clear the `_lorebird_smtp` Lua global.
    fn set_smtp_global(&self, smtp: Option<&SmtpConfig>) -> LuaResult<()> {
        let globals = self.lua.globals();
        match smtp {
            Some(config) => {
                // Serialize SmtpConfig to a Lua table
                let value = self.lua.to_value(&config)?;
                globals.set("_lorebird_smtp", value)?;
            }
            None => {
                globals.set("_lorebird_smtp", mlua::Value::Nil)?;
            }
        }
        Ok(())
    }

    // ── Legacy ──────────────────────────────────────────────────

    /// Evaluate a Lua expression and return the result as a string.
    pub fn eval(&self, code: &str) -> LuaResult<String> {
        let val: mlua::Value = self.lua.load(code).eval()?;
        Ok(format!("{:?}", val))
    }

    /// Clean up temporary files created by `write_tmpfile`.
    pub fn cleanup_temp_files(&self) {
        let paths = self.temp_files.borrow_mut().drain(..).collect::<Vec<_>>();
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.cleanup_temp_files();
    }
}

/// Read a Lua sequence of strings into a `Vec<String>`, failing loudly on a
/// non-sequence argument or a non-string element so a malformed `queries`
/// table cannot silently fetch nothing.
fn read_query_list(value: &Value) -> LuaResult<Vec<String>> {
    let table = value.as_table().ok_or_else(|| {
        mlua::Error::external("lorefetch_all: `queries` must be a sequence of strings")
    })?;
    // Read each element as a raw Value and require an actual Lua string. Reading
    // directly as String would let Lua coerce a number (42 → "42"), silently
    // accepting a malformed batch; we reject anything that is not a string.
    let mut out = Vec::with_capacity(table.raw_len());
    for entry in table.clone().sequence_values::<Value>() {
        match entry? {
            Value::String(s) => out.push(s.to_str()?.to_string()),
            _ => {
                return Err(mlua::Error::external(
                    "lorefetch_all: every entry in `queries` must be a string",
                ));
            }
        }
    }
    Ok(out)
}

// ── Lua ↔ Rust conversion helpers ─────────────────────────────────────

/// Build a Lua table from a `Mail` struct.
///
/// Used for both the `parent` and `mail` arguments passed to hooks.
/// `Option<String>` fields are converted to empty strings in Lua
/// so that hooks never see `nil` for string fields.
fn build_mail_table(lua: &Lua, mail: &Mail) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set("from", mail.from.as_str())?;
    table.set("to", mail.to.as_str())?;
    table.set("cc", mail.cc.as_str())?;
    table.set("bcc", mail.bcc.as_str())?;
    table.set("subject", mail.subject.as_str())?;
    table.set("date", mail.date.as_deref().unwrap_or(""))?;
    table.set("message_id", mail.message_id.as_deref().unwrap_or(""))?;
    table.set("in_reply_to", mail.in_reply_to.as_deref().unwrap_or(""))?;
    table.set("references", mail.references.as_deref().unwrap_or(""))?;
    table.set("body_text", mail.body_text.as_str())?;

    // Headers sub-table
    let headers = lua.create_table()?;
    for (k, v) in &mail.headers {
        headers.set(k.as_str(), v.as_str())?;
    }
    table.set("headers", headers)?;

    Ok(table)
}

/// Extract a `Mail` from a Lua table.
///
/// Missing optional fields default to empty strings or None.
fn extract_mail_from_table(table: &Table) -> LuaResult<Mail> {
    let from: String = table.get("from")?;
    let to: String = table.get("to")?;
    let cc: String = table.get("cc").unwrap_or_default();
    let bcc: String = table.get("bcc").unwrap_or_default();
    let subject: String = table.get("subject")?;
    let date: Option<String> = table.get("date")?;
    let message_id: Option<String> = table.get("message_id")?;
    let in_reply_to: Option<String> = table.get("in_reply_to")?;
    let references: Option<String> = table.get("references")?;
    let body_text: String = table.get("body_text")?;

    let headers: HashMap<String, String> = match table.get::<Table>("headers") {
        Ok(h) => {
            let mut map = HashMap::new();
            for pair in h.pairs::<String, String>() {
                let (k, v) = pair?;
                map.insert(k, v);
            }
            map
        }
        Err(_) => HashMap::new(),
    };

    Ok(Mail {
        from,
        to,
        cc,
        bcc,
        subject,
        date,
        message_id,
        in_reply_to,
        references,
        body_text,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> &'static str {
        r#"
config = {
  profiles = {
    ["test"] = {
      maildir = "/tmp/test-mail",
      on_fetch = function(label, maildir)
        return true
      end,
    },
  },
}
"#
    }

    #[test]
    fn new_vm_works() {
        let vm = Vm::new().unwrap();
        let result = vm.eval("2 + 2").unwrap();
        assert!(result.contains("4"));
    }

    #[test]
    fn load_minimal_config() {
        let vm = Vm::new().unwrap();
        let loaded = vm.load_config_string(minimal_config()).unwrap();

        assert!(loaded.config.user.is_none());
        assert_eq!(loaded.config.profiles.len(), 1);

        let profile = &loaded.config.profiles["test"];
        assert_eq!(profile.maildir, "/tmp/test-mail");
        assert!(profile.name.is_none());
        assert!(profile.email.is_none());
        assert!(profile.views.is_empty());

        // Hook was extracted
        assert!(loaded.profile_hooks["test"].on_fetch.is_some());
    }

    #[test]
    fn load_config_return_style() {
        let vm = Vm::new().unwrap();
        let code = r#"
return {
  profiles = {
    ["test"] = {
      maildir = "/tmp/test-mail",
      on_fetch = function(label, maildir) return true end,
    },
  },
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        assert_eq!(loaded.config.profiles.len(), 1);
        assert!(loaded.profile_hooks["test"].on_fetch.is_some());
    }

    #[test]
    fn load_full_config() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  user = {
    name = "Riccardo Maffulli",
    email = "riccardo@defmacro.it",
  },
  profiles = {
    ["qemu nvme"] = {
      name = "Riccardo (work)",
      email = "riccardo@work.com",
      maildir = "/home/nixos/loremail/INBOX",
      on_fetch = function(label, maildir)
        return sh({"echo", "fetching"}).ok
      end,
      views = {
        { label = "last week", query = "date:1w.." },
        { label = "maintainer", query = "f:its@irrelevant.dk" },
      },
    },
    ["personal"] = {
      -- inherits global name/email
      maildir = "/home/nixos/loremail/personal",
      on_fetch = function(label, maildir)
        return true
      end,
    },
  },
  on_send = function(profile, mail_fpath)
    sh({"sendmail", "-t"})
  end,
}
"#;
        let loaded = vm.load_config_string(code).unwrap();

        // Global user
        let user = loaded.config.user.as_ref().unwrap();
        assert_eq!(user.name.as_deref(), Some("Riccardo Maffulli"));
        assert_eq!(user.email.as_deref(), Some("riccardo@defmacro.it"));

        // Profile data
        assert_eq!(loaded.config.profiles.len(), 2);

        let qemu = &loaded.config.profiles["qemu nvme"];
        assert_eq!(qemu.name.as_deref(), Some("Riccardo (work)"));
        assert_eq!(qemu.email.as_deref(), Some("riccardo@work.com"));
        assert_eq!(qemu.maildir, "/home/nixos/loremail/INBOX");
        assert_eq!(qemu.views.len(), 2);
        assert_eq!(qemu.views[0].label, "last week");
        assert_eq!(qemu.views[0].query, "date:1w..");

        let personal = &loaded.config.profiles["personal"];
        assert!(personal.name.is_none());
        assert!(personal.email.is_none());
        assert!(personal.views.is_empty());

        // Resolution
        let resolved = loaded.config.resolve_all();
        assert_eq!(resolved["qemu nvme"].name, "Riccardo (work)");
        assert_eq!(resolved["qemu nvme"].email, "riccardo@work.com");
        assert_eq!(resolved["personal"].name, "Riccardo Maffulli");
        assert_eq!(resolved["personal"].email, "riccardo@defmacro.it");

        // Hooks
        assert!(loaded.profile_hooks["qemu nvme"].on_fetch.is_some());
        assert!(loaded.profile_hooks["personal"].on_fetch.is_some());
        assert!(loaded.global_hooks.on_send.is_some());
        assert!(loaded.global_hooks.on_reply.is_none());
    }

    #[test]
    fn resolve_cascade() {
        let vm = Vm::new().unwrap();
        let loaded = vm.load_config_string(minimal_config()).unwrap();
        let resolved = loaded.config.resolve_all();

        // No global user, no local name/email → defaults
        assert_eq!(resolved["test"].name, "Anonymous");
        assert_eq!(resolved["test"].email, "unknown@localhost");
    }

    #[test]
    fn sh_helper_works() {
        let vm = Vm::new().unwrap();
        let result: Table = vm
            .lua
            .load(r#"return sh({"echo", "hello"})"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
        assert!(result.get::<String>("stdout").unwrap().contains("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn sh_helper_captures_error() {
        let vm = Vm::new().unwrap();
        let result: Table = vm
            .lua
            .load(r#"return sh({"sh", "-c", "exit 42"})"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), false);
        assert_eq!(result.get::<Option<i64>>("exit_code").unwrap(), Some(42));
    }

    #[test]
    fn sh_helper_handles_missing_command() {
        let vm = Vm::new().unwrap();
        let result: Table = vm
            .lua
            .load(r#"return sh({"nonexistent_command_xyz_123"})"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), false);
        assert_eq!(result.get::<i64>("exit_code").unwrap(), -1);
    }

    // ── sh() tests with platform-specific commands ──────────────
    // cat, sh -c, echo $VAR are Unix-only. On Windows we use
    // cmd /C equivalents. The stdin/stdin_file/env features
    // themselves are cross-platform; only the test fixtures differ.

    #[cfg(unix)]
    #[test]
    fn sh_helper_stdin_string() {
        let vm = Vm::new().unwrap();
        let result: Table = vm
            .lua
            .load(r#"return sh({"cat"}, { stdin = "hello from stdin" })"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
        assert_eq!(result.get::<String>("stdout").unwrap(), "hello from stdin");
    }

    #[cfg(windows)]
    #[test]
    fn sh_helper_stdin_string() {
        let vm = Vm::new().unwrap();
        // cmd /C more reads from stdin and writes to stdout
        let result: Table = vm
            .lua
            .load(r#"return sh({"cmd", "/C", "more"}, { stdin = "hello from stdin" })"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert!(result.get::<String>("stdout").unwrap().contains("hello from stdin"));
    }

    #[cfg(unix)]
    #[test]
    fn sh_helper_stdin_string_multiline() {
        let vm = Vm::new().unwrap();
        let lua_code = r#"
            local body = "Subject: test\n\nHello world\n"
            return sh({"cat"}, { stdin = body })
        "#;
        let result: Table = vm.lua.load(lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        let stdout = result.get::<String>("stdout").unwrap();
        assert!(stdout.contains("Subject: test"));
        assert!(stdout.contains("Hello world"));
    }

    #[cfg(windows)]
    #[test]
    fn sh_helper_stdin_string_multiline() {
        let vm = Vm::new().unwrap();
        let lua_code = r#"
            local body = "Subject: test\n\nHello world\n"
            return sh({"cmd", "/C", "more"}, { stdin = body })
        "#;
        let result: Table = vm.lua.load(lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        let stdout = result.get::<String>("stdout").unwrap();
        assert!(stdout.contains("Subject: test"));
        assert!(stdout.contains("Hello world"));
    }

    #[cfg(unix)]
    #[test]
    fn sh_helper_stdin_file() {
        let vm = Vm::new().unwrap();
        let tmp = std::env::temp_dir().join("lorebird_test_stdin_file.txt");
        std::fs::write(&tmp, "piped from file").unwrap();
        let lua_code = format!(
            r#"return sh({{"cat"}}, {{ stdin_file = "{}" }})"#,
            tmp.display()
        );
        let result: Table = vm.lua.load(&lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert_eq!(result.get::<String>("stdout").unwrap(), "piped from file");
        std::fs::remove_file(&tmp).ok();
    }

    #[cfg(windows)]
    #[test]
    fn sh_helper_stdin_file() {
        let vm = Vm::new().unwrap();
        let tmp = std::env::temp_dir().join("lorebird_test_stdin_file.txt");
        std::fs::write(&tmp, "piped from file").unwrap();
        let lua_code = format!(
            r#"return sh({{"cmd", "/C", "more"}}, {{ stdin_file = "{}" }})"#,
            tmp.display()
        );
        let result: Table = vm.lua.load(&lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert!(result.get::<String>("stdout").unwrap().contains("piped from file"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn sh_helper_stdin_file_not_found() {
        let vm = Vm::new().unwrap();
        // This test doesn't actually run the child command —
        // the error happens before spawning, so the command doesn't matter.
        let result: Table = vm
            .lua
            .load(r#"return sh({"cat"}, { stdin_file = "/nonexistent/path/to/file.txt" })"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), false);
        let stderr = result.get::<String>("stderr").unwrap();
        assert!(stderr.contains("cannot open stdin_file"));
    }

    #[cfg(unix)]
    #[test]
    fn sh_helper_stdin_overrides_stdin_file() {
        let vm = Vm::new().unwrap();
        let tmp = std::env::temp_dir().join("lorebird_test_stdin_priority.txt");
        std::fs::write(&tmp, "from file").unwrap();
        let lua_code = format!(
            r#"return sh({{"cat"}}, {{ stdin = "from string", stdin_file = "{}" }})"#,
            tmp.display()
        );
        let result: Table = vm.lua.load(&lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert_eq!(result.get::<String>("stdout").unwrap(), "from string");
        std::fs::remove_file(&tmp).ok();
    }

    #[cfg(windows)]
    #[test]
    fn sh_helper_stdin_overrides_stdin_file() {
        let vm = Vm::new().unwrap();
        let tmp = std::env::temp_dir().join("lorebird_test_stdin_priority.txt");
        std::fs::write(&tmp, "from file").unwrap();
        let lua_code = format!(
            r#"return sh({{"cmd", "/C", "more"}}, {{ stdin = "from string", stdin_file = "{}" }})"#,
            tmp.display()
        );
        let result: Table = vm.lua.load(&lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert!(result.get::<String>("stdout").unwrap().contains("from string"));
        std::fs::remove_file(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn sh_helper_env_variables() {
        let vm = Vm::new().unwrap();
        let result: Table = vm
            .lua
            .load(r#"
                return sh({"sh", "-c", "echo $LOREBIRD_TEST_VAR"}, { env = { LOREBIRD_TEST_VAR = "hello_env" } })
            "#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        let stdout = result.get::<String>("stdout").unwrap();
        assert!(stdout.contains("hello_env"));
    }

    #[cfg(windows)]
    #[test]
    fn sh_helper_env_variables() {
        let vm = Vm::new().unwrap();
        // cmd /C echo %VAR% expands env vars
        let result: Table = vm
            .lua
            .load(r#"
                return sh({"cmd", "/C", "echo", "%LOREBIRD_TEST_VAR%"}, { env = { LOREBIRD_TEST_VAR = "hello_env" } })
            "#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        let stdout = result.get::<String>("stdout").unwrap();
        assert!(stdout.contains("hello_env"));
    }

    #[test]
    fn sh_helper_no_opts_backward_compat() {
        let vm = Vm::new().unwrap();
        // Calling sh() without opts should still work as before
        let result: Table = vm
            .lua
            .load(r#"return sh({"echo", "backward_compat"})"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert!(result.get::<String>("stdout").unwrap().contains("backward_compat"));
    }

    #[cfg(unix)]
    #[test]
    fn sh_helper_stdin_with_gpg_signing_pattern() {
        // Simulates the PGP signing pattern from idea_pgp_signing.md:
        //   local sig = sh({"cat"}, { stdin = mail.body_text })
        let vm = Vm::new().unwrap();
        let lua_code = r#"
            local mail_body = "Looks good.\n"
            local result = sh({"cat"}, { stdin = mail_body })
            return result
        "#;
        let result: Table = vm.lua.load(lua_code).eval().unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), true);
        assert_eq!(result.get::<String>("stdout").unwrap(), "Looks good.\n");
    }

    #[test]
    fn read_file_helper() {
        let vm = Vm::new().unwrap();
        let tmp = std::env::temp_dir().join("lorebird_test_read_file.txt");
        std::fs::write(&tmp, "hello from file").unwrap();
        let code = format!(r#"return read_file("{}")"#, tmp.display());
        let content: String = vm.lua.load(&code).eval().unwrap();
        assert_eq!(content, "hello from file");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn call_on_fetch_returns_truthy() {
        let vm = Vm::new().unwrap();
        let loaded = vm.load_config_string(minimal_config()).unwrap();
        let hooks = &loaded.profile_hooks["test"];
        let result = vm.call_on_fetch("test", "/tmp/test-mail", hooks).unwrap();
        assert!(result);
    }

    #[test]
    fn call_on_fetch_with_falsy_hook() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["fail"] = {
      maildir = "/tmp/none",
      on_fetch = function(label, maildir)
        return false
      end,
    },
  },
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        let hooks = &loaded.profile_hooks["fail"];
        let result = vm.call_on_fetch("fail", "/tmp/none", hooks).unwrap();
        assert!(!result);
    }

    #[test]
    fn lorefetch_all_rejects_non_sequence() {
        let vm = Vm::new().unwrap();
        // A string where a sequence is expected must error, not fetch nothing.
        let err = vm
            .lua
            .load(r#"return lorefetch_all("/tmp/none", "not a table")"#)
            .eval::<Table>()
            .unwrap_err();
        assert!(err.to_string().contains("sequence of strings"));
    }

    #[test]
    fn lorefetch_all_rejects_non_string_entry() {
        let vm = Vm::new().unwrap();
        let err = vm
            .lua
            .load(r#"return lorefetch_all("/tmp/none", {"ok", 42})"#)
            .eval::<Table>()
            .unwrap_err();
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn lorefetch_all_empty_batch_returns_falsy() {
        let vm = Vm::new().unwrap();
        // No queries → no network, no new mail, ok = false.
        let result: Table = vm
            .lua
            .load(r#"return lorefetch_all("/tmp/none", {})"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), false);
        assert_eq!(result.get::<i64>("new").unwrap(), 0);
        assert_eq!(result.get::<i64>("failures").unwrap(), 0);
    }

    #[test]
    fn progress_sink_receives_bare_lorefetch_steps() {
        use std::sync::{Arc, Mutex};
        let vm = Vm::new().unwrap();
        let events: Arc<Mutex<Vec<(usize, Option<usize>)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let _guard = install_progress_sink(Box::new(move |ev| {
            let FetchProgress::Query { step, total, .. } = ev;
            sink_events.lock().unwrap().push((step, total));
        }));

        // Two bare lorefetch calls into a nonexistent maildir; the fetch itself
        // fails but progress is emitted before the network attempt.
        let _: Table = vm
            .lua
            .load(r#"return lorefetch("/nonexistent/xyz", "rt:1w..")"#)
            .eval()
            .unwrap();
        let _: Table = vm
            .lua
            .load(r#"return lorefetch("/nonexistent/xyz", "rt:2w..")"#)
            .eval()
            .unwrap();

        let got = events.lock().unwrap().clone();
        assert_eq!(got, vec![(1, None), (2, None)]);
    }

    #[test]
    fn progress_guard_clears_sink_on_drop() {
        use std::sync::{Arc, Mutex};
        let vm = Vm::new().unwrap();
        let events: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let sink_events = events.clone();
        {
            let _guard = install_progress_sink(Box::new(move |_ev| {
                *sink_events.lock().unwrap() += 1;
            }));
            let _: Table = vm
                .lua
                .load(r#"return lorefetch("/nonexistent/xyz", "rt:1w..")"#)
                .eval()
                .unwrap();
        }
        // Guard dropped: no sink, so this emits nowhere.
        let _: Table = vm
            .lua
            .load(r#"return lorefetch("/nonexistent/xyz", "rt:2w..")"#)
            .eval()
            .unwrap();
        assert_eq!(*events.lock().unwrap(), 1);
    }

    #[test]
    fn write_tmpfile_helper() {
        let vm = Vm::new().unwrap();
        let content = "Hello, tmpfile!";
        let path: String = vm
            .lua
            .load(&format!(
                r#"return write_tmpfile("{}")"#,
                content.replace('\n', "\\n")
            ))
            .eval()
            .unwrap();
        assert!(!path.is_empty());
        let read_back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_back, content);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mail_to_rfc2822_helper() {
        let vm = Vm::new().unwrap();
        let result: String = vm
            .lua
            .load(
                r#"
local mail = {
  from = "Alice <alice@example.com>",
  to = "Bob <bob@example.com>",
  cc = "",
  bcc = "",
  subject = "Test",
  in_reply_to = "<parent@example.com>",
  references = "<grandparent@example.com> <parent@example.com>",
  body_text = "Hello\n",
  headers = { ["X-Mailer"] = "lorebird" },
}
return mail_to_rfc2822(mail)
"#,
            )
            .eval()
            .unwrap();
        assert!(result.contains("From: Alice <alice@example.com>"));
        assert!(result.contains("To: Bob <bob@example.com>"));
        assert!(result.contains("Subject: Test"));
        assert!(result.contains("In-Reply-To: <parent@example.com>"));
        assert!(result.contains("X-Mailer: lorebird"));
        assert!(result.contains("Hello"));
        // Auto-generated headers
        assert!(result.contains("Date: "));
        assert!(result.contains("Message-ID: <"));
        assert!(result.contains("MIME-Version: 1.0"));
        assert!(result.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(result.contains("X-Mailer: lorebird"));
    }

    #[test]
    fn call_on_reply_returns_modified_mail() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["test"] = {
      maildir = "/tmp/test-mail",
      on_fetch = function(label, maildir) return true end,
    },
  },
  on_reply = function(profile, parent, mail)
    mail.cc = "added-by-hook@example.com"
    return mail
  end,
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        let on_reply = loaded.global_hooks.on_reply.as_ref().unwrap();

        let parent = lorebird_core::compose::Mail {
            from: "Alice <alice@example.com>".to_string(),
            to: "list@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Test subject".to_string(),
            date: Some("2024-01-15".to_string()),
            message_id: Some("<abc@def>".to_string()),
            in_reply_to: None,
            references: None,
            body_text: "Original body".to_string(),
            headers: std::collections::HashMap::new(),
        };
        let mail = lorebird_core::compose::Mail::new_reply(
            &parent,
            "Riccardo",
            "riccardo@defmacro.it",
        );

        let result = vm.call_on_reply(on_reply, "test", &parent, &mail).unwrap();
        assert_eq!(result.cc, "added-by-hook@example.com");
        assert_eq!(
            result.to,
            "Alice <alice@example.com>"
        ); // To = parent.from
    }

    #[test]
    fn call_on_reply_in_place_modification() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["test"] = {
      maildir = "/tmp/test-mail",
      on_fetch = function(label, maildir) return true end,
    },
  },
  on_reply = function(profile, parent, mail)
    -- modify in-place, no explicit return needed
    mail.cc = "added-in-place@example.com"
  end,
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        let on_reply = loaded.global_hooks.on_reply.as_ref().unwrap();

        let parent = lorebird_core::compose::Mail {
            from: "Alice <alice@example.com>".to_string(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hello".to_string(),
            date: None,
            message_id: Some("<abc@def>".to_string()),
            in_reply_to: None,
            references: None,
            body_text: String::new(),
            headers: std::collections::HashMap::new(),
        };
        let mail =
            lorebird_core::compose::Mail::new_reply(&parent, "Bob", "bob@example.com");

        // The hook modifies mail.cc in-place; we always get back the
        // (potentially modified) mail, never None.
        let result = vm.call_on_reply(on_reply, "test", &parent, &mail).unwrap();
        assert_eq!(result.cc, "added-in-place@example.com");
    }

    #[test]
    fn call_on_send_invokes_hook() {
        let vm = Vm::new().unwrap();
        // Use a write_tmpfile-based on_send that we can verify
        let tmp = std::env::temp_dir().join("lorebird_test_on_send.txt");
        let tmp_path = tmp.display().to_string();
        let code = format!(
            r#"
config = {{
  profiles = {{
    ["test"] = {{
      maildir = "/tmp/test-mail",
      on_fetch = function(label, maildir) return true end,
    }},
  }},
  on_send = function(profile, mail)
    local fpath = write_tmpfile(mail_to_rfc2822(mail))
    -- Copy to a known location for the test to check
    sh({{"cp", fpath, "{}"}})
    return true
  end,
}}
"#,
            tmp_path
        );
        let loaded = vm.load_config_string(&code).unwrap();
        let on_send = loaded.global_hooks.on_send.as_ref().unwrap();

        let mail = lorebird_core::compose::Mail {
            from: "Alice <alice@example.com>".to_string(),
            to: "Bob <bob@example.com>".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Test send".to_string(),
            date: None,
            message_id: None,
            in_reply_to: None,
            references: None,
            body_text: "Hello from send test\n".to_string(),
            headers: std::collections::HashMap::new(),
        };

        // The on_send should succeed without error
        let ok = vm.call_on_send(on_send, "test", &mail, None).unwrap();
        assert!(ok);

        // Verify the file was written
        if let Ok(content) = std::fs::read_to_string(&tmp) {
            assert!(content.contains("From: Alice <alice@example.com>"));
            assert!(content.contains("Subject: Test send"));
            assert!(content.contains("Hello from send"));
            std::fs::remove_file(&tmp).ok();
        }
    }

    #[test]
    fn call_on_send_returns_false_on_failed_delivery() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["test"] = {
      maildir = "/tmp/test-mail",
      on_fetch = function(label, maildir) return true end,
    },
  },
  on_send = function(profile, mail) return false end,
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        let on_send = loaded.global_hooks.on_send.as_ref().unwrap();

        let mail = lorebird_core::compose::Mail {
            from: "Alice <alice@example.com>".to_string(),
            to: "Bob <bob@example.com>".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Test send".to_string(),
            date: None,
            message_id: None,
            in_reply_to: None,
            references: None,
            body_text: "body\n".to_string(),
            headers: std::collections::HashMap::new(),
        };

        let ok = vm.call_on_send(on_send, "test", &mail, None).unwrap();
        assert!(!ok);
    }

    #[test]
    fn load_config_with_smtp() {
        let vm = Vm::new().unwrap();
        let code = r#"
local my_smtp = {
  host = "smtp.example.com",
  port = 587,
  username = "user@example.com",
  password = "secret",
  starttls = true,
}
config = {
  profiles = {
    ["work"] = {
      maildir = "/tmp/work",
      smtp = my_smtp,
      on_fetch = function(label, maildir) return true end,
    },
    ["personal"] = {
      maildir = "/tmp/personal",
      -- no smtp
      on_fetch = function(label, maildir) return true end,
    },
  },
}
"#;
        let loaded = vm.load_config_string(code).unwrap();

        let resolved = loaded.config.resolve_all();
        assert!(resolved["work"].smtp.is_some());
        let smtp = resolved["work"].smtp.as_ref().unwrap();
        assert_eq!(smtp.host, "smtp.example.com");
        assert_eq!(smtp.port, 587);
        assert_eq!(smtp.username, "user@example.com");
        assert_eq!(smtp.starttls, Some(true));

        assert!(resolved["personal"].smtp.is_none());
    }

    #[test]
    fn smtp_password_eval_prefix_preserved_at_load_time() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["work"] = {
      maildir = "/tmp/work",
      smtp = {
        host     = "smtp.example.com",
        port     = 587,
        username = "user@example.com",
        password = "eval:echo resolved_secret",
        starttls = true,
      },
      on_fetch = function(label, maildir) return true end,
    },
  },
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        let resolved = loaded.config.resolve_all();
        let smtp = resolved["work"].smtp.as_ref().unwrap();
        // The eval: prefix is preserved in the stored config — passwords
        // are resolved at *send* time, not load time, so that rotating
        // tokens (e.g. from `pass`) are always fresh.
        assert_eq!(smtp.password, "eval:echo resolved_secret");
        // But resolved_password() evaluates it on demand:
        assert_eq!(smtp.resolved_password().unwrap(), "resolved_secret");
    }

    #[test]
    fn smtp_password_sh_prefix_preserved_at_load_time() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["work"] = {
      maildir = "/tmp/work",
      smtp = {
        host     = "smtp.example.com",
        port     = 465,
        username = "user@example.com",
        password = "sh:echo sh_secret",
        starttls = false,
      },
      on_fetch = function(label, maildir) return true end,
    },
  },
}
"#;
        let loaded = vm.load_config_string(code).unwrap();
        let resolved = loaded.config.resolve_all();
        let smtp = resolved["work"].smtp.as_ref().unwrap();
        // Password prefix preserved; resolved on demand.
        assert_eq!(smtp.password, "sh:echo sh_secret");
        assert_eq!(smtp.resolved_password().unwrap(), "sh_secret");
    }

    #[test]
    fn smtp_password_eval_failure_returns_send_error() {
        let vm = Vm::new().unwrap();
        let code = r#"
config = {
  profiles = {
    ["work"] = {
      maildir = "/tmp/work",
      smtp = {
        host     = "smtp.example.com",
        port     = 587,
        username = "user@example.com",
        password = "eval:false",
        starttls = true,
      },
      on_fetch = function(label, maildir) return true end,
    },
  },
}
"#;
        // Config loading should succeed — eval: prefix is preserved.
        // The failure happens at send time when resolved_password() is called.
        let loaded = vm.load_config_string(code).unwrap();
        let resolved = loaded.config.resolve_all();
        let smtp = resolved["work"].smtp.as_ref().unwrap();
        assert_eq!(smtp.password, "eval:false"); // prefix preserved
        assert!(
            smtp.resolved_password().is_err(),
            "eval:false should fail at send time"
        );
    }

    #[test]
    fn send_smtp_returns_no_config_error() {
        let vm = Vm::new().unwrap();
        let result: mlua::Table = vm.lua
            .load(r#"return send_smtp("From: test@example.com\nTo: test@example.com\nSubject: Test\n\nHello")"#)
            .eval()
            .unwrap();
        assert_eq!(result.get::<bool>("ok").unwrap(), false);
        let err: String = result.get("error").unwrap();
        assert!(err.contains("no smtp config"));
    }
}

