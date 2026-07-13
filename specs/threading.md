# lorebird Threading Model

## Overview

lorebird uses two threads:

```
┌──────────────────────┐   mpsc channel    ┌──────────────────────────┐
│   Main thread         │ ──── LuaCommand ─►│   Lua thread             │
│   (GTK, GObjects)     │                    │   (owns Vm, LoadedConfig) │
│   owns AppState,      │ ◄─── LuaResult ───│   runs on_fetch,         │
│   SQLite for reads,   │                    │   on_send, on_reply,     │
│   GObject tree        │                    │   + index_maildir        │
└──────────────────────┘                    └──────────────────────────┘
```

The **main thread** owns all GTK/GObject state and a read-only SQLite
connection used for querying and displaying threads.

The **Lua thread** owns the `mlua::Lua` VM (which is `!Send`) and
all hook handles.  It receives commands from the main thread via an
`mpsc` channel, executes them synchronously, and sends results back.

## Why two threads?

- `mlua::Lua` is `!Send` — it cannot be moved between threads.
  Putting it on its own thread lets us call Lua hooks (which may run
  external commands via `sh()`) without blocking the GTK main loop.
- `rusqlite::Connection` is also `!Send`, so the Lua thread opens its
  **own** SQLite connection for indexing.
- The main thread's SQLite connection is used only for reads
  (loading messages, searching).  SQLite's WAL mode allows
  concurrent readers with a single writer.

## Thread responsibilities

### Main thread (GTK)

| Responsibility | Detail |
|---|---|
| GTK widgets | All `GObject` creation, `ColumnView`, `ThreadNode`, etc. |
| AppState | Owns `AppState` (minus `Vm` and `LoadedConfig`) |
| SQLite reads | `load_all_messages`, `search`, `load_messages_by_ids` |
| Thread tree | `thread::thread_messages` → `ThreadNode` GObject construction |
| Channel sends | Dispatches `LuaCommand` to Lua thread |
| Channel receives | `glib::idle_add` callback processes `LuaResult` |

### Lua thread

| Responsibility | Detail |
|---|---|
| `Vm` + `LoadedConfig` | Owns the Lua VM, config data, and hook handles |
| `on_fetch(label, maildir)` | Calls the Lua hook; if truthy, runs `index_maildir` |
| `on_send(profile, mail)` | Calls the Lua hook; or auto-sends if `smtp` config present |
| `on_reply(profile, parent, mail)` | Calls the Lua hook; may modify mail in-place |
| `index_maildir(conn, maildir)` | Opens its own SQLite connection, indexes maildir |

## Channel protocol

```rust
/// Commands sent from main thread → Lua thread.
/// All fields are Send — only plain data crosses the boundary.
enum LuaCommand {
    /// Call `on_fetch` for the given profile.
    /// If the hook returns truthy, also index the maildir.
    Fetch {
        profile_label: String,
        maildir: PathBuf,
    },

    /// Call `on_reply` hook (if present) with parent and pre-filled mail.
    /// Returns the possibly-modified mail, or None if the hook was absent.
    Reply {
        profile_label: String,
        parent: ParentMail,
        mail: ComposeMail,
    },

    /// Call `on_send` hook with the composed mail.
    /// If no hook is defined but the profile has SMTP config, sends
    /// automatically via built-in SMTP.
    Send {
        profile_label: String,
        mail: ComposeMail,
    },

    /// Shut down the Lua thread.
    Shutdown,
}

/// Results sent from Lua thread → main thread.
/// All fields are Send — no GObject, mlua::Function, or rusqlite::Connection
/// is ever sent through the channel.
enum LuaResult {
    /// Config loaded successfully; here are the resolved profiles.
    InitDone {
        profiles: HashMap<String, ResolvedProfile>,
        theme: String,
        ui_scale: f64,
        has_on_reply: bool,
        has_on_send: bool,
    },

    /// Config loading failed.
    InitFailed {
        error: String,
    },

    /// Fetch + index operation completed.
    FetchDone {
        profile_label: String,
        indexed_count: usize,
        error: Option<String>,
    },

    /// Reply hook completed. Carries the possibly-modified mail (or
    /// None if the hook was absent / returned nil).
    ReplyDone {
        mail: Option<ComposeMail>,
        error: Option<String>,
    },

    /// Send hook completed.
    SendDone {
        error: Option<String>,
    },
}
```

All fields in `LuaCommand` and `LuaResult` are `Send` — only plain
data types (`String`, `PathBuf`, `usize`, `Option<String>`,
`ComposeMail`, `ParentMail`, `HashMap<String, ResolvedProfile>`) cross
the boundary.  No `GObject`, `mlua::Function`, or
`rusqlite::Connection` is ever sent through the channel.

`ComposeMail` and `ParentMail` are `#[derive(Debug, Clone,
Serialize, Deserialize)]` structs containing only `String`,
`Option<String>`, `Vec<ViewConfig>`, and `HashMap<String, String>`
fields — all `Send`.

## Startup sequence

1. Main thread creates `LuaThread::spawn(lorebird_conf_path)`, which:
   - Creates `mpsc::channel::<LuaCommand>()` and
     `mpsc::channel::<LuaResult>()`.
   - Spawns the Lua thread, which:
     a. Creates `Vm::new()`.
     b. Loads config from `lorebird_conf_path` (or default location).
     c. Resolves all profiles.
     d. Sends `LuaResult::InitDone` (or `InitFailed`) back.
2. Main thread calls `lua_thread.recv_init()`, blocking until the
   Lua thread reports success or failure.
3. Main thread creates `AppState` (no `Vm` field — has channel
   sender instead).
4. Lua thread enters command loop: `while let Ok(cmd) = rx.recv() { … }`.

## Fetch flow (detailed)

```
User clicks Fetch
  │
  ▼
Main thread: send LuaCommand::Fetch { label, maildir }
  │
  ▼                                              Lua thread receives Fetch
  (GTK main loop continues,                       │
   spinner animates)                              ▼
                                            Call on_fetch(label, maildir)
                                                 │
                                           ┌─────┴─────┐
                                           │ truthy?    │
                                           └─────┬─────┘
                                            yes  │   no
                                                 ▼
                                       Open SQLite (WAL mode),
                                       run index_maildir()
                                                 │
                                                 ▼
                                       Send LuaResult::FetchDone
                                                 │
  ◄──────────────────────────────────────────────┘
  │
  ▼
Main thread: glib::idle_add callback:
  - hide spinner
  - if error: show in status bar
  - if success: rebuild thread tree from DB, update status
```

## Send flow (detailed)

The send path has a three-way dispatch:

| `on_send` hook | Profile `smtp` config | Behaviour |
|---------------|----------------------|-----------|
| ✅ defined     | ✅ present            | Hook runs; may call `send_smtp()` or `sh({"msmtp"})` |
| ✅ defined     | ❌ absent             | Hook runs; must use `sh({"msmtp"})` etc. |
| ❌ absent      | ✅ present            | Automatic: lorebird sends via built-in SMTP |
| ❌ absent      | ❌ absent             | Error: "no on_send hook and no smtp config" |

When the `on_send` hook is called, the profile's SMTP config is set
as the Lua global `_lorebird_smtp` so that `send_smtp()` can access
it.  This global is set before the hook call and remains set
afterward (there is no teardown — it is simply overwritten on the
next send call for a different profile).

For the automatic path (no hook, `smtp` present), the Lua thread
calls `lorebird_sendmail::send()` directly from Rust, using the
`ComposeMail` struct.  No Lua code is involved.

```
User clicks "Send" in compose window
  │
  ▼
Main thread: send LuaCommand::Send { label, mail }
  │
  ▼                                                Lua thread
  (GTK main loop continues)                         │
                                              ┌─────┴─────────────┐
                                              │ on_send hook?       │
                                              └─────┬─────────────┘
                                               yes   │          no
                                                 ▼               ▼
                                          Set _lorebird    smtp config?
                                          smtp global       │
                                            │          ┌────┴────┐
                                            ▼         yes       no
                                     Call on_send(label,  │         ▼
                                              mail)      ▼    Send error
                                            │    Call send_smtp_auto()
                                            ▼         │
                                     Send LuaResult::SendDone
```

## Reply flow (detailed)

```
User clicks "Reply" on a message
  │
  ▼
Main thread: build ParentMail + pre-filled ComposeMail
  │
  ▼
Main thread: send LuaCommand::Reply { label, parent, mail }
  │
  ▼                                                Lua thread
  (GTK main loop continues)                         │
                                              ┌─────┴─────┐
                                              │ on_reply   │
                                              │ hook?      │
                                              └─────┬─────┘
                                               yes   │    no
                                                 ▼         ▼
                                     Call on_reply(profile,    Return mail
                                                   parent,    unchanged
                                                   mail)     (mail: None)
                                       │
                                       ▼
                              Extract (possibly modified) mail
                              from Lua table argument
                                       │
                                       ▼
                              Send LuaResult::ReplyDone
                                { mail: Some(modified) }
```

The `on_reply` hook may modify `mail` **in-place** in Lua — simply
set fields on the table directly.  The hook's return value is ignored;
lorebird always extracts the modified mail from the table argument
after the hook returns.

## SQLite concurrency

The main thread and Lua thread each have their own `rusqlite::Connection`.

SQLite is opened with `PRAGMA journal_mode=WAL` which allows concurrent
reads while one writer is active.  The main thread's connection is
opened once at index time and reused for reads.  The Lua thread opens
a fresh connection for each index run.

No mutex is needed — SQLite WAL handles this natively.

## Thread safety of domain types

`ComposeMail` and `ParentMail` are `#[derive(Debug, Clone, Serialize,
Deserialize)]` with only `String`, `Option<String>`,
`Vec<ViewConfig>`, and `HashMap<String, String>` fields — all `Send`.
They safely cross the channel boundary.

`ResolvedProfile` contains `String`, `PathBuf`, `Vec<ViewConfig>`,
and `Option<SmtpConfig>` — all `Send`.

`Thread<DbMessage>` is never sent across the channel.  The Lua thread
only sends counts and error messages.  The main thread reads from its
own SQLite connection and builds GObjects locally.

## Future considerations

- `on_fetch` hooks that call `lorefetch()` or `sh()` may take minutes.
  The Lua thread blocks during this, but the main thread stays
  responsive.
- Progress reporting is implemented: during a fetch the Lua thread sends
  non-terminal `LuaResult::FetchProgress { phase, step, total, label }`
  variants through the same channel ahead of the terminal `FetchDone`. A
  thread-local progress sink (installed for the duration of one fetch) lets
  the `lorefetch`/`lorefetch_all` builtins emit per-query events, and the
  indexer reports per-batch counts. The GTK poller drains these to drive a
  determinate progress bar.
- If we want cancel-on-close, the main thread can send `Shutdown` and
  the Lua thread will exit its receive loop.