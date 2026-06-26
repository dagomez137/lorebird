//! Bridge between `mail_parser::Message` and our domain types.
//!
//! Extracts the fields we care about (message ID, references, subject, etc.)
//! from a parsed raw message and provides the `thread::Message` trait impl.

use mail_parser::HeaderValue;

use crate::thread;

/// Extracted message fields, ready for threading or indexing.
#[derive(Debug, Clone)]
pub struct MailMessage {
    pub message_id: Option<String>,
    pub references: Vec<String>,
    /// The In-Reply-To header value (single message-id).
    pub in_reply_to: Option<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addr: Option<String>,
    pub cc_addr: Option<String>,
    /// The List-Id header value (e.g. `Linux NVMe <linux-nvme.lists.infradead.org>`),
    /// used to filter a combined maildir down to a single mailing list.
    pub list_id: Option<String>,
    pub date_rfc3339: Option<String>,
    pub date_ts: i64,
    pub received_ts: i64,
    pub body_text: Option<String>,
}

impl MailMessage {
    /// Parse raw message bytes into a `MailMessage`.
    pub fn from_bytes(raw: &[u8]) -> Option<Self> {
        let msg = mail_parser::MessageParser::default().parse(raw)?;

        let message_id = msg.message_id().map(|s| s.to_string());

        let references = msg
            .references()
            .iter_ids()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();

        let in_reply_to = msg
            .in_reply_to()
            .iter_ids()
            .last()
            .map(|s| s.to_string());

        // Deduplicate while preserving order
        let mut seen = std::collections::HashSet::new();
        let references: Vec<String> = references
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();

        let subject = msg.subject().map(|s| s.to_string());

        let from_addr = first_addr(msg.from());
        let to_addr = all_addrs(msg.to());
        let cc_addr = all_addrs(msg.cc());

        // mail_parser parses List-Id as an address header, so its value isn't
        // plain text. Read the raw header instead and keep it verbatim (e.g.
        // `Linux NVMe <linux-nvme.lists.infradead.org>`).
        let list_id = msg
            .header_raw("List-Id")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let date_rfc3339 = msg.date().map(|d| d.to_rfc3339());
        let date_ts = msg.date().map(|d| d.to_timestamp() as i64).unwrap_or(0);

        let received_ts = msg
            .received()
            .and_then(|r| r.date)
            .map(|d| d.to_timestamp() as i64)
            .unwrap_or(0);

        let body_text = msg.body_text(0).map(|s| s.to_string());

        Some(Self {
            message_id,
            references,
            in_reply_to,
            subject,
            from_addr,
            to_addr,
            cc_addr,
            list_id,
            date_rfc3339,
            date_ts,
            received_ts,
            body_text,
        })
    }
}

/// Normalise a raw `List-Id` header to its inner list identifier.
///
/// `List-Id` headers commonly look like `Linux block <linux-block.vger.kernel.org>`
/// or `<linux-block.vger.kernel.org>`. The stable, comparable identity is the
/// part inside the angle brackets. This is the single normalisation point used
/// both when indexing (`indexer`) and when evaluating queries
/// (`query::matches`), so equality/contains comparisons stay consistent.
///
/// Returns the lowercased inner id, or — if there are no angle brackets — the
/// trimmed, lowercased input.
pub fn normalize_list_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.find('<') {
        if let Some(close) = trimmed[open + 1..].find('>') {
            return trimmed[open + 1..open + 1 + close].trim().to_lowercase();
        }
    }
    trimmed.to_lowercase()
}

/// Extract the first email address from a mail_parser `Address` enum.
///
/// Returns the address in display form: `"Name <email>"` when a
/// display name is available, or just `"email"` otherwise.
fn first_addr(addr: Option<&mail_parser::Address>) -> Option<String> {
    use mail_parser::Address;
    addr.and_then(|a| match a {
        Address::List(addrs) => addrs.first().map(|a| format_addr(a)),
        Address::Group(groups) => groups.first().and_then(|g| g.addresses.first().map(|a| format_addr(a))),
    })
}

/// Collect all email addresses from a mail_parser `Address` enum into a
/// comma-separated RFC 2822 formatted string.
///
/// Each address is rendered as `"Name <email>"` (when a display
/// name exists) or just `"email"` (bare).  Addresses are separated by
/// `", "` per RFC 2822.
///
/// This also works for FTS5 tokenisation: commas and angle brackets
/// are word boundaries, so both display names and bare emails are
/// searchable.
fn all_addrs(addr: Option<&mail_parser::Address>) -> Option<String> {
    use mail_parser::Address;
    let addrs: Vec<String> = addr
        .map(|a| match a {
            Address::List(list) => list.iter().map(|a| format_addr(a)).collect(),
            Address::Group(groups) => groups
                .iter()
                .flat_map(|g| g.addresses.iter().map(|a| format_addr(a)))
                .collect(),
        })
        .unwrap_or_default();
    if addrs.is_empty() { None } else { Some(addrs.join(", ")) }
}

/// Format a single `mail_parser::Addr` as `"Name <email>"` or `"email"`.
fn format_addr(a: &mail_parser::Addr) -> String {
    match (&a.name, &a.address) {
        (Some(name), Some(addr)) if !name.is_empty() => {
            format!("{} <{}>", name, addr)
        }
        (_, Some(addr)) => addr.to_string(),
        (Some(name), None) if !name.is_empty() => name.to_string(),
        _ => String::new(),
    }
}

/// Extension trait to extract message IDs from a `HeaderValue`.
trait HeaderValueIdIter {
    fn iter_ids(&self) -> Box<dyn Iterator<Item = &str> + '_>;
}

impl HeaderValueIdIter for HeaderValue<'_> {
    fn iter_ids(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            HeaderValue::Text(s) => Box::new(s.split_whitespace()),
            HeaderValue::TextList(list) => Box::new(list.iter().map(|s| s.as_ref())),
            _ => Box::new(std::iter::empty()),
        }
    }
}

// ── thread::Message impl ─────────────────────────────────────────────

impl thread::Message for MailMessage {
    fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    fn references(&self) -> &[String] {
        &self.references
    }

    fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    fn received_ts(&self) -> i64 {
        self.received_ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_message() {
        let raw = b"From: a@b.com\r\nSubject: Hello\r\nMessage-ID: <abc@def>\r\n\r\nhi";
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert_eq!(msg.subject.as_deref(), Some("Hello"));
        assert_eq!(msg.message_id.as_deref(), Some("abc@def"));
    }

    #[test]
    fn parse_with_references() {
        let raw = concat!(
            "From: x@y.com\r\n",
            "Subject: Re: topic\r\n",
            "Message-ID: <child@x>\r\n",
            "References: <parent@x>\r\n",
            "In-Reply-To: <alt@x>\r\n",
            "\r\nbody",
        );
        let raw = raw.as_bytes();
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert!(msg.references.contains(&"parent@x".to_string()));
    }

    #[test]
    fn implements_thread_message_trait() {
        let raw = b"Message-ID: <a@b>\r\nSubject: Test\r\n\r\n.";
        let msg = MailMessage::from_bytes(raw).unwrap();
        let msgs = vec![msg];
        let threads = crate::thread::thread_messages(msgs);
        assert_eq!(threads.len(), 1);
    }

    #[test]
    fn parse_to_with_display_names() {
        let raw = b"To: Alice <alice@example.com>, Bob <bob@example.com>\r\nMessage-ID: <x@y>\r\n\r\nhi";
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert_eq!(msg.to_addr.as_deref(), Some("Alice <alice@example.com>, Bob <bob@example.com>"));
    }

    #[test]
    fn parse_to_bare_addresses() {
        let raw = b"To: a@b.com, c@d.com\r\nMessage-ID: <x@y>\r\n\r\nhi";
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert_eq!(msg.to_addr.as_deref(), Some("a@b.com, c@d.com"));
    }

    #[test]
    fn parse_list_id() {
        let raw = b"List-Id: Linux NVMe <linux-nvme.lists.infradead.org>\r\nMessage-ID: <x@y>\r\n\r\nhi";
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert_eq!(
            msg.list_id.as_deref(),
            Some("Linux NVMe <linux-nvme.lists.infradead.org>")
        );
    }

    #[test]
    fn parse_no_list_id() {
        let raw = b"From: a@b.com\r\nMessage-ID: <x@y>\r\n\r\nhi";
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert_eq!(msg.list_id, None);
    }

    #[test]
    fn normalize_list_id_with_display_name() {
        assert_eq!(
            normalize_list_id("Linux block <linux-block.vger.kernel.org>"),
            "linux-block.vger.kernel.org"
        );
    }

    #[test]
    fn normalize_list_id_bracketed_only() {
        assert_eq!(
            normalize_list_id("<linux-nvme.lists.infradead.org>"),
            "linux-nvme.lists.infradead.org"
        );
    }

    #[test]
    fn normalize_list_id_bare() {
        assert_eq!(
            normalize_list_id("  Linux-Block.VGER.kernel.org "),
            "linux-block.vger.kernel.org"
        );
    }

    #[test]
    fn parse_from_with_display_name() {
        let raw = b"From: Alice <alice@example.com>\r\nMessage-ID: <x@y>\r\n\r\nhi";
        let msg = MailMessage::from_bytes(raw).unwrap();
        assert_eq!(msg.from_addr.as_deref(), Some("Alice <alice@example.com>"));
    }
}