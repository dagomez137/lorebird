//! Query parser: a Xapian-inspired mini-language parsed with nom.
//!
//! Grammar (recursive descent, weakest-to-strongest binding):
//!   query     = or_expr
//!   or_expr   = and_expr ~ ("OR"  ~ and_expr)*
//!   and_expr  = not_expr ~ ("AND" ~ not_expr)*
//!   not_expr  = "NOT"? ~ atom
//!   atom      = field_term | quoted_phrase | "(" ~ or_expr ~ ")" | bare_word
//!   field_term = prefix ":" (quoted_string | unquoted_word | date_range)
//!
//! Examples:
//!   hello
//!   from:alice@example.com
//!   subject:"meeting notes"
//!   from:alice AND (subject:foo OR subject:bar)
//!   NOT from:bob
//!   date:3d..
//!   date:..1w
//!   date:2w..1w

use nom::{
    IResult,
    branch::alt,
    bytes::complete::{tag, tag_no_case, take_till1},
    character::complete::{char, digit1, multispace0, multispace1},
    combinator::{map, map_res, opt, peek},
    multi::many0,
    sequence::{delimited, preceded, terminated, tuple},
};
use rusqlite::{Connection, Result as SqlResult, params};
use std::time::SystemTime;

// ── AST ────────────────────────────────────────────────────────────────

/// A parsed query expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `prefix:value` field restriction.
    Field { prefix: String, value: String },
    /// `"quoted phrase"` — exact phrase match.
    Phrase(String),
    /// A bare word token.
    Word(String),
    /// `date:N<unit>..`, `date:..N<unit>`, or `date:N1<unit>..N2<unit>`.
    Date(DateRange),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
    Not(Box<Query>),
}

/// A relative date range parsed from a `date:` prefix value.
///
/// Offsets are seconds before "now".  `None` means unbounded: `start_secs`
/// = beginning of time, `end_secs` = now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateRange {
    /// Seconds before now for the older end (further in the past).
    pub start_secs: Option<i64>,
    /// Seconds before now for the newer end (closer to now).
    pub end_secs: Option<i64>,
}

impl DateRange {
    /// Resolve relative offsets to absolute Unix timestamps.
    fn resolve(&self) -> (i64, i64) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Saturating, then floored at 0: a clamped offset of i64::MAX means
        // "beginning of time" (Unix epoch), not a negative timestamp.
        let start = self.start_secs.map(|s| now.saturating_sub(s).max(0)).unwrap_or(0);
        let end = self.end_secs.map(|e| now.saturating_sub(e).max(0)).unwrap_or(now);
        (start.min(end), start.max(end))
    }
}

// ── public API ─────────────────────────────────────────────────────────

/// Parse a user-supplied query string into an AST.
///
/// Returns `Ok(Query)` on success.  Malformed input produces an `Err`
/// carrying a nom error (which includes position information).
pub fn parse_query(input: &str) -> Result<Query, nom::Err<nom::error::Error<&str>>> {
    let (rest, q) = preceded(multispace0, or_expr)(input)?;
    let (rest, _) = multispace0(rest)?;
    if rest.is_empty() {
        Ok(q)
    } else {
        Err(nom::Err::Error(nom::error::Error::new(
            rest,
            nom::error::ErrorKind::Eof,
        )))
    }
}

// ── FTS5 bridge ────────────────────────────────────────────────────────

/// Whether a query selects active (non-archived), archived, or all messages.
///
/// Driven by `is:` predicates in the query: `is:archived` → only archived,
/// `is:any` (or `is:all`) → both, anything else / absent → `Active`. Views
/// default to `Active` so archived mail stays hidden until explicitly asked
/// for; All Mail uses its own unfiltered path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArchivedFilter {
    /// Exclude archived messages — the default for views.
    #[default]
    Active,
    /// Only archived messages.
    Archived,
    /// Both archived and non-archived.
    Any,
}

/// Materialised query ready to hand to SQLite.
#[derive(Debug, Clone)]
pub struct ParsedQuery {
    /// FTS5 expression string (safe to pass directly to SQLite).
    pub fts5: String,
    /// Optional date range filter (applied via JOIN on `mail_ndx`).
    pub date_range: Option<DateRange>,
    /// Limit on the number of results.
    pub limit: i64,
    /// Archived-state filter derived from `is:` predicates.
    pub archived: ArchivedFilter,
}

impl ParsedQuery {
    /// Create a query that matches everything.
    pub fn all() -> Self {
        Self { fts5: String::new(), date_range: None, limit: 50, archived: ArchivedFilter::Active }
    }

    /// Build a materialised query from a parsed AST.
    pub fn from_ast(query: &Query, limit: i64) -> Self {
        let (fts5, date_range) = Self::build(query);
        Self { fts5, date_range, limit, archived: archived_filter(query) }
    }

    /// Walk the AST and produce an FTS5 expression plus an optional date range.
    fn build(query: &Query) -> (String, Option<DateRange>) {
        match query {
            Query::Date(range) => (String::new(), Some(range.clone())),
            Query::Field { prefix, value } => {
                let escaped = value.replace('"', "\"\"");
                // `a:`/`addr:` matches the address across From, To and Cc
                // (mirrors lorefetch's `a:` prefix).
                if matches!(prefix.to_lowercase().as_str(), "a" | "addr") {
                    let term = format!(
                        "(from:\"{e}\" OR to:\"{e}\" OR cc:\"{e}\")",
                        e = escaped
                    );
                    return (term, None);
                }
                // `is:` is an archived-state predicate, not a text match — it
                // contributes nothing to the FTS expression (see ArchivedFilter).
                if prefix.eq_ignore_ascii_case("is") {
                    return (String::new(), None);
                }
                // `dfn:`/`file:`/`path:` — find a patch by a source path it
                // touches. A patch diff carries the changed paths on its
                // `diff --git a/<path> b/<path>` and `+++ b/<path>` lines, all
                // in the message body, so the path is matched as a body phrase.
                // FTS5 tokenises `lib/xarray.c` to `lib xarray c`; a phrase
                // matches those tokens in sequence, which the diff header lines
                // contain (also matching any mail that quotes the path).
                if matches!(prefix.to_lowercase().as_str(), "dfn" | "file" | "path") {
                    // A phrase matches any longer path sharing the same leading
                    // tokens, so a MAINTAINERS `F:` directory entry (with or
                    // without a trailing glob) matches every file beneath it.
                    let path = value.trim_end_matches('*').trim_end_matches('/');
                    if path.is_empty() {
                        return (String::new(), None);
                    }
                    return (format!("body:\"{}\"", path.replace('"', "\"\"")), None);
                }
                let col = map_prefix_to_column(prefix);
                let term = match col {
                    Some(c) => format!("{}:\"{}\"", c, escaped),
                    None => value.split_whitespace()
                        .map(|w| format!("{}*", w))
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                (term, None)
            }
            Query::Phrase(s) => {
                (format!("\"{}\"", s.replace('"', "\"\"")), None)
            }
            Query::Word(w) => {
                (format!("{}*", w), None)
            }
            Query::And(a, b) => {
                let (l, ld) = Self::build(a);
                let (r, rd) = Self::build(b);
                let date_range = ld.or(rd);
                if l.is_empty() { return (r, date_range); }
                if r.is_empty() { return (l, date_range); }
                (format!("({}) AND ({})", l, r), date_range)
            }
            Query::Or(a, b) => {
                let (l, ld) = Self::build(a);
                let (r, rd) = Self::build(b);
                (format!("({}) OR ({})", l, r), ld.or(rd))
            }
            Query::Not(a) => {
                let (inner, dr) = Self::build(a);
                (format!("NOT ({})", inner), dr)
            }
        }
    }
}

// ── In-memory query evaluator ──────────────────────────────────────────

/// The subset of message fields the in-memory evaluator can filter on.
///
/// These come from `mail_ndx` (after the schema migration). Body text is
/// deliberately absent — it lives only in `mail_fts` and is not cached, so
/// `b:`/`body:` terms and bare words that would need the body are NOT
/// covered by this path (see [`matches`]).
pub struct FilterFields<'a> {
    pub subject: Option<&'a str>,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub cc: Option<&'a str>,
    /// Already-normalised inner list id (e.g. `linux-block.vger.kernel.org`).
    pub list_id: Option<&'a str>,
    pub received_ts: i64,
}

/// Case-insensitive substring test (`needle` already lowercased by caller).
fn contains_ci(haystack: Option<&str>, needle_lower: &str) -> bool {
    match haystack {
        Some(h) => h.to_lowercase().contains(needle_lower),
        None => false,
    }
}

/// Evaluate a parsed [`Query`] AST against a single cached message in memory.
///
/// Semantics (case-insensitive substring/token match — it need not perfectly
/// mirror FTS5 tokenisation):
/// - `l:`/`list:` → `list_id` contains the (normalised) value
/// - `f:`/`from:` → `from`
/// - `to:` / `cc:` → respective field
/// - `a:`/`addr:` → `from` OR `to` OR `cc`
/// - `s:`/`subject:` → `subject`
/// - bare `Word`/`Phrase` → match across subject/from/to/cc/list_id
/// - `Date` → `received_ts` within the resolved range
/// - `And`/`Or`/`Not` → recurse
///
/// Body-only terms (`b:`/`body:`) cannot be satisfied from the cache; they
/// evaluate to `false` here. Callers that need body matching must use the
/// FTS [`search`] path. An unknown field prefix is treated like a bare word.
pub fn matches(query: &Query, msg: &FilterFields) -> bool {
    match query {
        Query::Field { prefix, value } => {
            let needle = normalize_field_value(prefix, value);
            match prefix.to_lowercase().as_str() {
                "l" | "list" => contains_ci(msg.list_id, &needle),
                "f" | "from" => contains_ci(msg.from, &needle),
                "to" => contains_ci(msg.to, &needle),
                "cc" => contains_ci(msg.cc, &needle),
                "a" | "addr" => {
                    contains_ci(msg.from, &needle)
                        || contains_ci(msg.to, &needle)
                        || contains_ci(msg.cc, &needle)
                }
                "s" | "subject" => contains_ci(msg.subject, &needle),
                // Body is not cached → can't be satisfied here. `dfn:`/`file:`/
                // `path:` search the diff, which lives in the body, so they are
                // in the same boat and route through the FTS path.
                "b" | "body" | "dfn" | "file" | "path" => false,
                // `is:` is an archived-state predicate handled separately by
                // the caller (via ArchivedFilter + the archived id-set), so it
                // is a no-op for text matching here.
                "is" => true,
                // Unknown prefix → search across the available text fields.
                _ => matches_any_text(msg, &needle),
            }
        }
        Query::Phrase(s) => matches_any_text(msg, &s.to_lowercase()),
        Query::Word(w) => matches_any_text(msg, &w.to_lowercase()),
        Query::Date(range) => {
            let (start, end) = range.resolve();
            msg.received_ts >= start && msg.received_ts <= end
        }
        Query::And(a, b) => matches(a, msg) && matches(b, msg),
        Query::Or(a, b) => matches(a, msg) || matches(b, msg),
        Query::Not(a) => !matches(a, msg),
    }
}

/// Normalise a field value for comparison. `list` values are run through the
/// same `List-Id` normalisation used at index time, so e.g. a user typing
/// `l:Linux block <linux-block.vger.kernel.org>` still matches.
fn normalize_field_value(prefix: &str, value: &str) -> String {
    match prefix.to_lowercase().as_str() {
        "l" | "list" => crate::message::normalize_list_id(value),
        _ => value.to_lowercase(),
    }
}

/// Match a needle across every available text field.
fn matches_any_text(msg: &FilterFields, needle_lower: &str) -> bool {
    contains_ci(msg.subject, needle_lower)
        || contains_ci(msg.from, needle_lower)
        || contains_ci(msg.to, needle_lower)
        || contains_ci(msg.cc, needle_lower)
        || contains_ci(msg.list_id, needle_lower)
}

/// Whether evaluating `query` in memory would require the message body
/// (which the cache does not hold). When `true`, the caller must use the
/// FTS [`search`] path to get correct results.
///
/// A bare `Word`/`Phrase` is matched against the cached text fields here, so
/// it does NOT force the FTS path — only explicit body terms do: `b:`/`body:`
/// and the diff-path prefixes `dfn:`/`file:`/`path:`, which search the diff.
pub fn needs_body(query: &Query) -> bool {
    match query {
        Query::Field { prefix, .. } => {
            matches!(prefix.to_lowercase().as_str(), "b" | "body" | "dfn" | "file" | "path")
        }
        Query::And(a, b) | Query::Or(a, b) => needs_body(a) || needs_body(b),
        Query::Not(a) => needs_body(a),
        Query::Phrase(_) | Query::Word(_) | Query::Date(_) => false,
    }
}

/// Derive the [`ArchivedFilter`] from any `is:` predicates in the query.
///
/// `is:archived` → [`ArchivedFilter::Archived`], `is:any`/`is:all` →
/// [`ArchivedFilter::Any`]; with none present (or only `is:active`) the
/// default [`ArchivedFilter::Active`] hides archived mail. If several appear,
/// `Archived` wins over `Any` wins over `Active`.
pub fn archived_filter(query: &Query) -> ArchivedFilter {
    fn of_value(value: &str) -> ArchivedFilter {
        match value.to_lowercase().as_str() {
            "archived" => ArchivedFilter::Archived,
            "any" | "all" => ArchivedFilter::Any,
            _ => ArchivedFilter::Active, // is:active or unrecognised
        }
    }
    fn merge(a: ArchivedFilter, b: ArchivedFilter) -> ArchivedFilter {
        use ArchivedFilter::*;
        match (a, b) {
            (Archived, _) | (_, Archived) => Archived,
            (Any, _) | (_, Any) => Any,
            _ => Active,
        }
    }
    match query {
        Query::Field { prefix, value } if prefix.eq_ignore_ascii_case("is") => of_value(value),
        Query::And(a, b) | Query::Or(a, b) => merge(archived_filter(a), archived_filter(b)),
        Query::Not(a) => archived_filter(a),
        _ => ArchivedFilter::Active,
    }
}

/// Map a user-facing prefix to an FTS5 column name.
fn map_prefix_to_column(prefix: &str) -> Option<&'static str> {
    match prefix.to_lowercase().as_str() {
        "s" | "subject" => Some("subject"),
        "f" | "from" => Some("from"),
        "b" | "body" => Some("body"),
        "to" => Some("to"),
        "cc" => Some("cc"),
        "l" | "list" => Some("list_id"),
        _ => None, // unknown prefix → search all columns
    }
}

/// Walk a parsed [`Query`] AST and produce an FTS5 string only.
///
/// Convenience wrapper around `ParsedQuery::build` that discards the
/// date range.  For full query materialisation use `ParsedQuery::from_ast`.
pub fn query_to_fts5(query: &Query) -> String {
    ParsedQuery::build(query).0
}

/// SQL fragment applying the archived-state filter to `col` (a column
/// expression like `message_id` or `f.message_id`). Returns a leading-space
/// `AND …` clause, or empty for [`ArchivedFilter::Any`]. The text is fixed
/// (no user input), so it is safe to interpolate into the statement.
fn archived_sql(filter: ArchivedFilter, col: &str) -> String {
    match filter {
        ArchivedFilter::Active => {
            format!(" AND {col} NOT IN (SELECT message_id FROM archived)")
        }
        ArchivedFilter::Archived => {
            format!(" AND {col} IN (SELECT message_id FROM archived)")
        }
        ArchivedFilter::Any => String::new(),
    }
}

/// Search the FTS5 index and return matching message IDs.
pub fn search(conn: &Connection, query: &ParsedQuery) -> SqlResult<Vec<String>> {
    // ── date-filtered path ──
    if let Some(ref range) = query.date_range {
        let (start_ts, end_ts) = range.resolve();
        if query.fts5.is_empty() {
            let sql = format!(
                "SELECT message_id FROM mail_ndx
                 WHERE received_ts >= ?1 AND received_ts <= ?2{arch}
                 ORDER BY received_ts DESC LIMIT ?3",
                arch = archived_sql(query.archived, "message_id"),
            );
            let mut stmt = conn.prepare(&sql)?;
            return stmt
                .query_map(params![start_ts, end_ts, query.limit], |r| r.get(0))?
                .collect::<SqlResult<Vec<String>>>();
        }
        let sql = format!(
            "SELECT f.message_id
             FROM mail_fts f
             JOIN mail_ndx n USING (message_id)
             WHERE mail_fts MATCH ?1
               AND n.received_ts >= ?2 AND n.received_ts <= ?3{arch}
             ORDER BY n.received_ts DESC
             LIMIT ?4",
            arch = archived_sql(query.archived, "f.message_id"),
        );
        let mut stmt = conn.prepare(&sql)?;
        return stmt
            .query_map(
                params![&query.fts5, start_ts, end_ts, query.limit],
                |r| r.get(0),
            )?
            .collect::<SqlResult<Vec<String>>>();
    }

    // ── no date filter ──
    if query.fts5.is_empty() {
        let sql = format!(
            "SELECT message_id FROM mail_ndx
             WHERE 1=1{arch}
             ORDER BY received_ts DESC LIMIT ?1",
            arch = archived_sql(query.archived, "message_id"),
        );
        let mut stmt = conn.prepare(&sql)?;
        return stmt
            .query_map(params![query.limit], |r| r.get(0))?
            .collect::<SqlResult<Vec<String>>>();
    }

    // Join mail_ndx so the LIMIT keeps the *newest* matches, not an
    // arbitrary subset.
    let sql = format!(
        "SELECT f.message_id FROM mail_fts f
         JOIN mail_ndx n USING (message_id)
         WHERE mail_fts MATCH ?1{arch}
         ORDER BY n.received_ts DESC
         LIMIT ?2",
        arch = archived_sql(query.archived, "f.message_id"),
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(params![&query.fts5, query.limit], |r| r.get(0))?
        .collect::<SqlResult<Vec<String>>>()
}

// ── nom parsers (private) ──────────────────────────────────────────────

/// A non-whitespace, non-paren token for bare words / unquoted values.
fn unquoted_word(input: &str) -> IResult<&str, String> {
    let (rest, w) = take_till1(|c: char| c.is_whitespace() || c == '(' || c == ')')(input)?;
    Ok((rest, w.to_string()))
}

/// A double-quoted string: `"hello world"`.
fn quoted_string(input: &str) -> IResult<&str, String> {
    delimited(
        char('"'),
        take_till1(|c: char| c == '"'),
        char('"'),
    )(input)
    .map(|(rest, s)| (rest, s.to_string()))
}

/// A quoted phrase standing alone: `"hello world"`.
///
/// Uses `peek` first so that an unclosed quote fails without consuming
/// any input, allowing `alt` to fall through to `bare_word`.
fn quoted_phrase(input: &str) -> IResult<&str, Query> {
    let _ = peek(delimited(char('"'), take_till1(|c: char| c == '"'), char('"')))(input)?;
    map(quoted_string, Query::Phrase)(input)
}

// ── date-range parsers ─────────────────────────────────────────────────

/// Parse a non-negative integer used as a date offset multiplier.
///
/// The digit count is capped (an offset never needs more than this many
/// digits — `9` digits is already ~270 years in seconds when multiplied by
/// the largest unit) so an absurdly long run of digits can't overflow `i64`
/// in the later `n * unit` step and panic the worker thread in debug builds.
fn date_number(input: &str) -> IResult<&str, i64> {
    map_res(digit1, |s: &str| {
        // Clamp to a sane magnitude; saturating arithmetic downstream also
        // guards the multiply, but capping here keeps parses well-formed.
        if s.len() > MAX_DATE_DIGITS {
            return Ok::<i64, std::num::ParseIntError>(i64::MAX);
        }
        Ok(s.parse::<i64>().unwrap_or(i64::MAX))
    })(input)
}

/// Maximum digits accepted for a date offset before it is clamped.
const MAX_DATE_DIGITS: usize = 9;

/// Multiply an offset count by a unit-in-seconds without overflowing.
/// Saturates at `i64::MAX` so a huge `date:99999999999y..` clamps to "all
/// time" rather than panicking on the worker thread.
fn offset_secs(n: i64, unit: i64) -> i64 {
    n.checked_mul(unit).unwrap_or(i64::MAX)
}

/// Parse a time unit, returning the equivalent in seconds.
fn date_unit(input: &str) -> IResult<&str, i64> {
    alt((
        map(tag("mo"), |_| 30 * 86400),
        map(tag("m"), |_| 60),
        map(tag("h"), |_| 3600),
        map(tag("d"), |_| 86400),
        map(tag("w"), |_| 7 * 86400),
        map(tag("y"), |_| 365 * 86400),
    ))(input)
}

/// `N<unit>..`
fn date_open_start(input: &str) -> IResult<&str, DateRange> {
    let (rest, n) = date_number(input)?;
    let (rest, unit) = date_unit(rest)?;
    let (rest, _) = tag("..")(rest)?;
    Ok((rest, DateRange { start_secs: Some(offset_secs(n, unit)), end_secs: None }))
}

/// `..N<unit>`
fn date_open_end(input: &str) -> IResult<&str, DateRange> {
    let (rest, _) = tag("..")(input)?;
    let (rest, n) = date_number(rest)?;
    let (rest, unit) = date_unit(rest)?;
    Ok((rest, DateRange { start_secs: None, end_secs: Some(offset_secs(n, unit)) }))
}

/// `N1<unit>..N2<unit>`
fn date_bounded(input: &str) -> IResult<&str, DateRange> {
    let (rest, n1) = date_number(input)?;
    let (rest, u1) = date_unit(rest)?;
    let (rest, _) = tag("..")(rest)?;
    let (rest, n2) = date_number(rest)?;
    let (rest, u2) = date_unit(rest)?;
    let secs1 = offset_secs(n1, u1);
    let secs2 = offset_secs(n2, u2);
    let older = secs1.max(secs2);
    let newer = secs1.min(secs2);
    Ok((rest, DateRange { start_secs: Some(older), end_secs: Some(newer) }))
}

/// Any of the three date-range forms.
fn date_value(input: &str) -> IResult<&str, DateRange> {
    alt((date_bounded, date_open_start, date_open_end))(input)
}

// ── field-term parser (with date: special-casing) ──────────────────────

/// A field term: `prefix:value`, `prefix:"quoted value"`, or `date:...`
///
/// Uses `peek` to avoid consuming input when there is no colon,
/// which lets `alt` try the next alternative.
fn field_term(input: &str) -> IResult<&str, Query> {
    // Peek: must be identifier followed by ':'
    let _ = peek(tuple((
        take_till1(|c: char| c.is_whitespace() || c == ':' || c == '(' || c == ')' || c == '"'),
        char(':'),
    )))(input)?;

    let (rest, prefix) = take_till1(|c: char| c.is_whitespace() || c == ':' || c == '(' || c == ')' || c == '"')(input)?;
    let (rest, _) = char(':')(rest)?;

    // `date:` has its own sub-grammar
    if prefix.eq_ignore_ascii_case("date") {
        return date_value(rest).map(|(rest, dr)| (rest, Query::Date(dr)));
    }

    let (rest, value) = alt((quoted_string, unquoted_word))(rest)?;
    Ok((rest, Query::Field { prefix: prefix.to_string(), value }))
}

/// A parenthesised group: `( or_expr )`.
fn parens(input: &str) -> IResult<&str, Query> {
    delimited(
        preceded(multispace0, char('(')),
        preceded(multispace0, or_expr),
        preceded(multispace0, char(')')),
    )(input)
}

/// A bare word — rejects keywords and tokens starting with `"`
/// (almost certainly an unclosed quote).
fn bare_word(input: &str) -> IResult<&str, Query> {
    let (rest, w) = unquoted_word(input)?;
    if w.starts_with('"') {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Tag,
        )));
    }
    let lower = w.to_lowercase();
    if lower == "and" || lower == "or" || lower == "not" {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Tag,
        )));
    }
    Ok((rest, Query::Word(w)))
}

// ── recursive-descent combinators ──────────────────────────────────────

fn atom(input: &str) -> IResult<&str, Query> {
    preceded(
        multispace0,
        alt((field_term, quoted_phrase, parens, bare_word)),
    )(input)
}

fn not_expr(input: &str) -> IResult<&str, Query> {
    let (rest, has_not) = opt(terminated(
        preceded(multispace0, tag_no_case("NOT")),
        multispace1,
    ))(input)?;
    let (rest, inner) = atom(rest)?;
    if has_not.is_some() {
        Ok((rest, Query::Not(Box::new(inner))))
    } else {
        Ok((rest, inner))
    }
}

fn and_expr(input: &str) -> IResult<&str, Query> {
    let (rest, first) = not_expr(input)?;
    let (rest, rest_exprs) = many0(preceded(
        tuple((multispace0, tag_no_case("AND"), multispace1)),
        not_expr,
    ))(rest)?;
    Ok((
        rest,
        rest_exprs
            .into_iter()
            .fold(first, |acc, e| Query::And(Box::new(acc), Box::new(e))),
    ))
}

fn or_expr(input: &str) -> IResult<&str, Query> {
    let (rest, first) = and_expr(input)?;
    let (rest, rest_exprs) = many0(preceded(
        tuple((multispace0, tag_no_case("OR"), multispace1)),
        and_expr,
    ))(rest)?;
    Ok((
        rest,
        rest_exprs
            .into_iter()
            .fold(first, |acc, e| Query::Or(Box::new(acc), Box::new(e))),
    ))
}

// ── tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── leaf nodes ─────────────────────────────────────────────────

    #[test]
    fn parse_bare_word() {
        assert_eq!(parse_query("hello"), Ok(Query::Word("hello".into())));
    }

    #[test]
    fn parse_field_unquoted() {
        assert_eq!(
            parse_query("from:alice"),
            Ok(Query::Field { prefix: "from".into(), value: "alice".into() })
        );
    }

    #[test]
    fn parse_field_quoted_value() {
        assert_eq!(
            parse_query("subject:\"meeting notes\""),
            Ok(Query::Field { prefix: "subject".into(), value: "meeting notes".into() })
        );
    }

    #[test]
    fn parse_quoted_phrase() {
        assert_eq!(parse_query("\"hello world\""), Ok(Query::Phrase("hello world".into())));
    }

    // ── unary ──────────────────────────────────────────────────────

    #[test]
    fn parse_not() {
        assert_eq!(
            parse_query("NOT hello"),
            Ok(Query::Not(Box::new(Query::Word("hello".into()))))
        );
    }

    #[test]
    fn parse_not_field() {
        assert_eq!(
            parse_query("NOT from:bob"),
            Ok(Query::Not(Box::new(Query::Field {
                prefix: "from".into(),
                value: "bob".into(),
            })))
        );
    }

    // ── binary ─────────────────────────────────────────────────────

    #[test]
    fn parse_simple_and() {
        assert_eq!(
            parse_query("hello AND world"),
            Ok(Query::And(Box::new(Query::Word("hello".into())), Box::new(Query::Word("world".into()))))
        );
    }

    #[test]
    fn parse_simple_or() {
        assert_eq!(
            parse_query("hello OR world"),
            Ok(Query::Or(Box::new(Query::Word("hello".into())), Box::new(Query::Word("world".into()))))
        );
    }

    #[test]
    fn parse_three_way_and() {
        let q = parse_query("a AND b AND c").unwrap();
        // Left-folded: ((a AND b) AND c)
        assert!(matches!(q, Query::And(..)));
        if let Query::And(left, right) = q {
            assert!(matches!(*left, Query::And(..)));
            assert_eq!(*right, Query::Word("c".into()));
        }
    }

    // ── groups / precedence ────────────────────────────────────────

    #[test]
    fn parse_parens() {
        assert_eq!(
            parse_query("(hello)"),
            Ok(Query::Word("hello".into()))
        );
    }

    #[test]
    fn parse_paren_group_or() {
        assert_eq!(
            parse_query("(hello OR world)"),
            Ok(Query::Or(Box::new(Query::Word("hello".into())), Box::new(Query::Word("world".into()))))
        );
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // "a OR b AND c"  should parse as  a OR (b AND c)
        let q = parse_query("a OR b AND c").unwrap();
        assert!(matches!(q, Query::Or(..)));
        if let Query::Or(left, right) = q {
            assert_eq!(*left, Query::Word("a".into()));
            assert!(matches!(*right, Query::And(..)));
        }
    }

    // ── complex ────────────────────────────────────────────────────

    #[test]
    fn complex_field_and_group() {
        let q = parse_query("from:alice AND (subject:foo OR subject:bar)").unwrap();
        assert!(matches!(q, Query::And(..)));
        if let Query::And(left, right) = q {
            assert_eq!(*left, Query::Field { prefix: "from".into(), value: "alice".into() });
            assert!(matches!(*right, Query::Or(..)));
        }
    }

    #[test]
    fn complex_with_not() {
        let q = parse_query("NOT from:bob AND subject:hello").unwrap();
        // AND binds tighter than NOT?  NOT binds tightest.
        // NOT (from:bob) AND subject:hello
        assert!(matches!(q, Query::And(..)));
        if let Query::And(left, right) = q {
            assert!(matches!(*left, Query::Not(..)));
            assert_eq!(*right, Query::Field { prefix: "subject".into(), value: "hello".into() });
        }
    }

    // ── error cases ────────────────────────────────────────────────

    #[test]
    fn reject_unclosed_quote() {
        assert!(parse_query("\"unclosed").is_err());
    }

    #[test]
    fn reject_unclosed_paren() {
        assert!(parse_query("(hello").is_err());
    }

    #[test]
    fn reject_bare_keyword_and() {
        // "AND" alone should not parse as a word
        assert!(parse_query("AND").is_err());
    }

    #[test]
    fn operators_case_insensitive() {
        // AND / and / And all work
        let q_and = parse_query("hello AND world").unwrap();
        let q_and_lower = parse_query("hello and world").unwrap();
        let q_and_mixed = parse_query("hello And world").unwrap();
        assert_eq!(q_and, q_and_lower);
        assert_eq!(q_and, q_and_mixed);

        // OR / or / Or all work
        let q_or = parse_query("hello OR world").unwrap();
        let q_or_lower = parse_query("hello or world").unwrap();
        assert_eq!(q_or, q_or_lower);

        // NOT / not / Not all work
        let q_not = parse_query("NOT hello").unwrap();
        let q_not_lower = parse_query("not hello").unwrap();
        let q_not_mixed = parse_query("Not hello").unwrap();
        assert_eq!(q_not, q_not_lower);
        assert_eq!(q_not, q_not_mixed);
    }

    #[test]
    fn reject_bare_keyword_lowercase() {
        // Lowercase keywords are also rejected as bare words
        assert!(parse_query("and").is_err());
        assert!(parse_query("or").is_err());
        assert!(parse_query("not").is_err());
    }

    #[test]
    fn trailing_junk_is_error() {
        // implicit AND not supported (yet); extra tokens are an error
        assert!(parse_query("hello world").is_err());
    }

    // ── date: parsing ──────────────────────────────────────────────

    #[test]
    fn parse_date_open_start() {
        // 3 days ago and older
        let q = parse_query("date:3d..").unwrap();
        assert_eq!(q, Query::Date(DateRange { start_secs: Some(3 * 86400), end_secs: None }));
    }

    #[test]
    fn parse_date_open_end() {
        // within the last week
        let q = parse_query("date:..1w").unwrap();
        assert_eq!(q, Query::Date(DateRange { start_secs: None, end_secs: Some(7 * 86400) }));
    }

    #[test]
    fn parse_date_bounded() {
        // between 1 and 2 weeks ago
        let q = parse_query("date:2w..1w").unwrap();
        assert_eq!(q, Query::Date(DateRange {
            start_secs: Some(2 * 7 * 86400),
            end_secs: Some(1 * 7 * 86400),
        }));
    }

    #[test]
    fn parse_date_zero_all_time() {
        let q = parse_query("date:0d..").unwrap();
        assert_eq!(q, Query::Date(DateRange { start_secs: Some(0), end_secs: None }));
    }

    #[test]
    fn parse_date_mixed_units() {
        // 1 week to 3 days ago → older=1w, newer=3d
        let q = parse_query("date:1w..3d").unwrap();
        assert_eq!(q, Query::Date(DateRange {
            start_secs: Some(7 * 86400),
            end_secs: Some(3 * 86400),
        }));
    }

    #[test]
    fn parse_date_minutes_and_hours() {
        let q = parse_query("date:30m..1h").unwrap();
        assert_eq!(q, Query::Date(DateRange {
            start_secs: Some(3600),
            end_secs: Some(30 * 60),
        }));
    }

    #[test]
    fn parse_date_months() {
        let q = parse_query("date:6mo..").unwrap();
        assert_eq!(q, Query::Date(DateRange { start_secs: Some(6 * 30 * 86400), end_secs: None }));
    }

    #[test]
    fn parse_date_years() {
        let q = parse_query("date:..2y").unwrap();
        assert_eq!(q, Query::Date(DateRange { start_secs: None, end_secs: Some(2 * 365 * 86400) }));
    }

    #[test]
    fn parse_date_huge_offset_does_not_overflow() {
        // Previously `n * unit` could overflow i64 and panic (debug) on the
        // worker thread. A long run of digits is clamped, and resolve() must
        // not panic.
        let q = parse_query("date:99999999999999999999y..").unwrap();
        if let Query::Date(range) = q {
            // Clamped to i64::MAX, resolves to "beginning of time" without panic.
            let (start, end) = range.resolve();
            assert!(start <= end);
            assert_eq!(start, 0);
        } else {
            panic!("expected Date");
        }
    }

    #[test]
    fn parse_date_max_unit_no_overflow() {
        // Largest unit (year) with a 9-digit count must not overflow.
        let q = parse_query("date:999999999y..").unwrap();
        if let Query::Date(range) = q {
            let _ = range.resolve(); // must not panic
        } else {
            panic!("expected Date");
        }
    }

    #[test]
    fn parse_date_with_other_terms() {
        let q = parse_query("hello AND date:3d..").unwrap();
        assert!(matches!(q, Query::And(..)));
    }

    // ── FTS5 bridge ────────────────────────────────────────────────

    #[test]
    fn fts5_word() {
        let q = Query::Word("hello".into());
        assert_eq!(query_to_fts5(&q), "hello*");
    }

    #[test]
    fn fts5_phrase() {
        let q = Query::Phrase("hello world".into());
        assert_eq!(query_to_fts5(&q), "\"hello world\"");
    }

    #[test]
    fn fts5_and() {
        let q = Query::And(
            Box::new(Query::Word("a".into())),
            Box::new(Query::Word("b".into())),
        );
        assert_eq!(query_to_fts5(&q), "(a*) AND (b*)");
    }

    #[test]
    fn fts5_not() {
        let q = Query::Not(Box::new(Query::Word("spam".into())));
        assert_eq!(query_to_fts5(&q), "NOT (spam*)");
    }

    #[test]
    fn empty_query_all() {
        assert_eq!(ParsedQuery::all().fts5, String::new());
    }

    // ── prefix → column mapping ────────────────────────────────────

    #[test]
    fn fts5_subject_prefix() {
        let q = Query::Field { prefix: "s".into(), value: "hello".into() };
        assert_eq!(query_to_fts5(&q), "subject:\"hello\"");
    }

    #[test]
    fn fts5_from_prefix() {
        let q = Query::Field { prefix: "from".into(), value: "alice".into() };
        assert_eq!(query_to_fts5(&q), "from:\"alice\"");
    }

    #[test]
    fn fts5_field_phrase() {
        let q = Query::Field { prefix: "subject".into(), value: "meeting notes".into() };
        assert_eq!(query_to_fts5(&q), "subject:\"meeting notes\"");
    }

    #[test]
    fn fts5_addr_prefix_spans_from_to_cc() {
        let q = Query::Field { prefix: "a".into(), value: "me@example.com".into() };
        assert_eq!(
            query_to_fts5(&q),
            "(from:\"me@example.com\" OR to:\"me@example.com\" OR cc:\"me@example.com\")"
        );
    }

    #[test]
    fn fts5_list_prefix() {
        let q = Query::Field { prefix: "l".into(), value: "linux-nvme.lists.infradead.org".into() };
        assert_eq!(query_to_fts5(&q), "list_id:\"linux-nvme.lists.infradead.org\"");
    }

    // ── diff-path prefix (dfn:/file:/path:) ────────────────────────

    #[test]
    fn fts5_dfn_prefix_is_body_phrase() {
        // Mirrors public-inbox's `dfn:` locally as a body-phrase over the diff.
        let q = parse_query("dfn:lib/xarray.c").unwrap();
        assert_eq!(query_to_fts5(&q), "body:\"lib/xarray.c\"");
        // `file:`/`path:` are aliases.
        assert_eq!(
            query_to_fts5(&parse_query("file:include/linux/idr.h").unwrap()),
            "body:\"include/linux/idr.h\""
        );
        assert_eq!(
            query_to_fts5(&parse_query("path:lib/idr.c").unwrap()),
            "body:\"lib/idr.c\""
        );
    }

    #[test]
    fn fts5_dfn_strips_trailing_glob() {
        // A MAINTAINERS `F:` directory entry (trailing `/*` or `/`) becomes the
        // bare directory; the phrase then matches every path beneath it.
        assert_eq!(
            query_to_fts5(&parse_query("dfn:tools/testing/radix-tree/*").unwrap()),
            "body:\"tools/testing/radix-tree\""
        );
        assert_eq!(
            query_to_fts5(&parse_query("dfn:tools/testing/radix-tree/").unwrap()),
            "body:\"tools/testing/radix-tree\""
        );
    }

    #[test]
    fn dfn_needs_body_and_is_false_in_memory() {
        // The diff lives in the body, which the cache does not hold, so a
        // `dfn:` term must route through the FTS path.
        let q = parse_query("dfn:lib/xarray.c").unwrap();
        assert!(needs_body(&q));
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        assert!(!matches(&q, &m));
    }

    #[test]
    fn dfn_composes_with_or_and_date() {
        // The XArray subsystem shape: OR of diff paths, bounded by a date range.
        let q = parse_query(
            "(dfn:lib/xarray.c OR dfn:include/linux/xarray.h) AND date:10y..",
        )
        .unwrap();
        let pq = ParsedQuery::from_ast(&q, 50);
        assert!(pq.fts5.contains("body:\"lib/xarray.c\""));
        assert!(pq.fts5.contains("body:\"include/linux/xarray.h\""));
        assert!(pq.date_range.is_some());
    }

    // ── archived state (is:) ──────────────────────────────────────
    #[test]
    fn archived_filter_detection() {
        let f = |s: &str| archived_filter(&parse_query(s).unwrap());
        assert_eq!(f("l:linux-mm.kvack.org"), ArchivedFilter::Active);
        assert_eq!(f("is:active"), ArchivedFilter::Active);
        assert_eq!(f("is:archived"), ArchivedFilter::Archived);
        assert_eq!(f("is:any"), ArchivedFilter::Any);
        assert_eq!(f("l:linux-mm.kvack.org AND is:archived"), ArchivedFilter::Archived);
        assert_eq!(f("l:linux-mm.kvack.org AND is:any"), ArchivedFilter::Any);
    }

    #[test]
    fn is_predicate_is_empty_in_fts_and_passthrough_in_matches() {
        // `is:` contributes no FTS text and is a no-op in the in-memory match.
        let q = parse_query("is:archived").unwrap();
        assert_eq!(query_to_fts5(&q), "");
        let msg = FilterFields {
            subject: Some("hi"), from: None, to: None, cc: None,
            list_id: Some("linux-mm.kvack.org"), received_ts: 0,
        };
        assert!(matches(&q, &msg)); // membership filter applied separately
    }

    #[test]
    fn from_ast_carries_archived_filter() {
        let q = parse_query("l:linux-mm.kvack.org AND is:archived").unwrap();
        let pq = ParsedQuery::from_ast(&q, 50);
        assert_eq!(pq.archived, ArchivedFilter::Archived);
        // The list term still drives the FTS expression.
        assert!(pq.fts5.contains("list_id:"));
    }

    #[test]
    fn fts5_unknown_prefix_all_columns() {
        let q = Query::Field { prefix: "xyz".into(), value: "hello".into() };
        assert_eq!(query_to_fts5(&q), "hello*");
    }

    #[test]
    fn fts5_date_produces_empty() {
        let q = Query::Date(DateRange { start_secs: Some(86400), end_secs: None });
        assert_eq!(query_to_fts5(&q), "");
    }

    // ── build_query date extraction ────────────────────────────────

    #[test]
    fn build_query_extracts_date_from_and() {
        let q = parse_query("hello AND date:3d..").unwrap();
        let pq = ParsedQuery::from_ast(&q, 50);
        assert_eq!(pq.fts5, "hello*");
        assert!(pq.date_range.is_some());
    }

    #[test]
    fn build_query_no_date() {
        let q = parse_query("hello").unwrap();
        let pq = ParsedQuery::from_ast(&q, 50);
        assert_eq!(pq.fts5, "hello*");
        assert!(pq.date_range.is_none());
    }

    // ── in-memory evaluator ────────────────────────────────────────

    fn sample() -> (String, String, String, String, String) {
        (
            "Re: nvme: fix oops".to_string(),
            "Christoph Hellwig <hch@lst.de>".to_string(),
            "Jens Axboe <axboe@kernel.dk>".to_string(),
            "linux-block@vger.kernel.org".to_string(),
            "linux-block.vger.kernel.org".to_string(),
        )
    }

    fn fields<'a>(
        s: &'a str,
        f: &'a str,
        t: &'a str,
        c: &'a str,
        l: &'a str,
        ts: i64,
    ) -> FilterFields<'a> {
        FilterFields {
            subject: Some(s),
            from: Some(f),
            to: Some(t),
            cc: Some(c),
            list_id: Some(l),
            received_ts: ts,
        }
    }

    #[test]
    fn matches_list_filter() {
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        assert!(matches(&parse_query("l:linux-block.vger.kernel.org").unwrap(), &m));
        // Partial / contains also matches.
        assert!(matches(&parse_query("list:linux-block").unwrap(), &m));
        assert!(!matches(&parse_query("l:linux-nvme.lists.infradead.org").unwrap(), &m));
    }

    #[test]
    fn matches_list_filter_normalised_value() {
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        // User pastes the full raw List-Id; normalisation strips the brackets.
        let q = parse_query("l:\"Linux block <linux-block.vger.kernel.org>\"").unwrap();
        assert!(matches(&q, &m));
    }

    #[test]
    fn matches_from_and_addr() {
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        assert!(matches(&parse_query("from:hch@lst.de").unwrap(), &m));
        assert!(matches(&parse_query("f:hellwig").unwrap(), &m));
        // a: spans from/to/cc — axboe is in To.
        assert!(matches(&parse_query("a:axboe@kernel.dk").unwrap(), &m));
        assert!(!matches(&parse_query("from:axboe").unwrap(), &m));
    }

    #[test]
    fn matches_subject_and_bare_word() {
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        assert!(matches(&parse_query("s:oops").unwrap(), &m));
        assert!(matches(&parse_query("subject:\"fix oops\"").unwrap(), &m));
        // Bare word searches all text fields (here it hits the subject).
        assert!(matches(&parse_query("nvme").unwrap(), &m));
        // Bare word also hits the from field.
        assert!(matches(&parse_query("hellwig").unwrap(), &m));
        assert!(!matches(&parse_query("nonexistentxyz").unwrap(), &m));
    }

    #[test]
    fn matches_date_range() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (s, f, t, c, l) = sample();
        // Message from ~2 days ago.
        let recent = fields(&s, &f, &t, &c, &l, now - 2 * 86400);
        let old = fields(&s, &f, &t, &c, &l, now - 30 * 86400);
        // `date:7d..` → range [now-7d, now] = "within the last 7 days".
        let within_7d = parse_query("date:7d..").unwrap();
        assert!(matches(&within_7d, &recent)); // 2d ago is within the last 7d
        assert!(!matches(&within_7d, &old)); // 30d ago is not
        // `date:..1w` → range [epoch, now-1w] = "older than a week".
        let older_than_1w = parse_query("date:..1w").unwrap();
        assert!(matches(&older_than_1w, &old));
        assert!(!matches(&older_than_1w, &recent));
    }

    #[test]
    fn matches_boolean_and_not() {
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        assert!(matches(&parse_query("from:hch AND s:oops").unwrap(), &m));
        assert!(!matches(&parse_query("from:hch AND s:nonexistent").unwrap(), &m));
        assert!(matches(&parse_query("NOT from:axboe").unwrap(), &m));
        assert!(matches(&parse_query("from:axboe OR s:oops").unwrap(), &m));
    }

    #[test]
    fn matches_body_term_is_false_but_flagged() {
        let (s, f, t, c, l) = sample();
        let m = fields(&s, &f, &t, &c, &l, 100);
        let q = parse_query("b:somebodytext").unwrap();
        assert!(!matches(&q, &m));
        assert!(needs_body(&q));
        assert!(needs_body(&parse_query("from:hch AND body:foo").unwrap()));
        assert!(!needs_body(&parse_query("from:hch AND s:oops").unwrap()));
        assert!(!needs_body(&parse_query("plainword").unwrap()));
    }

    #[test]
    fn matches_handles_missing_fields() {
        let m = FilterFields {
            subject: Some("hello"),
            from: None,
            to: None,
            cc: None,
            list_id: None,
            received_ts: 0,
        };
        assert!(!matches(&parse_query("l:anything").unwrap(), &m));
        assert!(!matches(&parse_query("from:anyone").unwrap(), &m));
        assert!(matches(&parse_query("s:hello").unwrap(), &m));
    }
}
