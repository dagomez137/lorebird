# Line-length conventions (kernel mail and patches)

Background for the reading pane's default width (`reading_pane_columns`,
default 100). Summarises the line-length rules LoreBird's audience writes to,
so the pane can show patches, quoted replies and commit logs without wrapping.

## Email body

- **RFC 5322**: a body line SHOULD be at most **78** characters (excluding
  CRLF) and MUST be at most 998. The 78 figure exists so user interfaces do
  not truncate or wrap the display.
- **Thomas Gleixner (kernel netiquette)**: "Text-based e-mail should not
  exceed **80 columns** per line of text. ... enable proper line breaks around
  **column 78**." Also: plain text only, bottom-post, and trim quoted text.
- **Kernel `email-clients.rst`**: disable `format=flowed` and the client's
  automatic word wrap; configure a fixed wrap (Thunderbird's default
  `wraplength` is **72**, some clients wrap at **78**).
- **Quoting**: each reply level prefixes `> ` (2 columns). Wrapping the body
  near 72 to 78 leaves room for a few `> > >` levels before lines reach ~80.
- **subspace.kernel.org etiquette**: gives no column number, but requires
  plain text (no HTML), interleaved quoted replies (not top-posting),
  reply-to-all, trimming quotes, and not quoting large chunks of code. These
  shape how wide a quoted reply grows, which the body wrapping must allow for.

## Patches and code

- **`coding-style.rst`**: the preferred limit on a line is **80 columns**;
  longer lines are allowed only when they clearly improve readability.
- **`checkpatch.pl`**: `max_line_length = 100`. It warns only past **100**
  columns, the relaxed ceiling for code.

## Commit messages

- **`submitting-patches.rst`**: the summary (subject) must be **70 to 75**
  characters, written as `subsystem: summary`; the body is wrapped at **75**
  columns. Trailers are exempt.
- **`checkpatch.pl`**: `COMMIT_LOG_LONG_LINE` warns "Prefer a maximum 75 chars
  per line"; it also expects referenced commits as
  `commit <12+ chars of sha1> ("title")`.

## Why the reading pane defaults to 100 columns

100 monospace columns is the widest hard line the audience produces (the
checkpatch code ceiling). It also comfortably fits 80-column code, a 78-column
email with a couple of quote levels, and a 75-column commit log, all without
wrapping. The divider stays draggable and the count is configurable via
`reading_pane_columns`.

## Links

- RFC 5322, Internet Message Format: https://www.rfc-editor.org/rfc/rfc5322.html
- Thomas Gleixner, "Notes about netiquette": https://people.kernel.org/tglx/notes-about-netiquette
- subspace.kernel.org, mailing-list etiquette: https://subspace.kernel.org/etiquette.html
- Kernel docs `process/email-clients.rst`, `process/coding-style.rst`,
  `process/submitting-patches.rst` (in the kernel tree under `Documentation/`)
- `scripts/checkpatch.pl` (`max_line_length`, `COMMIT_LOG_LONG_LINE`)
