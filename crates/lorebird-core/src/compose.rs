//! Compose-mail logic: pre-filled replies, RFC 2822 serialisation.
//!
//! `ComposeMail` mirrors the Lua `mail` table from the spec and carries
//! all data needed for the compose window.  `ParentMail` carries the
//! read-only parent information passed to the `on_reply` hook.
//!
//! See `specs/app_config_and_email_compose.md` for the full design.

use std::collections::HashMap;

// ── Data types ────────────────────────────────────────────────────────

/// A mail being composed (reply or new).
///
/// Fields mirror the Lua `mail` table from the spec.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ComposeMail {
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
    /// Message-ID (e.g. `<unique@host>`).  Set by `new_reply()` so
    /// the hook can reference it; `None` means generate at serialise time.
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<String>,
    pub body_text: String,
    /// Arbitrary extra headers (e.g. Reply-To).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Parent message data passed to the `on_reply` hook.
///
/// Mirrors the `parent` table from the spec.  The `headers` map
/// carries all significant headers from the original message so that
/// hooks can inspect `Reply-To`, `List-Post`, `X-Mailer`, etc.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParentMail {
    pub message_id: Option<String>,
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub cc: String,
    pub subject: String,
    pub date: String,
    pub references: String,
    pub in_reply_to: Option<String>,
    pub body_text: String,
    /// All significant headers from the original message (name → value).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

// ── Reply construction ────────────────────────────────────────────────

impl ComposeMail {
    /// Build a pre-filled reply-all from a parent message and profile.
    ///
    /// To = author + original recipients + self; Cc = parent Cc minus self
    /// and minus anyone already in To, deduplicated by email so no recipient
    /// lands in both To and Cc.
    pub fn new_reply(parent: &ParentMail, profile_name: &str, profile_email: &str) -> Self {
        let from = format!("{} <{}>", profile_name, profile_email);
        let to_base = merge_addr_lists(&parent.from, &parent.to);
        let to = add_self_to_addrs(&to_base, profile_name, profile_email);
        let cc = remove_addrs_from_cc(&parent.cc, &to, profile_email);

        // Prepend "Re: " unless already present (case-insensitive).
        let subject = if parent.subject.to_lowercase().starts_with("re:") {
            parent.subject.clone()
        } else {
            format!("Re: {}", parent.subject)
        };

        let in_reply_to = parent.message_id.clone();

        let mut refs_parts = Vec::new();
        if !parent.references.is_empty() {
            refs_parts.push(parent.references.clone());
        }
        if let Some(ref mid) = parent.message_id {
            refs_parts.push(mid.clone());
        }
        let references = if refs_parts.is_empty() {
            None
        } else {
            Some(refs_parts.join(" "))
        };

        let body_text = format_reply_body(&parent.body_text, &parent.date, &parent.from);

        ComposeMail {
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
        out.push_str(&format!("Message-ID: {}\n", self.message_id_header()));
        if let Some(ref irt) = self.in_reply_to {
            out.push_str(&format!("In-Reply-To: {}\n", irt));
        }
        if let Some(ref refs) = self.references {
            out.push_str(&format!("References: {}\n", refs));
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

    /// Reconstruct a `ComposeMail` from a saved raw RFC 2822 message.
    ///
    /// Preserves `message_id` — it is the stable draft identity.
    pub fn from_raw(raw: &[u8]) -> Option<ComposeMail> {
        let m = crate::message::MailMessage::from_bytes(raw)?;
        Some(ComposeMail {
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

/// Add our own address to a comma-separated RFC 2822 address list
/// if not already present.
///
/// Addresses are `"Name <email>"` or bare `email`, separated
/// by `", "`.  We detect ourselves by email substring matching.
fn add_self_to_addrs(addrs: &str, name: &str, email: &str) -> String {
    if addr_list_contains(addrs, email) {
        addrs.to_string()
    } else if addrs.is_empty() {
        format!("{} <{}>", name, email)
    } else {
        format!("{}, {} <{}>", addrs, name, email)
    }
}

/// Concatenate two comma-separated address lists (`a` then `b`), dropping
/// duplicates matched case-insensitively by bare email.
fn merge_addr_lists(a: &str, b: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for entry in a.split(", ").chain(b.split(", ")) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let email = extract_email_from_from(entry).to_lowercase();
        if seen.iter().any(|e| *e == email) {
            continue;
        }
        seen.push(email);
        out.push(entry.to_string());
    }
    out.join(", ")
}

/// Drop our own address and anyone already in `to` (matched by bare email)
/// from `cc`, so no recipient appears in both To and Cc.
fn remove_addrs_from_cc(cc: &str, to: &str, self_email: &str) -> String {
    if cc.is_empty() {
        return String::new();
    }
    let self_l = self_email.to_lowercase();
    let to_emails: Vec<String> = to
        .split(", ")
        .map(|e| extract_email_from_from(e).to_lowercase())
        .collect();
    cc.split(", ")
        .filter(|entry| {
            let email = extract_email_from_from(entry).to_lowercase();
            email != self_l && !to_emails.iter().any(|t| *t == email)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check whether a comma-separated RFC 2822 address list contains the
/// given bare email.
///
/// Matches both `alice@example.com` and `Alice <alice@example.com>`
/// by testing substring containment on each comma-separated entry.
fn addr_list_contains(addrs: &str, email: &str) -> bool {
    addrs
        .split(", ")
        .any(|entry| entry.contains(email))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_parent() -> ParentMail {
        ParentMail {
            message_id: Some("<abc@def>".to_string()),
            from: "Alice <alice@example.com>".to_string(),
            to: "list@example.com".to_string(),
            cc: "bob@example.com, carol@example.com".to_string(),
            subject: "[PATCH v2] Fix memory leak".to_string(),
            date: "2024-01-15T10:30:00+00:00".to_string(),
            references: "<parent@def> <other@def>".to_string(),
            in_reply_to: Some("<parent@def>".to_string()),
            body_text: "This patch fixes...\n".to_string(),
            headers: HashMap::new(),
        }
    }

    #[test]
    fn new_reply_basic() {
        let parent = make_parent();
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");

        assert_eq!(mail.from, "Riccardo <riccardo@defmacro.it>");
        assert_eq!(
            mail.to,
            "Alice <alice@example.com>, list@example.com, Riccardo <riccardo@defmacro.it>"
        );
        assert_eq!(mail.cc, "bob@example.com, carol@example.com");
        assert_eq!(mail.bcc, "");
        assert_eq!(mail.subject, "Re: [PATCH v2] Fix memory leak");
        assert!(mail.date.is_none());
        assert!(mail.message_id.is_some());
        assert!(mail.message_id.as_ref().unwrap().starts_with("<"));
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
    fn new_reply_self_already_in_to() {
        let parent = ParentMail {
            to: "list@example.com, riccardo@defmacro.it".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // Self already in To is not re-added.
        assert_eq!(
            mail.to,
            "Alice <alice@example.com>, list@example.com, riccardo@defmacro.it"
        );
    }

    #[test]
    fn new_reply_removes_self_from_cc() {
        let parent = ParentMail {
            cc: "riccardo@defmacro.it, bob@example.com".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // Cc has self removed
        assert_eq!(mail.cc, "bob@example.com");
    }

    #[test]
    fn new_reply_removes_self_from_cc_angle_form() {
        let parent = ParentMail {
            cc: "Riccardo Maffulli <riccardo@defmacro.it>, bob@example.com".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        // The whole "Riccardo Maffulli <riccardo@defmacro.it>" entry is removed
        assert_eq!(mail.cc, "bob@example.com");
    }

    #[test]
    fn new_reply_empty_to_adds_self() {
        let parent = ParentMail {
            to: String::new(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        assert_eq!(
            mail.to,
            "Alice <alice@example.com>, Riccardo <riccardo@defmacro.it>"
        );
    }

    #[test]
    fn new_reply_empty_cc_stays_empty() {
        let parent = ParentMail {
            cc: String::new(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        assert_eq!(mail.cc, "");
    }

    #[test]
    fn new_reply_includes_author_when_only_in_from() {
        // Regression: author (parent.from) must appear in To even when
        // they are not among the original To recipients.
        let parent = ParentMail {
            from: "Alice <alice@example.com>".to_string(),
            to: "Dave <dave@example.com>".to_string(),
            cc: "list@example.com".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Riccardo", "riccardo@defmacro.it");
        assert_eq!(
            mail.to,
            "Alice <alice@example.com>, Dave <dave@example.com>, Riccardo <riccardo@defmacro.it>"
        );
        assert_eq!(mail.cc, "list@example.com");
    }

    #[test]
    fn new_reply_author_in_cc_is_deduped_to_to() {
        // Author in both From and Cc must end up in To only, never both.
        let parent = ParentMail {
            from: "Alice <alice@example.com>".to_string(),
            to: "list@example.com".to_string(),
            cc: "Alice <alice@example.com>, bob@example.com".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Bob", "bob2@example.com");
        assert_eq!(
            mail.to,
            "Alice <alice@example.com>, list@example.com, Bob <bob2@example.com>"
        );
        // Alice dropped from Cc because she moved to To.
        assert_eq!(mail.cc, "bob@example.com");
    }

    #[test]
    fn no_double_re_prefix() {
        let parent = ParentMail {
            subject: "Re: Already a reply".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Bob", "bob@example.com");
        assert_eq!(mail.subject, "Re: Already a reply");
    }

    #[test]
    fn case_insensitive_re_detection() {
        let parent = ParentMail {
            subject: "RE: Mixed case".to_string(),
            ..make_parent()
        };
        let mail = ComposeMail::new_reply(&parent, "Bob", "bob@example.com");
        assert_eq!(mail.subject, "RE: Mixed case");
    }

    #[test]
    fn new_reply_no_message_id() {
        let mut parent = make_parent();
        parent.message_id = None;
        parent.references = String::new();
        let mail = ComposeMail::new_reply(&parent, "Bob", "bob@example.com");
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
        let mut mail = ComposeMail {
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
    fn to_rfc2822_minimal() {
        let mail = ComposeMail {
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
        let mut mail = ComposeMail {
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
        let mail = ComposeMail::new_reply(&parent, "Bob", "bob@example.com");
        let raw = mail.to_rfc2822();
        let back = ComposeMail::from_raw(raw.as_bytes()).unwrap();
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
        let mail = ComposeMail::new_reply(&parent, "Bob", "bob@example.com");
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