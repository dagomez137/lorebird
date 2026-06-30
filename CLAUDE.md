# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

LoreBird is a graphical (GTK4) mail reader for `lore.kernel.org` mailing lists, written in Rust. It fetches mail into a maildir, indexes it with SQLite FTS5, threads it with the JWZ algorithm, and is configured/extended in Lua. See `README.md` and `docs/` (user docs) and `specs/` (design specs) for background.

## Building, running, testing

**The toolchain and all native deps (GTK4, GtkSourceView5, SQLite) come from the Nix dev shell — cargo will not build outside it.** Always enter it first:

```bash
nix develop                          # provides cargo, rustc, clippy, gtk4, gtksourceview5, sqlite
cargo run -p lorebird                # launch the GUI (binary name: lorebird, in crates/lorebird-gtk)
# or one-shot without an interactive shell:
nix develop -c cargo run -p lorebird
```

If `nix` is not on PATH, source it: `. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh`

```bash
cargo build                          # whole workspace
cargo test                           # all tests
cargo test -p lorebird-core          # one crate
cargo test -p lorebird-core thread   # one module's tests (filter by name)
cargo clippy --workspace
```

Helper CLIs (each its own workspace member under `tools/`) are useful for exercising core logic without the GUI:
- `cargo run -p query-test` — REPL that parses a query string and prints its AST + generated FTS5.
- `cargo run -p mail-query -- --db <path> --maildir <path> <query…>` — search + thread, print results.
- `cargo run -p lorefetch` / `maildir-index` / `thread-test` — fetch, index, threading.

`#[ignore]`-marked tests that read the user's real DB are sometimes added temporarily for timing/diagnosis — they are not part of the suite; remove them before finishing.

## Runtime layout (where things live)

- **Lua config:** `crates/lorebird-core/src/config_dir.rs` resolves it — macOS `~/Library/Application Support/lorebird/config.lua`, Linux `~/.config/lorebird/config.lua`. The config must `return` a table (or set a global `config`). Full schema is in `crates/lorebird-lua/src/config.rs`. Pass `--config <path>` to override.
- **Per-maildir index DB:** `<maildir>/.lorebird.db` (SQLite). Schema in `crates/lorebird-core/src/schema.rs`; created idempotently by `init_db`, which runs on every `open_db`.
- **App-managed state (not in config.lua):** followed series → `<configdir>/lorebird/follows.json`; drafts/sent-backups → inside the maildir.

## Architecture — the big picture

### Crate split
- **lorebird-core** — all non-UI logic: JWZ threading (`thread.rs`), mail parsing (`message.rs`), SQLite schema + indexer + store (`schema.rs`/`indexer.rs`/`store.rs`), the query DSL (`query.rs`), series normalization + follows + archive (`series.rs`/`follows.rs`/`archive.rs`), maildir read/write + compose.
- **lorebird-lua** — Lua VM (mlua) config loading; deserializes the config table and extracts the hook `Function`s separately (hooks can't be serde-deserialized). `ResolvedProfile` is the per-profile snapshot the UI consumes.
- **lorebird-gtk** — the GUI and **all threading orchestration** (see below). Depends on core + lua.
- **lorebird-lorefetch** — fetches mail from lore (`/all/` index) into a maildir; streams the mbox (no full-buffer), handles Anubis bot-challenges, supports incremental fetch via per-query `last_date` → rewritten `rt:` window.
- **lorebird-sendmail** — SMTP delivery used by the `on_send` hook.

### Three-thread model (the key to understanding lorebird-gtk)
The GTK main thread never blocks. Two background worker threads communicate via mpsc channels; the UI polls results on a glib timeout. See `specs/threading.md`.
- **Lua thread** (`lua_thread.rs`) — owns the Lua VM + config; runs `on_fetch`/`on_reply`/`on_send` hooks and (after a fetch) re-indexes the maildir. Commands/results: `LuaCommand`/`LuaResult`.
- **Query thread** (`query_thread.rs`) — owns a read-only SQLite connection. Loads the most recent `WORKING_SET_LIMIT` messages, JWZ-threads them **once** and caches the threaded view per maildir, then answers view/search requests against that cache. Results are produced as `Send`-able `PlainNode` trees (no per-message disk reads — rich fields like body are read lazily on selection) and **streamed to the UI in newest-first batches**. Cache is dropped on `InvalidateCache` (sent after a re-index).
- `AppState` (`app_state.rs`) is the shared main-thread state (DB handle, root `ListStore`, both thread handles, a monotonic `generation` counter to discard stale results). `window.rs` builds the tri-pane UI and the two pollers.

### Query pipeline (read `query.rs` end to end)
A Xapian-style mini-language is parsed with nom into a `Query` AST, then **either**:
- compiled to an FTS5 match string and run against SQLite (`search`), **or**
- evaluated **in memory** against the cached working set (`matches`) — this is the fast path for view switches; the FTS path is the fallback for body-text queries (`b:`/`body:`) or pre-migration DBs.

Field prefixes: `s:`/`subject`, `f:`/`from`, `b:`/`body`, `to:`, `cc:`, `l:`/`list` (→ List-Id), `a:`/`addr` (fans out across From/To/Cc), plus a `date:N<unit>..` relative-range sublanguage. Note: there is **no `from:(a OR b)` grouping** — write `from:a OR from:b`.

`is:` is an archived-state predicate, not a text match: `is:archived` (only archived), `is:any`/`is:all` (both), `is:active` or absent (exclude archived — the default for views). It contributes nothing to the FTS string and is a no-op in the in-memory `matches`; the actual filtering is applied via `ArchivedFilter` (the in-memory path checks the archived id-set; `search` emits a conditional `IN/NOT IN archived` clause). All Mail's separate LoadAll path is unaffected and always shows everything.

### Indexing & threading notes (non-obvious, learned the hard way)
- The indexer is `INSERT OR IGNORE` keyed on the filename UNIQUE constraint, so it only writes rows for *new* files — existing rows are **not** backfilled when columns are added. Enabling a new indexed column requires deleting `<maildir>/.lorebird.db` and re-indexing.
- Indexing runs in a **single SQLite transaction**; on a very large maildir this is an all-or-nothing, multi-hour operation. Keep the indexed set bounded (fetch windows in `config.lua`).
- Real-world `References` headers can be malformed and form cycles; the threading code has explicit cycle guards (`is_ancestor` bound, visited set) — preserve them. Threading the whole index is in-memory, so the query worker caps it to the recent `WORKING_SET_LIMIT`.

## Conventions
- Edition 2024, `resolver = "3"`. Match the surrounding code's terse comment style and `// ── section ──` dividers.
- Mail sending requires an `on_send` hook returning truthy; returning false is treated as a delivery failure. Reply/compose default headers are pre-filled in core and may be modified by the `on_reply` hook.
- Use long-form command flags everywhere (scripts, docs, commit messages): `mkdir --parents`, `rm --recursive --force`. Use a short flag only when no long form exists, such as `ssh -L`.
- Avoid the em-dash and en-dash appositive style in all prose, including code comments, docs, and commit messages. Rewrite such constructions as separate sentences, or use a colon, a semicolon, or parentheses instead. Box-drawing dividers in code are fine; this rule is about em and en dashes inside sentences.
- Distinguish a tool's project name from its command. Name the project in plain text with its canonical spelling (SQLite, GTK, GtkSourceView, Lua, Nix, Xapian, Anubis) and backtick the executable you run (`cargo`, `sqlite3`, `nix`, `lua`). Backtick code identifiers, file paths, query operators (`is:archived`, `l:`, `rt:`), and SQL.
- Use the modern unified `nix` CLI (`nix build`, `nix develop`, `nix flake check`), never the classic `nix-*` binaries.
- Comment only what the code cannot say for itself. Skip comments and doc comments that restate what an obvious name or line already conveys; reserve them for the non-obvious: the why, an invariant, a subtle ordering or lifetime, a safety justification. Trim docs to the complex or surprising cases. Put the rationale for a change (the why, and the how when it is intricate) in the commit message, not as narration in the source.

## Commit rules

1. One commit per change. Atomic commits only; do not mix unrelated changes, such as a spelling fix with a code change. When in doubt, leave spelling fixes out unless explicitly asked.

2. Write the subject as `subsystem: summary` in the imperative mood, where the subsystem names the area changed: a crate (`core`, `gtk`, `lua`, `lorefetch`, `sendmail`) or a finer area when clearer (`query`, `thread`, `indexer`, `schema`, `series`, `docs`, `build`). Keep the whole subject within 75 characters, following the Linux kernel rule that the summary must be no more than 70 to 75 characters. Aim short; never pad. Do not use conventional-commits prefixes such as `feat:` or `fix:`.

3. Sign off using the git-configured identity. Check it with `git config user.name` and `git config user.email`, then add a `Signed-off-by` trailer.

4. Mark AI-generated work with a `Generated-by: Claude AI` trailer placed immediately before `Signed-off-by`, with no blank line between them:

   ```
   subsystem: summarise the change in the imperative mood

   Plain-English description of what changed and why, wrapped at 75
   columns.

   Generated-by: Claude AI
   Signed-off-by: User Name <user.name@example.org>
   ```

5. No shopping-cart lists. Write the body as plain-English paragraphs, not bullet points or itemised lists, wrapped at 75 columns (trailers are exempt), focused on helping a reviewer understand the implementation.

6. Before committing, build warning-free and run the tests in the Nix dev shell: `nix develop -c cargo build`, `nix develop -c cargo test`, and `nix develop -c cargo clippy --workspace`. The em/en-dash, long-form-flag, and project-name-versus-command rules above apply to commit messages too.
