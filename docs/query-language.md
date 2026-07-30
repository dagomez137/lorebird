# Query Language

lorebird uses a Xapian-inspired mini-language for searching messages.
Queries are parsed into an AST, then translated to SQLite FTS5 expressions
(and optional date-range SQL) for execution.

## Simple terms

A bare word matches any column with a prefix search:

```
patch        →  messages containing "patch", "patches", "patchwork", …
```

An exact phrase is wrapped in double quotes:

```
"memory leak"  →  messages containing the exact phrase "memory leak"
```

## Field prefixes

Restrict a search to a specific header or body column:

| Prefix    | Aliases | Column    | Example                        |
|-----------|---------|-----------|--------------------------------|
| `from`    | `f`     | `from`    | `from:alice@example.com`       |
| `to`      |         | `to`      | `to:qemu-devel@nongnu.org`     |
| `cc`      |         | `cc`      | `cc:linus@kernel.org`          |
| `subject` | `s`     | `subject` | `subject:"memory leak"`        |
| `body`    | `b`     | `body`    | `body:use-after-free`          |
| `dfn`     | `file`, `path` | `body` (diff) | `dfn:lib/xarray.c`      |

A field value can be quoted if it contains spaces:

```
subject:"meeting notes"
```

## Diff paths (`dfn:`)

`dfn:` ("diff filename") finds a patch by a source path it touches. It is a
lore.kernel.org (public-inbox) search prefix: when it appears in a
`config.lua` fetch window it is sent to lore verbatim and matched against
lore's indexed diffs. Locally, the same prefix searches the message body as a
phrase, because a patch diff carries its changed paths on the
`diff --git a/<path> b/<path>` and `+++ b/<path>` lines, which are indexed in
the `body` column. Anything that merely quotes the path matches too.

A trailing `/` or glob (`/*`) is stripped, and a phrase matches any longer
path sharing the same leading path components, so a `MAINTAINERS` `F:`
directory entry matches every file beneath it:

```
dfn:lib/xarray.c                 →  patches touching lib/xarray.c
dfn:tools/testing/radix-tree/*   →  patches under tools/testing/radix-tree/
```

A whole `MAINTAINERS` entry becomes an `OR` of its `F:` paths, optionally
bounded by a date range. For the XARRAY subsystem:

```
(dfn:Documentation/core-api/idr.rst
 OR dfn:Documentation/core-api/xarray.rst
 OR dfn:include/linux/idr.h OR dfn:include/linux/xarray.h
 OR dfn:lib/idr.c OR dfn:lib/test_xarray.c OR dfn:lib/xarray.c
 OR dfn:tools/testing/radix-tree/*)
AND date:10y..
```

Note the local date form is `date:10y..` (see below), not lore's
`rt:10.years.ago..`.

Unknown prefixes fall back to a full-text search across all columns:

```
xyz:hello  →  hello* (searches all columns)
```

## Boolean operators

| Operator | Meaning         | Example                            |
|----------|-----------------|------------------------------------|
| `AND`    | both must match | `from:alice AND subject:patch`     |
| `OR`     | either matches  | `subject:fix OR subject:refactor`  |
| `NOT`    | negation        | `NOT from:bot@kernel.org`          |

Operators are case-insensitive (`and`, `And`, `AND` all work).

### Precedence

`NOT` binds tightest, then `AND`, then `OR`:

```
a OR b AND c        →  a OR (b AND c)
NOT from:bob AND s:hello  →  (NOT from:bob) AND s:hello
```

Use parentheses to override:

```
(a OR b) AND c
from:alice AND (subject:fix OR subject:refactor)
```

### Implicit AND is NOT supported

Two bare words without an operator is a parse error:

```
hello world        →  ✗ error (use "hello AND world")
```

This avoids ambiguity and keeps the grammar simple.

## Date ranges

The `date:` prefix selects messages by age.  It uses a range syntax
with relative offsets from "now":

```
date:N<unit>..         →  N units ago and older
date:..N<unit>         →  within the last N units
date:N1<unit>..N2<unit> →  between N1 and N2 units ago
```

The two bounds are order-independent — `date:2w..1w` and `date:1w..2w`
produce the same range.  The parser normalises so that the older bound
(larger offset) always becomes the start of the range.

### Time units

| Unit | Meaning          | Example             |
|------|------------------|---------------------|
| `m`  | minutes          | `date:30m..`        |
| `h`  | hours            | `date:2h..`         |
| `d`  | days             | `date:3d..`         |
| `w`  | weeks            | `date:..1w`         |
| `mo` | months (30 days) | `date:6mo..`        |
| `y`  | years (365 days) | `date:1y..`         |

### Date examples

| Query            | Meaning                        |
|------------------|--------------------------------|
| `date:3d..`      | 3 days ago and older           |
| `date:..1w`      | within the last week           |
| `date:2w..1w`    | between 1 and 2 weeks ago      |
| `date:1w..3d`    | same range, order-independent  |
| `date:0d..`      | all time (degenerate)          |
| `date:30m..1h`   | between 30 min and 1 hour ago  |

Date filters are applied via a SQL `JOIN` on `mail_ndx.received_ts`,
not through FTS5.  They can be freely combined with text terms:

```
from:alice AND date:1w..
subject:patch AND date:..3d
```

## How it works

```
User query ──► parse_query() ──► Query AST
                                    │
                     ParsedQuery::from_ast()
                                    │
                      ┌─────────────┴──────────────┐
                      │                            │
                   FTS5 expr              DateRange (optional)
                      │                            │
               mail_fts MATCH          mail_ndx.received_ts BETWEEN
                      │                            │
                      └─────────────┬──────────────┘
                                    │
                              message_ids
```

1. `parse_query()` parses the input into a `Query` AST using a
   recursive-descent parser (nom combinators).
2. `ParsedQuery::from_ast()` walks the AST and produces:
   - An FTS5 `MATCH` expression for text terms
   - An optional `DateRange` for date filtering
3. `search()` runs the SQL query, joining `mail_fts` and `mail_ndx`
   as needed, and returns matching message IDs ordered by date (newest first).

## Error handling

| Input             | Error reason                    |
|-------------------|---------------------------------|
| `"unclosed`       | Unclosed double quote           |
| `(unclosed`       | Unclosed parenthesis            |
| `AND`             | Bare keyword without operands   |
| `hello world`     | Implicit AND not supported      |
| `date:..`         | Missing number in date range    |
| `date:3x..`       | Unknown time unit `x`           |

Malformed queries return a parse error with position information.
The UI should display this to the user so they can correct the query.
