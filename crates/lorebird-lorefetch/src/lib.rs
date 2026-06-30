// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2025 Jesper Devantier <jwd@defmacro.it>
// Ported from Go to Rust by the lorebird project.

//! Fetch mailing-list threads from lore.kernel.org into a maildir.
//!
//! This crate contacts a public-inbox instance, solves the Anubis
//! bot-protection challenge if required, streams the gzipped mbox
//! response through a pipeline (decompress → parse → dedup → write),
//! and writes new messages into a maildir using SHA-1 hashes of the
//! Message-ID as filenames.
//!
//! Streaming design: the HTTP response is never buffered in full
//! (except for Anubis challenge pages, which are tiny HTML).  Messages
//! are decompressed and parsed one at a time via `MboxParser`, written
//! to disk immediately, and deduplicated against the cache.
//!
//! Body timeout: ureq 3.3 only exposes *total* duration timeouts
//! (`timeout_recv_body`), not a per-read idle/stall timeout, so the body
//! timeout here is a wall-clock cap on the entire body transfer — NOT a
//! "no bytes for N seconds" stall detector.  It is deliberately generous
//! ([`RECV_BODY_TIMEOUT_SECS`]) so a large but healthy streamed fetch is
//! not truncated mid-stream.  On a timeout the partial fetch is preserved
//! (`FetchResult.timed_out`) and the next incremental run resumes from the
//! newest `Date:` already seen.
//!
//! Incremental fetches use per-query `last_date` tracking: on
//! subsequent fetches for the same query, a `dt:` range is injected
//! to only pull new mail (with a 1-day overlap for safety).

use std::collections::{HashMap, HashSet};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1 as Sha1Hasher};
use sha2::Sha256;

// ── Constants ────────────────────────────────────────────────────────

const BASE_URL: &str = "https://lore.kernel.org";
const USER_AGENT: &str = "Lorefetch/1.x (https://github.com/jwdevantier/lorefetch)";
/// Total wall-clock cap on receiving the response body (ureq's
/// `timeout_recv_body`). This is NOT an idle/stall timeout — ureq 3.3 does
/// not expose one — so it must be large enough to stream a big (gzipped) mbox
/// over a slow link without truncating. A full-list initial fetch can be
/// hundreds of MB decompressed; 30s was far too short and silently truncated
/// such fetches. 30 minutes leaves ample headroom while still bounding a
/// genuinely dead connection.
const RECV_BODY_TIMEOUT_SECS: u64 = 30 * 60;
const CACHE_VERSION: i32 = 2;
const CACHE_FILENAME: &str = ".lorefetch-cache.json";

#[cfg(unix)]
const MAIL_FILE_MODE: u32 = 0o644;
#[cfg(unix)]
const MAILDIR_MODE: u32 = 0o755;

// ── Error type ───────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum LoreError {
    #[error("HTTP request failed: {0}")]
    Http(String),

    #[error("Anubis challenge: {0}")]
    Anubis(String),

    #[error("Mbox is empty or contains no messages")]
    EmptyMbox,

    #[error("Message has no Message-ID header (message {index})")]
    MissingMessageId { index: usize },

    #[error("Maildir: {0}")]
    Maildir(String),

    #[error("Cache I/O: {0}")]
    CacheIo(#[from] std::io::Error),

    #[error("Cache format: {0}")]
    CacheFormat(String),

    #[error("Response is not mbox format")]
    NotMbox,

    #[error("Body transfer exceeded the {0}s total timeout (partial fetch saved)")]
    ReadTimeout(u64),

    #[error("No results found")]
    NoResults,
}

// ── Result type ──────────────────────────────────────────────────────

/// Result of a fetch operation.
#[derive(Debug)]
pub struct FetchResult {
    /// Number of new messages written to the maildir.
    pub new_messages: usize,
    /// Total messages seen (new + already cached).
    pub total_messages: usize,
    /// Whether a read timeout occurred (partial data may have been saved).
    pub timed_out: bool,
}

// ── Cache ─────────────────────────────────────────────────────────────

/// Per-query state for incremental fetches.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct QueryState {
    last_date: String,
}

/// Maildir cache: tracks which SHA-1 filenames exist on disk and
/// per-query incremental state.
#[derive(Serialize, Deserialize)]
struct MaildirCache {
    version: i32,
    cache: HashSet<String>,
    queries: HashMap<String, QueryState>,
}

impl MaildirCache {
    #[allow(dead_code)]
    fn new() -> Self {
        Self { version: CACHE_VERSION, cache: HashSet::new(), queries: HashMap::new() }
    }

    fn load_or_init(maildir: &Path) -> Result<Self, LoreError> {
        let path = maildir.join(CACHE_FILENAME);
        if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            match serde_json::from_str::<MaildirCache>(&data) {
                Ok(cache) if cache.version == CACHE_VERSION => Ok(cache),
                _ => {
                    let _ = std::fs::remove_file(&path);
                    Self::init_from_maildir(maildir)
                }
            }
        } else {
            Self::init_from_maildir(maildir)
        }
    }

    fn init_from_maildir(maildir: &Path) -> Result<Self, LoreError> {
        let mut cache = HashSet::new();
        let new_dir = maildir.join("new");
        if new_dir.is_dir() {
            for entry in std::fs::read_dir(&new_dir)? {
                cache.insert(entry?.file_name().to_string_lossy().to_string());
            }
        }
        let cur_dir = maildir.join("cur");
        if cur_dir.is_dir() {
            for entry in std::fs::read_dir(&cur_dir)? {
                let name = entry?.file_name().to_string_lossy().to_string();
                if let Some(idx) = name.rfind(":2") {
                    cache.insert(name[..idx].to_string());
                }
            }
        }
        Ok(Self { version: CACHE_VERSION, cache, queries: HashMap::new() })
    }

    fn exists(&self, key: &str) -> bool { self.cache.contains(key) }
    fn add(&mut self, key: &str) { self.cache.insert(key.to_string()); }
    fn query_last_date(&self, query: &str) -> Option<&str> {
        self.queries.get(query).map(|qs| qs.last_date.as_str())
    }
    fn update_query(&mut self, query: &str, last_date: &str) {
        self.queries.insert(query.to_string(), QueryState { last_date: last_date.to_string() });
    }
    fn save(&self, maildir: &Path) -> Result<(), LoreError> {
        let path = maildir.join(CACHE_FILENAME);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| LoreError::CacheFormat(e.to_string()))?;
        std::fs::write(&tmp, &data)?;
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

// ── Anubis challenge ─────────────────────────────────────────────────

struct AnubisChallenge { challenge: String, difficulty: usize }

#[derive(Deserialize)]
struct AnubisChallengeJson { challenge: String, rules: AnubisRulesJson }

#[derive(Deserialize)]
struct AnubisRulesJson { #[allow(dead_code)] algorithm: String, difficulty: usize, #[allow(dead_code)] report_as: usize }

fn detect_anubis(body: &str) -> Option<AnubisChallenge> {
    if !body.contains("anubis_challenge") && !body.contains("Making sure you&#39;re not a bot") {
        return None;
    }
    let start_tag = r#"<script id="anubis_challenge" type="application/json">"#;
    let s_idx = body.find(start_tag)?;
    let content_start = s_idx + start_tag.len();
    let json = &body[content_start..body[content_start..].find("</script>")? + content_start];
    let parsed: AnubisChallengeJson = serde_json::from_str(json).ok()?;
    Some(AnubisChallenge { challenge: parsed.challenge, difficulty: parsed.rules.difficulty })
}

fn solve_challenge(challenge: &str, difficulty: usize) -> (String, usize) {
    let prefix = "0".repeat(difficulty);
    for nonce in 0usize.. {
        let hash = Sha256::digest(format!("{}{}", challenge, nonce).as_bytes());
        let hex = format!("{:x}", hash);
        if hex.starts_with(&prefix) { return (hex, nonce); }
    }
    unreachable!()
}

// ── Streaming mbox parser ────────────────────────────────────────────

/// Streaming mbox parser that yields one complete RFC 2822 message at
/// a time from any `Read` source.
///
/// Handles mboxrd `>From ` escaping: lines starting with `>+From `
/// have one leading `>` stripped.
struct MboxParser<R: Read> {
    reader: std::io::BufReader<R>,
    line_buf: Vec<u8>,
    msg_buf: Vec<u8>,
    msg_index: usize,
}

impl<R: Read> MboxParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: std::io::BufReader::with_capacity(64 * 1024, reader),
            line_buf: Vec::with_capacity(4096),
            msg_buf: Vec::with_capacity(64 * 1024),
            msg_index: 0,
        }
    }

    /// Read the next complete message from the mbox stream.
    /// Returns `Ok(Some((index, bytes)))`, `Ok(None)` at EOF, or timeout error.
    fn next_message(&mut self) -> Result<Option<(usize, Vec<u8>)>, LoreError> {
        loop {
            self.line_buf.clear();
            let n = self.read_line()?;

            if n == 0 {
                if self.msg_buf.is_empty() { return Ok(None); }
                let msg = unescape_mboxrd(&self.msg_buf);
                let idx = self.msg_index;
                self.msg_index += 1;
                self.msg_buf.clear();
                return Ok(Some((idx, msg)));
            }

            if self.line_buf.starts_with(b"From ") && !self.msg_buf.is_empty() {
                let msg = unescape_mboxrd(&self.msg_buf);
                let idx = self.msg_index;
                self.msg_index += 1;
                self.msg_buf.clear();
                return Ok(Some((idx, msg)));
            }

            // Accumulate this line into the current message buffer
            // (read_line includes the trailing \n, so lines are
            // already separated)
            self.msg_buf.extend_from_slice(&self.line_buf);
        }
    }

    /// Read one line (including the trailing `\n`) into `line_buf`, returning
    /// the number of bytes read (`0` at EOF). Uses the buffered reader's
    /// `read_until` rather than a byte-at-a-time loop, so each line costs a
    /// single buffer scan instead of one syscall-shaped read per byte.
    fn read_line(&mut self) -> Result<usize, LoreError> {
        use std::io::BufRead;
        match self.reader.read_until(b'\n', &mut self.line_buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.to_string().contains("timed out")
                    || e.to_string().contains("Timeout")
                {
                    Err(LoreError::ReadTimeout(RECV_BODY_TIMEOUT_SECS))
                } else {
                    Err(LoreError::Http(format!("read error: {}", e)))
                }
            }
        }
    }
}

/// Un-escape mboxrd `>From ` quoting.
fn unescape_mboxrd(data: &[u8]) -> Vec<u8> {
    let input = String::from_utf8_lossy(data);
    let mut output = Vec::with_capacity(data.len());
    for line in input.lines() {
        let leading = line.chars().take_while(|c| *c == '>').count();
        if leading > 0 && line[leading..].starts_with("From ") {
            output.extend_from_slice(line[1..].as_bytes());
        } else {
            output.extend_from_slice(line.as_bytes());
        }
        output.push(b'\n');
    }
    output
}

// ── Date parsing for incremental queries ──────────────────────────────

fn parse_date_string(s: &str) -> Option<String> {
    const MONTHS: &[(&str, u8)] = &[
        ("Jan",1),("Feb",2),("Mar",3),("Apr",4),("May",5),("Jun",6),
        ("Jul",7),("Aug",8),("Sep",9),("Oct",10),("Nov",11),("Dec",12),
    ];
    let parts: Vec<&str> = s.split_whitespace().collect();
    let mut day: Option<u32> = None;
    let mut month: Option<u8> = None;
    let mut year: Option<u32> = None;
    for part in &parts {
        let t = part.trim_end_matches(',');
        if let Ok(d) = t.parse::<u32>() {
            if d >= 1 && d <= 31 && day.is_none() { day = Some(d); }
            else if d >= 1990 && d <= 2099 { year = Some(d); }
            continue;
        }
        for (name, num) in MONTHS {
            if part.eq_ignore_ascii_case(name) { month = Some(*num); break; }
        }
    }
    match (year, month, day) {
        (Some(y), Some(m), Some(d)) => Some(format!("{:04}-{:02}-{:02}", y, m, d)),
        _ => None,
    }
}

/// Byte offset of `prefix` where it begins a query token (start of string or
/// after a space or `(`), so e.g. `rt:` doesn't match inside `s:support:`.
fn find_prefix(query: &str, prefix: &str) -> Option<usize> {
    let bytes = query.as_bytes();
    let mut from = 0;
    while let Some(rel) = query[from..].find(prefix) {
        let idx = from + rel;
        if idx == 0 || matches!(bytes[idx - 1], b' ' | b'(') {
            return Some(idx);
        }
        from = idx + prefix.len();
    }
    None
}

fn build_incremental_query(query: &str, last_date: &str) -> String {
    let ymd: Vec<&str> = last_date.split('-').collect();
    if ymd.len() != 3 { return query.to_string(); }
    let (Ok(year), Ok(month), Ok(day)) = (ymd[0].parse::<i32>(), ymd[1].parse::<i32>(), ymd[2].parse::<i32>()) else {
        return query.to_string();
    };
    if year == 0 || month == 0 || day == 0 { return query.to_string(); }
    let (y, m, d) = subtract_one_day(year, month, day);
    let rt_lower = format!("{:04}{:02}{:02}", y, m, d);
    // Bound by received time (`rt:`), not sent Date (`dt:`/`d:`): a message can
    // arrive at lore well after the date in its own header, so a `dt:` cutoff
    // silently skips late-arriving mail. Any existing `rt:` clause is rewritten
    // to start at our incremental cutoff, keeping its upper bound if present.
    if let Some(rt_start) = find_prefix(query, "rt:") {
        let after_rt = &query[rt_start..];
        let rt_end = after_rt.find(' ').map(|i| rt_start + i).unwrap_or(query.len());
        let rt_clause = &query[rt_start..rt_end];
        if let Some(dotdot) = rt_clause.find("..") {
            format!("{}rt:{}{}{}",
                &query[..rt_start], rt_lower, &rt_clause[dotdot..], &query[rt_end..])
        } else {
            format!("{}rt:{}..{}", &query[..rt_start], rt_lower, &query[rt_end..])
        }
    } else {
        format!("{} rt:{}..", query.trim_end(), rt_lower)
    }
}

fn subtract_one_day(year: i32, month: i32, day: i32) -> (i32, i32, i32) {
    const DIM: [i32; 12] = [31,28,31,30,31,30,31,31,30,31,30,31];
    if day > 1 { (year, month, day - 1) }
    else if month > 1 {
        let pm = month - 1;
        (year, pm, if pm == 2 && is_leap_year(year) { 29 } else { DIM[(pm-1) as usize] })
    } else { (year - 1, 12, 31) }
}
fn is_leap_year(y: i32) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }

// ── Maildir helpers ──────────────────────────────────────────────────

fn validate_maildir(path: &Path) -> Result<(), LoreError> {
    for s in &["cur","new","tmp"] {
        if !path.join(s).is_dir() {
            return Err(LoreError::Maildir(format!("invalid maildir '{}' — '{}' missing", path.display(), s)));
        }
    }
    Ok(())
}
fn create_maildir(path: &Path) -> Result<(), LoreError> {
    for s in &["cur","new","tmp"] {
        let p = path.join(s);
        std::fs::create_dir_all(&p).map_err(|e| LoreError::Maildir(format!("creating {}: {}", p.display(), e)))?;
        #[cfg(unix)] { std::fs::set_permissions(&p, std::fs::Permissions::from_mode(MAILDIR_MODE))?; }
    }
    Ok(())
}

// ── HTTP client ──────────────────────────────────────────────────────

/// How many bytes to peek to classify a response (mbox vs gzip vs HTML
/// challenge) without buffering the whole body.
const PEEK_SIZE: usize = 8192;

/// Result of a single search request: either a streaming mbox reader or the
/// (small) HTML body of a challenge / error page.
enum StreamOrHtml {
    Stream(Box<dyn Read>),
    Html(String),
}

/// Map an I/O error from a body read to the right `LoreError`.
fn map_read_err(e: std::io::Error) -> LoreError {
    if e.kind() == std::io::ErrorKind::TimedOut
        || e.to_string().contains("timed out")
        || e.to_string().contains("Timeout")
    {
        LoreError::ReadTimeout(RECV_BODY_TIMEOUT_SECS)
    } else {
        LoreError::Http(format!("reading response: {}", e))
    }
}

/// Read up to `n` bytes, tolerating short reads (returns fewer at EOF).
fn read_prefix<R: Read>(r: &mut R, n: usize) -> Result<Vec<u8>, LoreError> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(k) => filled += k,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(map_read_err(e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

pub struct LoreClient {
    base_url: String,
    verbose: bool,
}

impl LoreClient {
    pub fn new() -> Self { Self { base_url: BASE_URL.to_string(), verbose: false } }
    pub fn with_base_url(url: &str) -> Self { Self { base_url: url.to_string(), verbose: false } }
    pub fn verbose(mut self, v: bool) -> Self { self.verbose = v; self }

    fn build_agent(&self) -> ureq::Agent {
        use ureq::config::Config;
        Config::builder()
            .timeout_recv_body(Some(Duration::from_secs(RECV_BODY_TIMEOUT_SECS)))
            // public-inbox answers a zero-result mbox download with HTTP 404;
            // we want to inspect the status ourselves rather than have ureq
            // turn every non-2xx into a transport error.
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn search_url(&self, query: &str, list: Option<&str>) -> String {
        let base = match list {
            Some(l) => format!("{}/{}/", self.base_url, l),
            None => format!("{}/all/", self.base_url),
        };
        format!("{}?q={}&x=m", base, urlencoding(query))
    }

    /// Issue the search POST and return a streaming classification of the
    /// response. Only a small prefix is buffered to tell mbox from a gzip
    /// stream from an HTML challenge — the mbox body itself is never fully
    /// buffered, so memory stays constant regardless of size.
    fn post_stream(&self, agent: &ureq::Agent, search_url: &str) -> Result<StreamOrHtml, LoreError> {
        let response = agent
            .post(search_url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.5")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Origin", &self.base_url)
            .header("Connection", "keep-alive")
            .header("Referer", search_url)
            .header("Upgrade-Insecure-Requests", "1")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-User", "?1")
            .header("Priority", "u=0, i")
            .send_form([("x", "full threads")])
            .map_err(|e| LoreError::Http(format!("POST request failed: {}", e)))?;

        let status = response.status().as_u16();
        let mut reader = response.into_body().into_reader();
        let head = read_prefix(&mut reader, PEEK_SIZE)?;

        // public-inbox answers a zero-match search with 404 "No results found"
        // — a normal "caught up" outcome. Other 404s are genuine errors.
        if status == 404 {
            if String::from_utf8_lossy(&head).contains("No results found") {
                return Err(LoreError::NoResults);
            }
            return Err(LoreError::Http(format!("request returned status {}", status)));
        }
        if status != 200 {
            return Err(LoreError::Http(format!("request returned status {}", status)));
        }

        // Gzip stream (lore serves the mbox gzip-compressed via x=m).
        if head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b {
            let chained = std::io::Cursor::new(head).chain(reader);
            let mut gz = flate2::read::GzDecoder::new(chained);
            let dhead = read_prefix(&mut gz, 4096)?;
            if dhead.starts_with(b"From ") {
                return Ok(StreamOrHtml::Stream(Box::new(std::io::Cursor::new(dhead).chain(gz))));
            }
            // Gzipped HTML — a (small) challenge or error page.
            let mut rest = Vec::new();
            gz.read_to_end(&mut rest).map_err(map_read_err)?;
            let mut full = dhead;
            full.extend_from_slice(&rest);
            return Ok(StreamOrHtml::Html(String::from_utf8_lossy(&full).into_owned()));
        }

        // Raw (uncompressed) mbox.
        if head.starts_with(b"From ") {
            return Ok(StreamOrHtml::Stream(Box::new(std::io::Cursor::new(head).chain(reader))));
        }

        // HTML — read the (bounded) remainder so Anubis can be detected.
        let mut rest = Vec::new();
        std::io::Read::take(&mut reader, 4 * 1024 * 1024)
            .read_to_end(&mut rest)
            .map_err(map_read_err)?;
        let mut full = head;
        full.extend_from_slice(&rest);
        Ok(StreamOrHtml::Html(String::from_utf8_lossy(&full).into_owned()))
    }

    /// Fetch a streaming mbox reader, transparently solving an Anubis
    /// challenge and retrying once if one is presented.
    fn fetch_response_reader(&self, query: &str, list: Option<&str>) -> Result<Box<dyn Read>, LoreError> {
        let agent = self.build_agent();
        let search_url = self.search_url(query, list);
        if self.verbose { eprintln!("[lorefetch] Fetching: {}", search_url); }

        match self.post_stream(&agent, &search_url)? {
            StreamOrHtml::Stream(r) => Ok(r),
            StreamOrHtml::Html(body) => {
                if body.contains("anubis_challenge") || body.contains("Making sure you&#39;re not a bot") {
                    if self.verbose { eprintln!("[lorefetch] Detected Anubis, solving challenge..."); }
                    self.solve_anubis(&agent, &body, &search_url)?;
                    match self.post_stream(&agent, &search_url)? {
                        StreamOrHtml::Stream(r) => Ok(r),
                        StreamOrHtml::Html(_) => Err(LoreError::NotMbox),
                    }
                } else {
                    Err(LoreError::NotMbox)
                }
            }
        }
    }

    /// Solve an Anubis challenge and submit the solution. The caller re-issues
    /// the search afterwards.
    fn solve_anubis(&self, agent: &ureq::Agent, body: &str, search_url: &str) -> Result<(), LoreError> {
        let challenge = detect_anubis(body).ok_or_else(|| {
            LoreError::Anubis("could not extract Anubis challenge data".to_string())
        })?;
        if self.verbose { eprintln!("[lorefetch] Solving challenge: {} (difficulty {})", challenge.challenge, challenge.difficulty); }
        let (hash_hex, nonce) = solve_challenge(&challenge.challenge, challenge.difficulty);
        if self.verbose { eprintln!("[lorefetch] Solution: nonce={}", nonce); }

        let pass_url = format!("{}/.within.website/x/cmd/anubis/api/pass-challenge?response={}&nonce={}&redir={}&elapsedTime=5000",
            self.base_url, urlencoding(&hash_hex), nonce, urlencoding(search_url));
        let pass_resp = agent.get(&pass_url)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("User-Agent", USER_AGENT)
            .header("Referer", search_url)
            .header("Upgrade-Insecure-Requests", "1")
            .call()
            .map_err(|e| LoreError::Anubis(format!("submitting solution: {}", e)))?;
        // With `http_status_as_error(false)` a rejected solution no longer
        // raises an error on its own, so check it explicitly.
        if pass_resp.status().as_u16() >= 400 {
            return Err(LoreError::Anubis(format!(
                "challenge submission returned status {}", pass_resp.status())));
        }

        if self.verbose { eprintln!("[lorefetch] Challenge solved"); }
        Ok(())
    }

    /// Fetch raw mbox content and write to a file.
    /// Used by the `--mbox` CLI flag.  Streams through GzDecoder
    /// and writes the decompressed content to the file.
    pub fn fetch_mbox_to_file(&self, query: &str, list: Option<&str>, path: &Path) -> Result<usize, LoreError> {
        let mut reader = self.fetch_response_reader(query, list)?;
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(map_read_err)?;
        let written = buf.len();
        std::fs::write(path, &buf)?;
        Ok(written)
    }
}



// ── Minimal URL-encoding ─────────────────────────────────────────────

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => { out.push('%'); out.push_str(&format!("{:02X}", b)); }
        }
    }
    out
}

// ── Public API: fetch to maildir ─────────────────────────────────────

/// Fetch mail from lore.kernel.org and write new messages into a maildir.
///
/// Steps:
///   1. Create or validate the maildir structure
///   2. Load or initialise the cache (including per-query `last_date`)
///   3. Compute the incremental query (injecting `dt:` if applicable)
///   4. Fetch the response bytes (solving Anubis if needed)
///   5. Detect content type from first bytes (gzip vs raw mbox vs HTML)
///   6. Stream through GzDecoder → MboxParser → dedup → write
///   7. Track newest Date: header for incremental state
///   8. Save the updated cache atomically
///
/// On a read timeout, all messages already processed are preserved.
/// The `FetchResult.timed_out` flag indicates a partial fetch.
pub fn fetch_to_maildir(
    query: &str,
    list: Option<&str>,
    maildir: &Path,
    verbose: bool,
) -> Result<FetchResult, LoreError> {
    // 1. Maildir structure
    if maildir.exists() { validate_maildir(maildir)?; } else { create_maildir(maildir)?; }

    // 2. Cache
    let mut cache = MaildirCache::load_or_init(maildir)?;

    // 3. Compute incremental query
    let effective_query = match cache.query_last_date(query) {
        Some(ld) => {
            if verbose { eprintln!("[lorefetch] Incremental: last_date={}, injecting dt:", ld); }
            build_incremental_query(query, ld)
        }
        None => {
            if verbose { eprintln!("[lorefetch] Full fetch (no last_date)"); }
            query.to_string()
        }
    };

    // 4. Open a streaming reader for the response (handles Anubis if needed).
    //    The mbox is never fully buffered — memory stays constant regardless
    //    of size, and there is no per-response size cap.
    let client = LoreClient::new().verbose(verbose);
    let reader = match client.fetch_response_reader(&effective_query, list) {
        Ok(r) => r,
        Err(LoreError::NoResults) => {
            if verbose { eprintln!("[lorefetch] No new results — caught up"); }
            return Ok(FetchResult { new_messages: 0, total_messages: 0, timed_out: false });
        }
        Err(e) => return Err(e),
    };

    // 5. Stream through MboxParser → process each message
    let mut parser = MboxParser::new(reader);
    let new_dir = maildir.join("new");
    let mut num_saved = 0usize;
    let mut total_seen = 0usize;
    let mut newest_date: Option<String> = None;
    let mut timed_out = false;

    loop {
        match parser.next_message() {
            Ok(Some((i, msg_bytes))) => {
                total_seen += 1;
                let msg_text = String::from_utf8_lossy(&msg_bytes);
                let parsed = match mail_parser::MessageParser::default().parse(msg_text.as_bytes()) {
                    Some(p) => p,
                    None => {
                        if verbose { eprintln!("[lorefetch] Skipping message {}: no Message-ID", i); }
                        continue;
                    }
                };
                let msg_id = match parsed.message_id() {
                    Some(id) => id.to_string(),
                    None => {
                        if verbose { eprintln!("[lorefetch] Skipping message {}: no Message-ID", i); }
                        continue;
                    }
                };

                // SHA-1 hash of Message-ID
                let mut hasher = Sha1Hasher::new();
                hasher.update(msg_id.as_bytes());
                let filename = format!("{:x}", hasher.finalize());

                // Track newest Date:
                if let Some(date_hdr) = parsed.date() {
                    if let Some(d) = parse_date_string(&date_hdr.to_rfc822()) {
                        match &newest_date {
                            None => newest_date = Some(d),
                            Some(cur) if &d > cur => newest_date = Some(d),
                            _ => {}
                        }
                    }
                }

                if cache.exists(&filename) { continue; }

                let file_path = new_dir.join(&filename);
                std::fs::write(&file_path, msg_text.as_bytes())?;
                #[cfg(unix)] { std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(MAIL_FILE_MODE))?; }
                cache.add(&filename);
                num_saved += 1;
            }
            Ok(None) => break, // EOF
            Err(LoreError::ReadTimeout(_)) => {
                timed_out = true;
                if verbose { eprintln!("[lorefetch] Read timeout — partial fetch saved"); }
                break;
            }
            Err(e) => return Err(e),
        }
    }

    if total_seen == 0 && !timed_out {
        // Empty response — still save cache to persist query entry
    }

    // 8. Update cache and save
    if let Some(ref ld) = newest_date { cache.update_query(query, ld); }
    cache.save(maildir)?;

    Ok(FetchResult { new_messages: num_saved, total_messages: total_seen, timed_out })
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbox_parser_single_message() {
        let mbox = b"From sender@example.com Mon Jan  2 03:14:15 2024\n\
                      Message-ID: <abc@def>\n\
                      Subject: Hello\n\
                      \n\
                      Body here\n";
        let mut parser = MboxParser::new(&mbox[..]);
        let (idx, msg) = parser.next_message().unwrap().unwrap();
        assert_eq!(idx, 0);
        let s = String::from_utf8_lossy(&msg);
        assert!(s.contains("Message-ID: <abc@def>"));
        assert!(s.contains("Body here"));
        assert!(parser.next_message().unwrap().is_none());
    }

    #[test]
    fn mbox_parser_multiple_messages() {
        let mbox = b"From a@b.com Mon Jan  1 00:00:00 2024\n\
                      Message-ID: <first@test>\n\
                      \n\
                      First\n\
                      \n\
                      From b@c.com Mon Jan  2 00:00:00 2024\n\
                      Message-ID: <second@test>\n\
                      \n\
                      Second\n";
        let mut parser = MboxParser::new(&mbox[..]);
        let (i1, m1) = parser.next_message().unwrap().unwrap();
        assert_eq!(i1, 0);
        assert!(String::from_utf8_lossy(&m1).contains("first@test"));
        let (i2, m2) = parser.next_message().unwrap().unwrap();
        assert_eq!(i2, 1);
        assert!(String::from_utf8_lossy(&m2).contains("second@test"));
        assert!(parser.next_message().unwrap().is_none());
    }

    #[test]
    fn mbox_parser_empty() {
        let mut parser = MboxParser::new(&b""[..]);
        assert!(parser.next_message().unwrap().is_none());
    }

    #[test]
    fn unescape_mboxrd_from() {
        let input = b"Subject: test\n>From the depths\n";
        let out = unescape_mboxrd(input);
        assert!(String::from_utf8_lossy(&out).contains("From the depths"));
    }

    #[test]
    fn unescape_mboxrd_nested() {
        let input = b"Subject: test\n>>From here\n";
        let out = unescape_mboxrd(input);
        assert!(String::from_utf8_lossy(&out).contains(">From here"));
    }

    #[test]
    fn solve_trivial_challenge() {
        let (hash, nonce) = solve_challenge("test", 0);
        assert_eq!(nonce, 0);
        assert!(!hash.is_empty());
    }

    #[test]
    fn solve_difficulty_1() {
        let (hash, nonce) = solve_challenge("test", 1);
        assert!(hash.starts_with('0'));
        assert!(format!("{:x}", sha2::Sha256::digest(format!("test{}", nonce).as_bytes())).starts_with('0'));
    }

    #[test]
    fn detect_anubis_present() {
        let html = r#"<html><script id="anubis_challenge" type="application/json">{"challenge":"abc","rules":{"algorithm":"sha256","difficulty":4,"report_as":4}}</script></html>"#;
        assert!(detect_anubis(html).is_some());
        assert_eq!(detect_anubis(html).unwrap().difficulty, 4);
    }

    #[test]
    fn detect_anubis_absent() { assert!(detect_anubis("<html><body>hi</body></html>").is_none()); }

    #[test]
    fn detect_anubis_entity_only() {
        assert!(detect_anubis(r#"<html><p>Making sure you&#39;re not a bot</p></html>"#).is_none());
    }

    #[test]
    fn urlencoding_basic() {
        assert_eq!(urlencoding("hello"), "hello");
        assert_eq!(urlencoding("a b"), "a%20b");
        assert_eq!(urlencoding("a=b"), "a%3Db");
    }

    #[test]
    fn read_prefix_handles_short_and_full() {
        // Fewer bytes than requested (EOF) returns what's available.
        let mut src = std::io::Cursor::new(b"From abc".to_vec());
        let got = read_prefix(&mut src, 4096).unwrap();
        assert_eq!(got, b"From abc");

        // Exactly the requested count.
        let mut src = std::io::Cursor::new(vec![b'x'; 100]);
        let got = read_prefix(&mut src, 10).unwrap();
        assert_eq!(got.len(), 10);
    }

    #[test]
    fn sha1_filename() {
        let mut h = Sha1Hasher::new();
        h.update(b"<abc@def>");
        let f = format!("{:x}", h.finalize());
        assert_eq!(f.len(), 40);
    }

    #[test]
    fn parse_date_rfc2822() {
        assert_eq!(parse_date_string("Fri, 06 Jun 2025 14:32:00 +0000"), Some("2025-06-06".into()));
    }

    #[test]
    fn parse_date_no_day() {
        assert_eq!(parse_date_string("06 Jun 2025 14:32:00 +0000"), Some("2025-06-06".into()));
    }

    #[test]
    fn parse_date_other_month() {
        assert_eq!(parse_date_string("15 Mar 2024 09:00:00 -0500"), Some("2024-03-15".into()));
    }

    #[test]
    fn parse_date_garbage() { assert_eq!(parse_date_string("not a date"), None); }

    #[test]
    fn incremental_no_last_date() {
        assert_eq!(MaildirCache::new().query_last_date("s:lk"), None);
    }

    #[test]
    fn incremental_with_last_date() {
        assert_eq!(build_incremental_query("s:linux-kernel", "2025-06-08"), "s:linux-kernel rt:20250607..");
    }

    #[test]
    fn incremental_existing_rt() {
        assert_eq!(build_incremental_query("s:lk rt:20240101..1.month.ago", "2025-03-15"),
            "s:lk rt:20250314..1.month.ago");
    }

    #[test]
    fn incremental_rt_not_substring() {
        // "rt:" inside "support:" must not be mistaken for an rt: clause.
        assert_eq!(build_incremental_query("s:support:foo", "2025-03-15"),
            "s:support:foo rt:20250314..");
    }

    #[test]
    fn subtract_day() {
        assert_eq!(subtract_one_day(2025, 6, 8), (2025, 6, 7));
        assert_eq!(subtract_one_day(2025, 6, 1), (2025, 5, 31));
        assert_eq!(subtract_one_day(2025, 1, 1), (2024, 12, 31));
        assert_eq!(subtract_one_day(2024, 3, 1), (2024, 2, 29)); // leap year
    }

    #[test]
    fn cache_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path();
        for s in &["cur","new","tmp"] { std::fs::create_dir_all(md.join(s)).unwrap(); }
        let mut c = MaildirCache::new();
        c.add("f1"); c.add("f2"); c.update_query("s:lk", "2025-06-08");
        c.save(md).unwrap();
        let loaded = MaildirCache::load_or_init(md).unwrap();
        assert!(loaded.exists("f1")); assert!(loaded.exists("f2"));
        assert_eq!(loaded.query_last_date("s:lk"), Some("2025-06-08"));
    }

    #[test]
    fn cache_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path();
        for s in &["cur","new","tmp"] { std::fs::create_dir_all(md.join(s)).unwrap(); }
        let bad = MaildirCache { version: 999, cache: HashSet::new(), queries: HashMap::new() };
        let p = md.join(CACHE_FILENAME);
        std::fs::write(&p, serde_json::to_string_pretty(&bad).unwrap()).unwrap();
        std::fs::write(md.join("new/x"), "").unwrap();
        let loaded = MaildirCache::load_or_init(md).unwrap();
        assert!(loaded.exists("x")); assert!(!p.exists()); // deleted & rebuilt
    }

    #[test]
    fn cache_v1_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path();
        for s in &["cur","new","tmp"] { std::fs::create_dir_all(md.join(s)).unwrap(); }
        std::fs::write(md.join(CACHE_FILENAME), r#"{"version":1,"cache":["abc"]}"#).unwrap();
        std::fs::write(md.join("new/abc"), "").unwrap();
        let loaded = MaildirCache::load_or_init(md).unwrap();
        assert!(loaded.exists("abc")); assert!(!md.join(CACHE_FILENAME).exists());
    }

    #[test]
    fn cache_init_from_maildir() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path();
        for s in &["cur","new","tmp"] { std::fs::create_dir_all(md.join(s)).unwrap(); }
        std::fs::write(md.join("new/abc123"), "").unwrap();
        std::fs::write(md.join("cur/ghi789:2,S"), "").unwrap();
        let c = MaildirCache::init_from_maildir(md).unwrap();
        assert!(c.exists("abc123")); assert!(c.exists("ghi789")); assert!(!c.exists("ghi789:2,S"));
    }

    #[test]
    fn cache_per_query() {
        let mut c = MaildirCache::new();
        assert_eq!(c.query_last_date("s:lk"), None);
        c.update_query("s:lk", "2025-06-08");
        assert_eq!(c.query_last_date("s:lk"), Some("2025-06-08"));
        c.update_query("s:raid", "2025-05-01");
        assert_eq!(c.query_last_date("s:raid"), Some("2025-05-01"));
        assert_eq!(c.query_last_date("s:lk"), Some("2025-06-08")); // preserved
    }

    #[test]
    fn validate_maildir_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path();
        for s in &["cur","new","tmp"] { std::fs::create_dir_all(md.join(s)).unwrap(); }
        assert!(validate_maildir(md).is_ok());
    }

    #[test]
    fn validate_maildir_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path();
        std::fs::create_dir_all(md.join("cur")).unwrap();
        assert!(validate_maildir(md).is_err());
    }

    /// Mirror the streaming classification in `post_stream`: peek the raw
    /// prefix, detect gzip by magic, decode-and-peek to confirm mbox, then
    /// stream the rest through `Cursor(decoded_head).chain(gz)` losslessly.
    #[test]
    fn streaming_gzip_mbox_roundtrip() {
        let original = b"From a@b.com\nMessage-ID: <x>\n\nBody\nFrom c@d.com\nMessage-ID: <y>\n\nMore\n";
        let mut compressed = Vec::new();
        let mut gz = flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
        std::io::Write::write_all(&mut gz, original).unwrap();
        gz.finish().unwrap();

        // Stand-in for the HTTP body reader.
        let mut body = std::io::Cursor::new(compressed);
        let head = read_prefix(&mut body, PEEK_SIZE).unwrap();
        assert!(head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b, "gzip magic");

        let chained = std::io::Cursor::new(head).chain(body);
        let mut gzr = flate2::read::GzDecoder::new(chained);
        let dhead = read_prefix(&mut gzr, 4096).unwrap();
        assert!(dhead.starts_with(b"From "), "decoded head is mbox");

        let mut stream = std::io::Cursor::new(dhead).chain(gzr);
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        assert_eq!(&out, original, "no bytes lost across the peek boundary");
    }

    #[test]
    fn streaming_detects_gzipped_html() {
        let original = b"<!DOCTYPE html><html>anubis_challenge</html>";
        let mut compressed = Vec::new();
        let mut gz = flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
        std::io::Write::write_all(&mut gz, original).unwrap();
        gz.finish().unwrap();

        let mut body = std::io::Cursor::new(compressed);
        let head = read_prefix(&mut body, PEEK_SIZE).unwrap();
        let chained = std::io::Cursor::new(head).chain(body);
        let mut gzr = flate2::read::GzDecoder::new(chained);
        let dhead = read_prefix(&mut gzr, 4096).unwrap();
        assert!(!dhead.starts_with(b"From "));
        assert!(dhead.starts_with(b"<"));
    }
}