//! Normalise a mail subject down to a stable "series key".
//!
//! A *series* is a set of related threads that share a title but differ by
//! version: periodic editions (`[GIT PULL] nvme updates for Linux 7.2`,
//! `… 7.1`, `… 7.0`) and patch-set revisions (`[PATCH v3 0/9] nvme: foo`,
//! `[PATCH v4 0/9] nvme: foo`). Collapsing the volatile bits yields a key
//! that an FTS5 `subject:"…"` phrase query will match across every edition,
//! past and future.
//!
//! Examples:
//! - `[GIT PULL] nvme updates for Linux 7.2`     → `nvme updates for Linux`
//! - `Re: [GIT PULL] nvme updates for Linux 7.1` → `nvme updates for Linux`
//! - `[PATCH v3 0/9] nvme: fabrics keep-alive`   → `nvme: fabrics keep-alive`

/// Reduce `subject` to its series key. Returns an empty string only if the
/// subject is empty or consists entirely of strippable tokens.
pub fn series_key(subject: &str) -> String {
    let mut s = subject.trim();

    // Strip leading reply/forward prefixes (Re:, Fwd:, Aw:, …), repeatedly.
    loop {
        match strip_reply_prefix(s) {
            Some(rest) => s = rest.trim_start(),
            None => break,
        }
    }

    // Strip leading bracket tags ([GIT PULL], [PATCH v3 0/9], [RFC], …).
    loop {
        match strip_leading_bracket(s) {
            Some(rest) => s = rest.trim_start(),
            None => break,
        }
    }

    // Strip trailing version tokens (7.2, v3, 7.2-rc1), repeatedly.
    let mut owned = s.to_string();
    loop {
        match strip_trailing_version(&owned) {
            Some(rest) => owned = rest.trim_end().to_string(),
            None => break,
        }
    }

    // Tidy: drop dangling separators, collapse internal whitespace.
    let trimmed = owned
        .trim()
        .trim_end_matches([':', '-', '–', '—', ' ', '\t'])
        .trim();
    collapse_whitespace(trimmed)
}

/// If `s` begins with a reply/forward prefix like `Re:`, `RE[2]:`, `Fwd:`,
/// return the remainder after the colon.
fn strip_reply_prefix(s: &str) -> Option<&str> {
    let colon = s.find(':')?;
    let mut tok = s[..colon].trim();
    // Tolerate a numbered prefix such as `Re[2]`.
    if let Some(b) = tok.find('[') {
        tok = tok[..b].trim();
    }
    let known = ["re", "fwd", "fw", "aw", "sv", "vs", "antw"];
    if known.contains(&tok.to_ascii_lowercase().as_str()) {
        Some(&s[colon + 1..])
    } else {
        None
    }
}

/// If `s` begins with a `[...]` tag, return the remainder after the `]`.
fn strip_leading_bracket(s: &str) -> Option<&str> {
    if !s.starts_with('[') {
        return None;
    }
    let close = s.find(']')?;
    Some(&s[close + 1..])
}

/// If `s` ends with a version-like token, return `s` without that token.
fn strip_trailing_version(s: &str) -> Option<&str> {
    let trimmed = s.trim_end();
    let last_space = trimmed.rfind(char::is_whitespace);
    let (head, tok) = match last_space {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => return None, // a lone token is the whole title; keep it
    };
    if is_version_token(tok) {
        Some(head)
    } else {
        None
    }
}

/// Is `tok` a version token we should strip from a series title?
///
/// Accepts `v3`, `7.2`, `6.10`, `7.2-rc1`. Deliberately rejects a bare
/// integer like `7` (too ambiguous — could be part of the real title); a
/// version must carry a `v` prefix, a dotted number, or an `-rcN` suffix.
fn is_version_token(tok: &str) -> bool {
    let mut t = tok;
    let had_v = t.starts_with('v') || t.starts_with('V');
    if had_v {
        t = &t[1..];
    }

    let mut had_rc = false;
    let lower = t.to_ascii_lowercase();
    if let Some(i) = lower.find("-rc") {
        let rest = &t[i + 3..];
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            had_rc = true;
            t = &t[..i];
        }
    }

    if t.is_empty() {
        return false;
    }
    let only_digits_dots = t.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let has_digit = t.bytes().any(|b| b.is_ascii_digit());
    if !only_digits_dots || !has_digit {
        return false;
    }
    had_v || had_rc || t.contains('.')
}

/// Collapse runs of whitespace to single spaces.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_edition_drops_trailing_version() {
        assert_eq!(
            series_key("[GIT PULL] nvme updates for Linux 7.2"),
            "nvme updates for Linux"
        );
        assert_eq!(
            series_key("Re: [GIT PULL] nvme updates for Linux 7.1"),
            "nvme updates for Linux"
        );
    }

    #[test]
    fn patch_revisions_collapse() {
        let a = series_key("[PATCH v3 0/9] nvme: fabrics keep-alive");
        let b = series_key("[PATCH v4 0/9] nvme: fabrics keep-alive");
        assert_eq!(a, "nvme: fabrics keep-alive");
        assert_eq!(a, b);
    }

    #[test]
    fn handles_rfc_and_resend_and_re() {
        assert_eq!(series_key("[RFC PATCH] block: foo"), "block: foo");
        assert_eq!(series_key("[PATCH RESEND] block: foo"), "block: foo");
        assert_eq!(series_key("Re: [PATCH 1/2] block: foo"), "block: foo");
    }

    #[test]
    fn strips_rc_and_dotted_versions() {
        assert_eq!(series_key("[GIT PULL] xfs: updates 6.10-rc1"), "xfs: updates");
        assert_eq!(series_key("mm: bar v2"), "mm: bar");
    }

    #[test]
    fn keeps_bare_integers_in_title() {
        // A lone integer is ambiguous, so we keep it.
        assert_eq!(series_key("[PATCH] introduce io_uring zone 7"), "introduce io_uring zone 7");
    }

    #[test]
    fn empty_and_tag_only() {
        assert_eq!(series_key(""), "");
        assert_eq!(series_key("[GIT PULL]"), "");
    }

    #[test]
    fn no_tags_passthrough() {
        assert_eq!(series_key("just a normal subject"), "just a normal subject");
    }
}
