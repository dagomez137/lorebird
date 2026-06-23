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
        let start = self.start_secs.map(|s| now - s).unwrap_or(0);
        let end = self.end_secs.map(|e| now - e).unwrap_or(now);
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

/// Materialised query ready to hand to SQLite.
#[derive(Debug, Clone)]
pub struct ParsedQuery {
    /// FTS5 expression string (safe to pass directly to SQLite).
    pub fts5: String,
    /// Optional date range filter (applied via JOIN on `mail_ndx`).
    pub date_range: Option<DateRange>,
    /// Limit on the number of results.
    pub limit: i64,
}

impl ParsedQuery {
    /// Create a query that matches everything.
    pub fn all() -> Self {
        Self { fts5: String::new(), date_range: None, limit: 50 }
    }

    /// Build a materialised query from a parsed AST.
    pub fn from_ast(query: &Query, limit: i64) -> Self {
        let (fts5, date_range) = Self::build(query);
        Self { fts5, date_range, limit }
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

/// Search the FTS5 index and return matching message IDs.
pub fn search(conn: &Connection, query: &ParsedQuery) -> SqlResult<Vec<String>> {
    // ── date-filtered path ──
    if let Some(ref range) = query.date_range {
        let (start_ts, end_ts) = range.resolve();
        if query.fts5.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT message_id FROM mail_ndx
                 WHERE received_ts >= ?1 AND received_ts <= ?2
                 ORDER BY received_ts DESC LIMIT ?3",
            )?;
            return stmt
                .query_map(params![start_ts, end_ts, query.limit], |r| r.get(0))?
                .collect::<SqlResult<Vec<String>>>();
        }
        let mut stmt = conn.prepare(
            "SELECT f.message_id
             FROM mail_fts f
             JOIN mail_ndx n USING (message_id)
             WHERE mail_fts MATCH ?1
               AND n.received_ts >= ?2 AND n.received_ts <= ?3
             ORDER BY n.received_ts DESC
             LIMIT ?4",
        )?;
        return stmt
            .query_map(
                params![&query.fts5, start_ts, end_ts, query.limit],
                |r| r.get(0),
            )?
            .collect::<SqlResult<Vec<String>>>();
    }

    // ── no date filter ──
    if query.fts5.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT message_id FROM mail_ndx ORDER BY received_ts DESC LIMIT ?1",
        )?;
        return stmt
            .query_map(params![query.limit], |r| r.get(0))?
            .collect::<SqlResult<Vec<String>>>();
    }

    let mut stmt = conn.prepare(
        "SELECT message_id FROM mail_fts WHERE mail_fts MATCH ?1 LIMIT ?2",
    )?;
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

/// Parse a non-negative integer.
fn date_number(input: &str) -> IResult<&str, i64> {
    map_res(digit1, |s: &str| s.parse::<i64>())(input)
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
    Ok((rest, DateRange { start_secs: Some(n * unit), end_secs: None }))
}

/// `..N<unit>`
fn date_open_end(input: &str) -> IResult<&str, DateRange> {
    let (rest, _) = tag("..")(input)?;
    let (rest, n) = date_number(rest)?;
    let (rest, unit) = date_unit(rest)?;
    Ok((rest, DateRange { start_secs: None, end_secs: Some(n * unit) }))
}

/// `N1<unit>..N2<unit>`
fn date_bounded(input: &str) -> IResult<&str, DateRange> {
    let (rest, n1) = date_number(input)?;
    let (rest, u1) = date_unit(rest)?;
    let (rest, _) = tag("..")(rest)?;
    let (rest, n2) = date_number(rest)?;
    let (rest, u2) = date_unit(rest)?;
    let secs1 = n1 * u1;
    let secs2 = n2 * u2;
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
}
