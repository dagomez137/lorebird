# Lua API Reference

When writing the configuration file and its hooks, you have access to the
complete [Lua 5.4 API](https://www.lua.org/manual/5.4/) - be sure to see
the list of functions at the bottom of that page.

In addition, LoreBird exports some additional functions, some for convenience,
some to provide access to internal functionality like the ability to fetch
and send mail.

---

## Types

Lorebird passes structured data to hooks via Lua tables.  The table shapes
are documented below so you know which fields exist and what they contain.

### mail

A **mail table** represents an email message.  The same shape is used for
both the `parent` (read-only, the message you're replying to) and `mail`
(mutable, the message you're composing) arguments passed to hooks.

You can change any field on the `mail` argument in-place — the modified
table is always what Lorebird uses after the hook returns.

| Field | Type | Description |
|-------|------|-------------|
| `from` | string | Sender address, e.g. `"Alice <alice@example.com>"` |
| `to` | string | Recipient address(es), comma-separated |
| `cc` | string | Cc address(es), comma-separated. Defaults to `""` |
| `bcc` | string | Bcc address(es), comma-separated. Defaults to `""` |
| `subject` | string | Subject line |
| `date` | string | RFC 2822 date. Empty string `""` if not set (auto-generated at send time) |
| `message_id` | string | Message-ID header, e.g. `"<unique@host>"`. Empty string `""` if not set |
| `in_reply_to` | string | In-Reply-To header. Empty string `""` if not a reply |
| `references` | string | References header (space-separated chain). Empty string `""` if not a reply |
| `body_text` | string | Plain-text body of the email |
| `headers` | table | Arbitrary extra headers, e.g. `{ ["X-Mailer"] = "lorebird" }` |

> **Note:** In Lua, `Option<String>` fields (`date`, `message_id`,
> `in_reply_to`, `references`) are converted to empty strings rather than
> `nil`, so hooks never need to nil-check these fields.

**Example — modifying mail in an on_send hook:**

```lua
on_send = function(profile, mail)
  print("Sending to: " .. mail.to)
  mail.headers["X-Custom"] = "added-by-hook"
  -- modifications are picked up automatically
end
```

**Example — inspecting parent in an on_reply hook:**

```lua
on_reply = function(profile, parent, mail)
  -- parent has the same fields as mail (but read-only)
  if parent.from:find("bugzilla") then
    mail.cc = ""  -- drop Cc for automated messages
  end
end
```

### smtp

An **smtp table** configures an SMTP connection.  Placed inside a profile
or at the top level (shared by all profiles).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | *(required)* | SMTP server hostname |
| `port` | number | `587` | Port number |
| `username` | string | *(required)* | SMTP AUTH username |
| `password` | string | `""` | Password, or `eval:`/`sh:` prefix for on-demand resolution |
| `starttls` | bool? | auto | `true` → force STARTTLS; `false` → SMTPS wrapper; `nil` → auto from port |

**Password prefixes:**

| Prefix | Behaviour |
|--------|-----------|
| *(none)* | Literal password |
| `eval:` | Run the remainder as a shell command, use trimmed stdout as the password |
| `sh:` | Same as `eval:` — run the remainder via `sh -c`, use trimmed stdout |

Passwords with prefixes are **not** resolved at config-load time.  They are
resolved each time `send_smtp()` is called, so rotating tokens stay fresh.

**Example:**

```lua
local smtp = {
  host = "smtp.fastmail.com",
  port = 587,
  username = "me@fastmail.com",
  password = "sh:pass show email/fastmail",
  starttls = true,
}
```

---

## Functions

### `sh(cmd [, opts]) → result_table`

Synchronously execute an external command and capture its output.

**Parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `cmd` | table | yes | Array of strings: `{"program", "arg1", "arg2", ...}` |
| `opts` | table | no | Options table (see below) |

> **Important:** `cmd` is an *array* of separate arguments.  Do not pass a
> single string like `"gpg --armor --detach-sign"` — each argument must be
> its own element.  This avoids shell-injection risks and cross-platform
> quoting issues.

**`opts` keys:**

| Key | Type | Description |
|-----|------|-------------|
| `stdin` | string | Pipe this string to the child's stdin. Takes priority over `stdin_file` |
| `stdin_file` | string | Path to a file piped to the child's stdin |
| `env` | table | Extra environment variables merged with the inherited env, e.g. `{ KEY = "value" }` |

When both `stdin` and `stdin_file` are given, `stdin` wins.

**Return value:**

```lua
{
  ok        = true,     -- bool: exit code was 0
  exit_code = 0,        -- number: process exit code (or -1 on spawn failure)
  stdout    = "...",    -- string: captured standard output
  stderr    = "...",    -- string: captured standard error
}
```

On spawn failure (command not found, permission denied, etc.), `ok` is
`false`, `exit_code` is `-1`, and `stderr` contains an error message.

**Examples:**

```lua
-- Simple command
local result = sh({"echo", "hello"})
print(result.stdout)  -- "hello\n"

-- Pipe a string to stdin (PGP signing pattern)
local sig = sh({"gpg", "--armor", "--detach-sign"}, { stdin = mail.body_text })

-- Pipe a file to stdin (sendmail pattern)
local fpath = write_tmpfile(mail_to_rfc2822(mail))
sh({"msmtp", "-t"}, { stdin_file = fpath })

-- Extra environment variables
sh({"pass", "show", "email/gmail"}, { env = { PASSWORD_STORE_DIR = "/custom/path" } })

-- Combine stdin and env
local result = sh({"gpg", "--armor", "--encrypt", "-r", recipient},
                  { stdin = plain, env = { GNUPGHOME = "/custom/gnupg" } })
```

---

### `lorefetch(maildir, query) → result_table`

Fetch mail from a public-inbox instance (lore.kernel.org) into a local
maildir using Lorebird's built-in HTTP client, Anubis solver, and indexer.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `maildir` | string | Path to the local maildir directory |
| `query` | string | Xapian query string, as used on lore.kernel.org |

**Return value:**

On success:

```lua
{
  ok        = true,
  count     = 42,        -- total messages fetched
  new       = 7,        -- previously unknown messages
  timed_out = true,     -- present only if the fetch timed out
}
```

On failure:

```lua
{
  ok    = false,
  error = "description of the error",
}
```

**Example:**

```lua
on_fetch = function(profile, maildir)
  local query = [[l:qemu-devel AND dfn:nvme AND rt:6.month.ago..now]]
  local result = lorefetch(maildir, query)
  if result.ok then
    print(string.format("%s: %d new of %d total", profile, result.new, result.count))
  else
    print(string.format("%s: fetch error: %s", profile, result.error))
  end
  return result.ok
end
```

---

### `lorefetch_all(maildir, queries) → result_table`

Fetch a whole batch of queries into one maildir. The loop runs in Rust, so
Lorebird knows the batch size up front and drives a determinate progress bar
(step `k` of `N`, with a per-query label) as it works. Prefer this over a
hand-rolled Lua `for` loop around `lorefetch()`: a manual loop leaves the
batch size opaque, so the fetch phase can only pulse instead of showing
`k/N`.

Per-query failures are counted and the loop continues, matching the common
"index whatever arrived, even if one list failed" intent. `queries` must be a
sequence of strings; a non-sequence argument or a non-string element raises an
error rather than silently fetching nothing.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `maildir` | string | Path to the local maildir directory |
| `queries` | table | A sequence (array) of Xapian query strings |

**Return value:**

```lua
{
  ok       = true,   -- true when new > 0
  new      = 12,     -- previously unknown messages across the whole batch
  failures = 1,      -- number of queries that errored (loop still continued)
}
```

**Example:**

```lua
local queries = {
  "l:linux-modules.vger.kernel.org AND rt:6.months.ago..",
  "l:linux-block.vger.kernel.org AND rt:6.weeks.ago..",
}

on_fetch = function(profile, maildir)
  local r = lorefetch_all(maildir, queries)
  print(string.format("[%s] %d new total, %d query failure(s)",
                      profile, r.new or 0, r.failures or 0))
  return (r.new or 0) > 0
end
```

---

### `send_smtp(rfc2822_text) → result_table`

Send an RFC 2822-formatted email via the current profile's SMTP configuration.

The SMTP config is set automatically by Lorebird before calling `on_send`.
If no config is available, the call fails.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `rfc2822_text` | string | Complete RFC 2822 message text |

**Return value:**

```lua
-- On success:
{ ok = true }

-- On failure:
{ ok = false, error = "description of the error" }
```

**Example:**

```lua
on_send = function(profile, mail)
  send_smtp(mail_to_rfc2822(mail))
end
```

---

### `mail_to_rfc2822(mail) → string`

Serialize a mail table into a valid RFC 2822 string.  Use this to convert
the structured `mail` object into a format suitable for `send_smtp()` or
piping to an external MTA.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `mail` | table | A mail table (see [mail](#mail) type above) |

**Return value:** A string containing the complete RFC 2822 message.

Auto-generated headers (`Date`, `Message-ID`, `MIME-Version`,
`Content-Type`) are added if not present.  The `headers` sub-table is
merged in, allowing custom headers like `X-Mailer` or `Reply-To`.

**Example:**

```lua
local rfc_text = mail_to_rfc2822(mail)
print(rfc_text)
-- From: Alice <alice@example.com>
-- To: Bob <bob@example.com>
-- Subject: Re: patch series v2
-- Date: Sun, 8 Jun 2026 12:00:00 +0000
-- Message-ID: <lorebird.1234@example.com>
-- MIME-Version: 1.0
-- Content-Type: text/plain; charset=utf-8
--
-- Looks good.
```

---

### `read_file(path) → string`

Read the entire contents of a file as a string.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `path` | string | Filesystem path to the file |

**Return value:** The file contents as a string.

**Errors:** Raises a Lua error if the file cannot be read.

**Example:**

```lua
local template = read_file("/home/user/.lorebird/reply_template.txt")
```

---

### `write_tmpfile(content) → path`

Write `content` to a unique temporary file and return the file path.  Files
are created under the system temp directory (e.g. `/tmp/lorebird_12345_0` on
Linux) and cleaned up when the application exits.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `content` | string | Content to write to the file |

**Return value:** The file path as a string (not a file handle).

**Example:**

```lua
-- Write mail to temp file and pipe to msmtp
local fpath = write_tmpfile(mail_to_rfc2822(mail))
sh({"msmtp", "-t"}, { stdin_file = fpath })
```

---

## Hooks

Hooks are Lua functions defined in the config file that Lorebird calls at
specific lifecycle points.  They are the primary extension mechanism.

### `on_fetch(profile, maildir) → bool`

**Scope:** Per-profile.  Defined inside each profile table.

Called when the user clicks the Fetch button for a profile.  Use this to
pull new mail into the maildir, either via `lorefetch()` or by shelling out
to an external tool.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `profile` | string | The profile label, e.g. `"qemu-devel"` |
| `maildir` | string | The maildir path from the profile config |

**Return value:** A truthy value indicates success; `false` or `nil`
indicates failure.

**Example:**

```lua
profiles = {
  ["qemu-devel"] = {
    maildir = "/home/user/mail/qemu",
    on_fetch = function(profile, maildir)
      local result = lorefetch(maildir, "l:qemu-devel AND rt:1w..now")
      return result.ok
    end,
  },
},
```

### `on_reply(profile, parent, mail)`

**Scope:** Global.  Defined at the top level of the config table.

Called before the compose/reply window is shown.  The `parent` and `mail`
arguments both use the [mail](#mail) type.  `parent` is the message being
replied to (treat it as read-only).  `mail` is the pre-filled reply and is
**mutable** — changes are reflected in the compose window.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `profile` | string | The profile label |
| `parent` | table | The message being replied to (see [mail](#mail) type, read-only) |
| `mail` | table | The pre-filled reply (see [mail](#mail) type). **Mutable** |

**Return value:** Optional.  Modifications to `mail` are picked up regardless
of whether you return it or modify it in-place.

**Example:**

```lua
on_reply = function(profile, parent, mail)
  -- Drop Cc when replying to automated messages
  if parent.headers["X-Auto-Response"] then
    mail.cc = ""
  end
  -- Add a custom header
  mail.headers["X-Origin"] = "lorebird"
end,
```

### `on_send(profile, mail)`

**Scope:** Global.  Defined at the top level of the config table.

Called when the user clicks Send.  This hook is responsible for actually
delivering the email — either via `send_smtp()` or by piping to an external
MTA.

**Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `profile` | string | The profile label |
| `mail` | table | The composed mail (see [mail](#mail) type). **Mutable** |

**3-way send dispatch:**

| `on_send` defined? | Profile has `smtp`? | Behaviour |
|---------------------|---------------------|-----------|
| ✅ | ✅ | Hook runs; can call `send_smtp(mail_to_rfc2822(mail))` |
| ✅ | ❌ | Hook runs; must use `sh()` or similar to deliver |
| ❌ | ✅ | Automatic: Lorebird sends via built-in SMTP |
| ❌ | ❌ | Error: no way to deliver mail |

**Examples:**

```lua
-- Using built-in SMTP (simplest)
on_send = function(profile, mail)
  send_smtp(mail_to_rfc2822(mail))
end,

-- Using external msmtp via stdin_file
on_send = function(profile, mail)
  local fpath = write_tmpfile(mail_to_rfc2822(mail))
  sh({"msmtp", "-t"}, { stdin_file = fpath })
end,

-- Using external msmtp via stdin
on_send = function(profile, mail)
  sh({"msmtp", "-t"}, { stdin = mail_to_rfc2822(mail) })
end,

-- PGP/MIME signing (see specs/idea_pgp_signing.md for full example)
on_send = function(profile, mail)
  local boundary = "lorebird_pgp_" .. os.time()
  local sig = sh({"gpg", "--armor", "--detach-sign"}, { stdin = mail.body_text })
  mail.headers["Content-Type"] =
    'multipart/signed; protocol="application/pgp-signature"; boundary="' .. boundary .. '"'
  mail.body_text =
        "--" .. boundary .. "\r\n"
    ..  mail.body_text .. "\r\n"
    ..  "--" .. boundary .. "\r\n"
    ..  "Content-Type: application/pgp-signature\r\n\r\n"
    ..  sig .. "\r\n"
    ..  "--" .. boundary .. "--\r\n"
  send_smtp(mail_to_rfc2822(mail))
end,
```

---

## Config File Structure

The Lua config file can use either the **return style** (recommended) or the
**global assignment style** (legacy):

```lua
-- Modern return style (recommended)
return {
  user = { name = "Alice", email = "alice@example.com" },
  profiles = { ... },
  on_send = function(profile, mail) ... end,
}

-- Legacy global assignment style
config = {
  user = { name = "Alice", email = "alice@example.com" },
  profiles = { ... },
  on_send = function(profile, mail) ... end,
}
```

### Top-level keys

| Key | Type | Description |
|-----|------|-------------|
| `user` | table? | Global default `name` and `email` for profiles that don't define their own |
| `theme` | string | `"light"` (default) or `"dark"` |
| `ui_scale` | number | UI scale factor, `1.0` = no scaling |
| `editor` | table? | External editor for the compose body (see [External editor](#external-editor)) |
| `profiles` | table | Map of profile label → profile config |
| `on_reply` | function? | Global reply hook |
| `on_send` | function? | Global send hook |

### Profile keys

| Key | Type | Description |
|-----|------|-------------|
| `maildir` | string | *(required)* Path to local maildir |
| `name` | string? | Sender name (overrides global `user.name`) |
| `email` | string? | Sender email (overrides global `user.email`) |
| `smtp` | table? | SMTP config (see [smtp](#smtp) type) |
| `on_fetch` | function? | Fetch hook for this profile |
| `views` | table? | Array of `{ label = "...", query = "..." }` saved searches |

### Name/email resolution cascade

Each profile resolves its `name` and `email` using this priority:

1. Per-profile `name` / `email` (highest)
2. Global `user.name` / `user.email`
3. Fallback defaults: `"Anonymous"` / `"unknown@localhost"` (lowest)

### External editor

Set `editor` to edit the compose BODY in an external editor. Only the body
round-trips through the editor; the header fields (To/Cc/Subject) stay in the
compose window UI. When an editor is configured the compose window gains an
"Edit body" button, and with `on_open = true` the editor also opens
automatically when a reply or new message is composed.

| Key | Type | Description |
|-----|------|-------------|
| `command` | table | *(required)* argv template. `{file}` is replaced with the temp-file path; if absent, the path is appended as the last argument |
| `on_open` | bool | Launch the editor automatically when the compose window opens (default `false`) |
| `file_suffix` | string | Temp-file suffix so the editor can pick a filetype (default `".eml"`) |

Because a TUI editor such as `hx`, `vim`, or `nvim` needs a terminal that the
GTK process cannot provide, `command` must open one. Use a GUI terminal that
runs in the foreground and exits when the editor exits (Alacritty, Kitty,
WezTerm). macOS `Terminal.app` is not supported here: it does not block, so
LoreBird cannot tell when editing finished.

```lua
-- Helix inside Alacritty; edits the body, syncs back on exit.
editor = {
  command = { "alacritty", "--command", "hx", "{file}" },
  on_open = true,
  file_suffix = ".eml",
}

-- Kitty runs the program directly, no execute flag needed.
editor = { command = { "kitty", "nvim", "{file}" } }
```

While the editor is open the compose body is read-only and Send / Save Draft /
Discard are disabled, so the two buffers cannot diverge. If the editor command
cannot be launched, an error is shown and the in-window editor stays usable.
