//! Mail data types, reply construction, and RFC 2822 serialisation.
//!
//! A single `Mail` struct represents both incoming messages (passed as
//! the `parent` argument to hooks) and messages being composed (the
//! `mail` argument).  Fields that may be absent use `Option<String>`;
//! the Lua layer converts `None` to `""` when building tables so that
//! hooks never see `nil` for string fields.
//!
//! See `specs/app_config_and_email_compose.md` for the full design.

use std::collections::HashMap;

// ── Data types ────────────────────────────────────────────────────────

/// A mail message.
///
/// Used for both the read-only `parent` passed to `on_reply` and the
/// mutable `mail` passed to `on_reply` / `on_send`.  In Lua, `None`
/// fields appear as empty strings so hooks never need to nil-check.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Mail {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub cc: String,
    #[serde(default)]
    pub bcc: String,
    pub subject: String,
    /// RFC 2822 Date header.  `None` means "generate at send time".
    #[serde(default)]
    pub date: Option<String>,
    /// Message-ID (e.g. `<unique@host>`).  `None` means generate at
    /// serialise time.
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<String>,
    pub body_text: String,
    /// Arbitrary extra headers (e.g. Reply-To, X-Mailer).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

// ── Reply construction ────────────────────────────────────────────────

impl Mail {
    /// Build a pre-filled reply from a parent message and profile.
    ///
    /// Reply-All (mailing-list style):
    /// - `to`  = parent's `to` addresses + self (added if not already present)
    /// - `cc`  = parent's `cc` addresses minus self
    /// - `subject` = "Re: " prefix (no double Re:)
    /// - `in_reply_to` = parent's `message_id`
    /// - `references` = parent's references + " " + parent's `message_id`
    /// - `from` = "Profile Name <profile@email>"
    /// - `date` = `None` (auto-generated at send time)
    /// - `message_id` = freshly generated unique ID
    /// - `body_text` = quoted parent body with attribution line
    pub fn new_reply(parent: &Mail, profile_name: &str, profile_email: &str) -> Self {
        let from = format!("{} <{}>", profile_name, profile_email);

        // To = parent's From (the person we're replying to)
        let to = parent.from.clone();

        // Cc = parent's To + parent's Cc, minus ourselves and the sender
        // (who is already in To), deduplicated.
        let cc = merge_and_dedup(&parent.to, &parent.cc, profile_email, &parent.from);

        // Subject: prepend "Re: " unless it already starts with it (case-insensitive).
        let subject = if parent.subject.to_lowercase().starts_with("re:") {
            parent.subject.clone()
        } else {
            format!("Re: {}", parent.subject)
        };

        let in_reply_to = parent.message_id.clone();

        // References chain: parent's references + parent's message_id.
        let mut refs_parts = Vec::new();
        if let Some(ref refs) = parent.references {
            if !refs.is_empty() {
                refs_parts.push(refs.clone());
            }
        }
        if let Some(ref mid) = parent.message_id {
            refs_parts.push(mid.clone());
        }
        let references = if refs_parts.is_empty() {
            None
        } else {
            Some(refs_parts.join(" "))
        };

        let parent_date = parent.date.as_deref().unwrap_or("");
        let body_text = format_reply_body(&parent.body_text, parent_date, &parent.from);

        Mail {
            from,
            to,
            cc,
            bcc: String::new(),
            subject,
            date: None, // generated at send time by to_rfc2822()
            message_id: Some(generate_message_id(profile_email)),
            in_reply_to,
            references,
            body_text,
            headers: HashMap::new(),
        }
    }

    /// Serialize this mail to RFC 2822 format.
    ///
    /// Produces a string suitable for piping to sendmail `-t` or
    /// saving as a draft.  Headers are written in a deterministic
    /// order.  Missing `Date` and `Message-ID` are auto-generated.
    /// `MIME-Version`, `Content-Type`, and `X-Mailer` are emitted
    /// unless the user already set them in `self.headers`.
    pub fn to_rfc2822(&self) -> String {
        let mut out = String::new();

        // ── Mandatory / conventional headers in standard order ──
        out.push_str(&format!("Date: {}\n", self.date_header()));
        out.push_str(&format!("From: {}\n", self.from));
        out.push_str(&format!("To: {}\n", self.to));
        if !self.cc.is_empty() {
            out.push_str(&format!("Cc: {}\n", self.cc));
        }
        if !self.bcc.is_empty() {
            out.push_str(&format!("Bcc: {}\n", self.bcc));
        }
        out.push_str(&format!("Subject: {}\n", self.subject));
        out.push_str(&format!("Message-ID: {}\n", wrap_msgid(&self.message_id_header())));
        if let Some(ref irt) = self.in_reply_to {
            out.push_str(&format!("In-Reply-To: {}\n", wrap_msgid_list(irt)));
        }
        if let Some(ref refs) = self.references {
            out.push_str(&format!("References: {}\n", wrap_msgid_list(refs)));
        }

        // ── MIME / X-Mailer (emit if user hasn't overridden) ──
        let user_keys: std::collections::HashSet<&str> =
            self.headers.keys().map(|s| s.as_str()).collect();

        if !user_keys.contains("MIME-Version") {
            out.push_str("MIME-Version: 1.0\n");
        }
        if !user_keys.contains("Content-Type") {
            out.push_str("Content-Type: text/plain; charset=utf-8\n");
        }
        if !user_keys.contains("X-Mailer") {
            out.push_str("X-Mailer: lorebird\n");
        }

        // ── Arbitrary user headers in sorted order ──
        let mut headers: Vec<_> = self.headers.iter().collect();
        headers.sort_by_key(|(k, _)| *k);
        for (key, value) in headers {
            out.push_str(&format!("{}: {}\n", key, value));
        }

        // ── Blank line + body ──
        out.push('\n');
        out.push_str(&self.body_text);
        out.push('\n');

        out
    }

    /// Reconstruct a `Mail` from a saved raw RFC 2822 message.
    ///
    /// Preserves `message_id` — it is the stable draft identity.
    pub fn from_raw(raw: &[u8]) -> Option<Mail> {
        let m = crate::message::MailMessage::from_bytes(raw)?;
        Some(Mail {
            from: m.from_addr.unwrap_or_default(),
            to: m.to_addr.unwrap_or_default(),
            cc: m.cc_addr.unwrap_or_default(),
            bcc: String::new(),
            subject: m.subject.unwrap_or_default(),
            date: None,
            message_id: m.message_id,
            in_reply_to: m.in_reply_to,
            references: if m.references.is_empty() {
                None
            } else {
                Some(m.references.join(" "))
            },
            body_text: m.body_text.unwrap_or_default(),
            headers: HashMap::new(),
        })
    }

    /// Stable draft identity passed to `save_draft`/`delete_draft`.
    pub fn draft_id(&self) -> String {
        match &self.message_id {
            Some(id) => id.clone(),
            None => generate_message_id(&extract_email_from_from(&self.from)),
        }
    }

    /// Return the Date header value, generating it from the current
    /// time if `self.date` is `None`.
    fn date_header(&self) -> String {
        match &self.date {
            Some(d) => d.clone(),
            None => {
                let now = chrono::Local::now();
                now.to_rfc2822()
            }
        }
    }

    /// Return the Message-ID header value, generating one if
    /// `self.message_id` is `None`.  Uses the sender's email from
    /// `self.from` to derive the Message-ID domain.
    fn message_id_header(&self) -> String {
        match &self.message_id {
            Some(id) => id.clone(),
            None => {
                let email = extract_email_from_from(&self.from);
                generate_message_id(&email)
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Wrap a single message id in angle brackets if it lacks them.
///
/// RFC 5322 message ids are `<id@host>`, and public-inbox / lore strict
/// threading only matches bracketed ids. `mail_parser` yields ids without the
/// brackets, so a reply built from a parsed parent would emit bracket-less
/// `In-Reply-To` / `References` and thread loosely (by subject) instead. This
/// restores the brackets on the way out and is idempotent for ids that already
/// have them.
fn wrap_msgid(id: &str) -> String {
    let t = id.trim();
    if t.starts_with('<') && t.ends_with('>') && t.len() >= 2 {
        t.to_string()
    } else {
        format!("<{}>", t.trim_matches(|c| c == '<' || c == '>'))
    }
}

/// Wrap every whitespace-separated id in a `References` / `In-Reply-To` value.
fn wrap_msgid_list(ids: &str) -> String {
    ids.split_whitespace()
        .map(wrap_msgid)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Generate a unique Message-ID in the style of `git send-email`:
///
/// `<YYYYMMDDHHMMSS.COUNTER-USERNAME@DOMAIN>`
///
/// - Timestamp in `%Y%m%d%H%M%S` for human-readability and sortability.
/// - Per-process counter for uniqueness within a session.
/// - Sender's email split: local-part → `USERNAME`, domain → Message-ID domain.
///   Falls back to `unknown@localhost` if no email is provided.
fn generate_message_id(sender_email: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d%H%M%S");
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    let (user, domain) = split_email(sender_email);
    format!("<{}.{seq}-{user}@{domain}>", ts)
}

/// Split an email address into `(local_part, domain)`.
///
/// `"alice@example.com"` → `("alice", "example.com")`.
/// Falls back to `("unknown", "localhost")` if the address has no `@`.
fn split_email(email: &str) -> (String, String) {
    match email.rfind('@') {
        Some(i) => (email[..i].to_string(), email[i + 1..].to_string()),
        None => ("unknown".to_string(), "localhost".to_string()),
    }
}

/// Extract the bare email address from a `From` header value.
///
/// Handles both `"Name <email>"` and bare `"email"` forms.
fn extract_email_from_from(from: &str) -> String {
    // Try to extract from angle brackets
    if let Some(start) = from.rfind('<') {
        if let Some(end) = from[start..].find('>') {
            return from[start + 1..start + end].to_string();
        }
    }
    // Fall back: treat the whole string as a bare address
    from.trim().to_string()
}

/// Format the reply body with attribution and quoted text.
///
/// Produces:
/// ```text
///
/// On <date>, <from> wrote:
/// > line 1
/// > line 2
/// ```
fn format_reply_body(body: &str, date: &str, from: &str) -> String {
    let mut out = String::new();
    out.push_str("\n\n");
    out.push_str(&format!("On {}, {} wrote:\n", date, from));
    for line in body.lines() {
        out.push('>');
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Merge two comma-separated address lists, remove our own address
/// and the sender (who is already in To), and deduplicate by bare email.
///
/// Used to compute the Cc of a reply:
///   Cc = (parent.to + parent.cc) − self − parent.from
fn merge_and_dedup(to: &str, cc: &str, self_email: &str, sender: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    // Seed the dedup set with the sender's email so they don't appear
    // in Cc (they're already in To).
    seen.insert(extract_email_from_from(sender).to_lowercase());

    // Collect entries from both lists in order
    let entries: Vec<&str> = to.split(", ").chain(cc.split(", ")).collect();

    for entry in entries {
        if entry.is_empty() {
            continue;
        }
        // Skip ourselves
        if entry.contains(self_email) {
            continue;
        }
        // Deduplicate by bare email
        let email = extract_email_from_from(entry).to_lowercase();
        if seen.insert(email) {
            result.push(entry.to_string());
        }
    }

    result.join(", ")
}

/// Remove our own address from a comma-separated RFC 2822 address list.
///
// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_parent() -> Mail {
        Mail {
            from: "Alice <alice@example.com>".to_string(),
            to: "list@example.com".to_string(),
            cc: "bob@example.com, carol@example.com".to_string(),
            bcc: String::new(),
            subject: "[PATCH v2] Fix memory leak".to_string(),
            date: Some("2024-01-15T10:30:00+00:00".to_string()),
            message_id: Some("<abc@def>".to_string()),
            in_reply_to: Some("<parent@def>".to_string()),
            references: Some("<parent@def> <other@def>".to_string()),
            body_text: "This patch fixes...\n".to_string(),
            headers: HashMap::new(),
        }
    }

    #[test]
    fn new_reply_basic() {
        let parent = make_parent();
        let mail = Mail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");

        assert_eq!(mail.from, "Riccardo <riccardo@defmacro.it>");
        // To = parent.from (the person we're replying to)
        assert_eq!(mail.to, "Alice <alice@example.com>");
        // Cc = parent.to + parent.cc, minus self, deduped
        assert_eq!(mail.cc, "list@example.com, bob@example.com, carol@example.com");
        assert_eq!(mail.bcc, "");
        assert_eq!(mail.subject, "Re: [PATCH v2] Fix memory leak");
        assert!(mail.date.is_none()); // generated at send time
        assert!(mail.message_id.is_some()); // freshly generated
        assert!(mail.message_id.as_ref().unwrap().starts_with("<"));
        // Message-ID uses sender domain: riccardo@defmacro.it → @defmacro.it
        assert!(mail.message_id.as_ref().unwrap().ends_with("@defmacro.it>"));
        assert_eq!(mail.in_reply_to, Some("<abc@def>".to_string()));
        assert_eq!(
            mail.references,
            Some("<parent@def> <other@def> <abc@def>".to_string())
        );
        assert!(mail.body_text.contains("On 2024-01-15T10:30:00+00:00, Alice <alice@example.com> wrote:"));
        assert!(mail.body_text.contains("> This patch fixes..."));
    }

    #[test]
    fn new_reply_sender_in_to_not_cc() {
        // If the original sender is also in parent.to, they become To
        // and are deduplicated from Cc.
        let parent = Mail {
            to: "list@example.com, alice@example.com".to_string(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // To = parent.from (Alice)
        assert_eq!(mail.to, "Alice <alice@example.com>");
        // Cc = parent.to (minus Alice who's already in To) + parent.cc
        assert_eq!(mail.cc, "list@example.com, bob@example.com, carol@example.com");
    }

    #[test]
    fn new_reply_removes_self_from_cc() {
        let parent = Mail {
            cc: "riccardo@defmacro.it, bob@example.com".to_string(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // Self removed from Cc, parent.to (list) merged in
        assert_eq!(mail.cc, "list@example.com, bob@example.com");
    }

    #[test]
    fn new_reply_removes_self_from_cc_angle_form() {
        let parent = Mail {
            cc: "Riccardo Maffulli <riccardo@defmacro.it>, bob@example.com".to_string(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // The whole "Riccardo Maffulli <riccardo@defmacro.it>" entry is removed
        assert_eq!(mail.cc, "list@example.com, bob@example.com");
    }

    #[test]
    fn new_reply_empty_to_sender_becomes_to() {
        // Even with empty To, sender goes to To and nothing in Cc
        let parent = Mail {
            to: String::new(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        assert_eq!(mail.to, "Alice <alice@example.com>");
        // Cc = parent.cc (no parent.to to merge in)
        assert_eq!(mail.cc, "bob@example.com, carol@example.com");
    }

    #[test]
    fn new_reply_empty_cc_stays_merged() {
        let parent = Mail {
            cc: String::new(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // Cc = parent.to (merged in even though cc is empty)
        assert_eq!(mail.cc, "list@example.com");
    }

    #[test]
    fn no_double_re_prefix() {
        let parent = Mail {
            subject: "Re: Already a reply".to_string(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Bob", "bob@example.com");
        assert_eq!(mail.subject, "Re: Already a reply");
    }

    #[test]
    fn case_insensitive_re_detection() {
        let parent = Mail {
            subject: "RE: Mixed case".to_string(),
            ..make_parent()
        };
        let mail = Mail::new_reply(&parent, "Bob", "bob@example.com");
        assert_eq!(mail.subject, "RE: Mixed case");
    }

    #[test]
    fn new_reply_no_message_id() {
        let mut parent = make_parent();
        parent.message_id = None;
        parent.references = None;
        let mail = Mail::new_reply(&parent, "Bob", "bob@example.com");
        assert_eq!(mail.in_reply_to, None);
        assert_eq!(mail.references, None);
    }

    #[test]
    fn format_reply_body_wraps_lines() {
        let body = "Line one\nLine two\n";
        let result = format_reply_body(body, "2024-01-15", "Alice");
        assert!(result.contains("On 2024-01-15, Alice wrote:"));
        assert!(result.contains("> Line one"));
        assert!(result.contains("> Line two"));
    }

    #[test]
    fn format_reply_body_empty_line() {
        let body = "Hello\n\nWorld\n";
        let result = format_reply_body(body, "2024-01-15", "Bob");
        assert!(result.contains("> Hello"));
        assert!(result.contains(">\n")); // empty line just has >
        assert!(result.contains("> World"));
    }

    #[test]
    fn to_rfc2822_basic() {
        let mut mail = Mail {
            from: "Bob <bob@example.com>".to_string(),
            to: "Alice <alice@example.com>".to_string(),
            cc: "list@example.com".to_string(),
            bcc: String::new(),
            subject: "Re: Test".to_string(),
            date: Some("Thu, 29 May 2025 10:00:00 +0000".to_string()),
            message_id: Some("<test@localhost>".to_string()),
            in_reply_to: Some("<parent@example.com>".to_string()),
            references: Some("<grandparent@example.com> <parent@example.com>".to_string()),
            body_text: "Hello\n".to_string(),
            headers: HashMap::new(),
        };
        mail.headers.insert("X-Custom".to_string(), "value".to_string());

        let rfc = mail.to_rfc2822();
        assert!(rfc.contains("Date: Thu, 29 May 2025 10:00:00 +0000\n"));
        assert!(rfc.contains("From: Bob <bob@example.com>\n"));
        assert!(rfc.contains("To: Alice <alice@example.com>\n"));
        assert!(rfc.contains("Cc: list@example.com\n"));
        assert!(!rfc.contains("Bcc:")); // empty Bcc omitted
        assert!(rfc.contains("Subject: Re: Test\n"));
        assert!(rfc.contains("Message-ID: <test@localhost>\n"));
        assert!(rfc.contains("In-Reply-To: <parent@example.com>\n"));
        assert!(rfc.contains("References: <grandparent@example.com> <parent@example.com>\n"));
        // Auto-generated MIME / X-Mailer headers
        assert!(rfc.contains("MIME-Version: 1.0\n"));
        assert!(rfc.contains("Content-Type: text/plain; charset=utf-8\n"));
        assert!(rfc.contains("X-Mailer: lorebird\n"));
        // User custom header
        assert!(rfc.contains("X-Custom: value\n"));
        // Blank line + body
        assert!(rfc.contains("\nHello\n"));
    }

    #[test]
    fn to_rfc2822_brackets_bare_reply_ids() {
        // A reply built from a mail_parser-parsed parent carries bracket-less
        // ids; they must be bracketed on the way out so lore strict-threads.
        let mail = Mail {
            from: "Bob <bob@example.com>".to_string(),
            to: "Alice <alice@example.com>".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Re: Test".to_string(),
            date: Some("Thu, 29 May 2025 10:00:00 +0000".to_string()),
            message_id: Some("gen.0-bob@example.com".to_string()),
            in_reply_to: Some("parent@example.com".to_string()),
            references: Some("grandparent@example.com parent@example.com".to_string()),
            body_text: "Hi\n".to_string(),
            headers: HashMap::new(),
        };
        let rfc = mail.to_rfc2822();
        assert!(rfc.contains("Message-ID: <gen.0-bob@example.com>\n"), "{rfc}");
        assert!(rfc.contains("In-Reply-To: <parent@example.com>\n"), "{rfc}");
        assert!(
            rfc.contains("References: <grandparent@example.com> <parent@example.com>\n"),
            "{rfc}"
        );
    }

    #[test]
    fn to_rfc2822_minimal() {
        let mail = Mail {
            from: "A <a@b>".to_string(),
            to: "C <c@d>".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hello".to_string(),
            date: None,       // auto-generated
            message_id: None, // auto-generated
            in_reply_to: None,
            references: None,
            body_text: "Body\n".to_string(),
            headers: HashMap::new(),
        };
        let rfc = mail.to_rfc2822();
        assert!(rfc.contains("Date: ")); // auto-generated
        assert!(rfc.contains("Message-ID: <")); // auto-generated
        // Auto-generated uses sender domain from From: header
        assert!(rfc.contains("@b>")); // from "A <a@b>"
        assert!(rfc.contains("MIME-Version: 1.0"));
        assert!(rfc.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(rfc.contains("X-Mailer: lorebird"));
        assert!(!rfc.contains("In-Reply-To:"));
        assert!(!rfc.contains("References:"));
        assert!(!rfc.contains("Cc:"));
    }

    #[test]
    fn to_rfc2822_user_overrides_mime_headers() {
        let mut mail = Mail {
            from: "A <a@b>".to_string(),
            to: "C <c@d>".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hello".to_string(),
            date: Some("Thu, 01 Jan 2025 00:00:00 +0000".to_string()),
            message_id: Some("<override@me>".to_string()),
            in_reply_to: None,
            references: None,
            body_text: "Body\n".to_string(),
            headers: HashMap::new(),
        };
        // User sets Content-Type and X-Mailer in headers map
        mail.headers.insert("Content-Type".to_string(), "text/html; charset=utf-8".to_string());
        mail.headers.insert("X-Mailer".to_string(), "my-client/1.0".to_string());

        let rfc = mail.to_rfc2822();
        // Default Content-Type should NOT appear (user overrides)
        assert!(!rfc.contains("text/plain; charset=utf-8"));
        // User's values appear in the arbitrary-headers section
        assert!(rfc.contains("Content-Type: text/html; charset=utf-8"));
        assert!(rfc.contains("X-Mailer: my-client/1.0"));
    }

    #[test]
    fn generate_message_id_format() {
        let mid = generate_message_id("alice@example.com");
        assert!(mid.starts_with("<"));
        assert!(mid.ends_with("@example.com>"));
        // Local-part: YYYYMMDDHHMMSS.COUNTER-alice
        let inner = &mid[1..mid.len() - 1];
        let parts: Vec<&str> = inner.split('@').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("-alice")); // username from email
        assert_eq!(parts[1], "example.com"); // domain from email
        // Timestamp is 14 digits at the start
        let dot_pos = parts[0].find('.').unwrap();
        let ts = &parts[0][..dot_pos];
        assert_eq!(ts.len(), 14);
        assert!(ts.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn from_raw_round_trip() {
        let parent = make_parent();
        let mail = Mail::new_reply(&parent, "Bob", "bob@example.com");
        let raw = mail.to_rfc2822();
        let back = Mail::from_raw(raw.as_bytes()).unwrap();
        assert_eq!(back.subject, mail.subject);
        assert_eq!(back.to, mail.to);
        // The parser strips <> from Message-ID; the draft identity is bracket-insensitive.
        assert!(back.message_id.is_some());
        let stripped = mail.message_id.as_ref().unwrap().trim_matches(|c| c == '<' || c == '>');
        assert_eq!(back.message_id.as_deref(), Some(stripped));
    }

    #[test]
    fn draft_id_uses_message_id() {
        let parent = make_parent();
        let mail = Mail::new_reply(&parent, "Bob", "bob@example.com");
        assert_eq!(Some(mail.draft_id()), mail.message_id);
    }

    #[test]
    fn generate_message_id_no_at() {
        // Fallback when email has no @
        let mid = generate_message_id("no-at-sign");
        assert!(mid.ends_with("@localhost>"));
        assert!(mid.contains("unknown"));
    }
}