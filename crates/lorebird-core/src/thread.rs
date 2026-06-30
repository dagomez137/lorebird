//! JWZ email threading algorithm.
//!
//! Based on <https://www.jwz.org/doc/threading.html>.
//!
//! The algorithm groups messages into trees based on:
//! - `Message-ID` (unique per message)
//! - `References` + `In-Reply-To` headers (parent links)
//! - `Subject` fallback (bare subject, stripped of "Re:", "Fwd:", etc.)

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Minimal data the threading algorithm needs from each message.
///
/// Implement this trait for whatever message type your application uses
/// (e.g. a parsed mail, a database row, etc.).
pub trait Message {
    /// The `Message-ID` header value, without angle brackets.
    /// Returns `None` if the message has no ID (malformed).
    fn message_id(&self) -> Option<&str>;

    /// All references to parent messages, in order.
    ///
    /// Typically this is the `References` header followed by the
    /// `In-Reply-To` header (if not already present in `References`).
    /// Each entry should be a message ID without angle brackets.
    fn references(&self) -> &[String];

    /// The `Subject` header value. Used as a fallback when no reference
    /// links exist: messages sharing the same "bare" subject (after
    /// stripping "Re:", "Fwd:", etc.) are grouped together.
    fn subject(&self) -> Option<&str>;

    /// Unix timestamp from the `Received:` header, for post-threading
    /// sibling sort order (oldest first).  Messages without a
    /// `Received:` header should return 0.
    fn received_ts(&self) -> i64;
}

#[derive(Debug)]
struct Container<T> {
    /// None for ghost/placeholder containers
    message: Option<T>,

    parent: Option<usize>,
    children: Vec<usize>,
}

impl<T> Container<T> {
    fn new(message: Option<T>) -> Self {
        Self {
            message,
            parent: None,
            children: vec![],
        }
    }
}

// ── Thread tree ──────────────────────────────────────────────────────

/// A node in a thread tree.
///
/// Most nodes carry a message. Ghost nodes (created when multiple
/// messages reference a missing parent, or when messages are grouped
/// by subject) have `message: None`.
#[derive(Debug)]
pub struct Thread<T: Message> {
    /// The message at this node, or `None` for ghost/placeholder nodes.
    pub message: Option<T>,
    /// Child messages (replies).
    pub children: Vec<Thread<T>>,
}

// ── Algorithm ────────────────────────────────────────────────────────

fn is_ancestor<T>(ancestor_idx: usize, descendant_idx: usize, lst: &[Container<T>]) -> bool {
    let mut cur = lst[descendant_idx].parent;
    let mut steps = 0usize;
    while let Some(p) = cur {
        if p == ancestor_idx {
            return true;
        }
        // Cycle guard: a valid ancestor chain can't be longer than the number
        // of containers. Malformed real-world References can form parent
        // cycles; without this, the walk would loop forever. Treat a cycle as
        // "is an ancestor" so the caller conservatively refuses to add a link.
        steps += 1;
        if steps > lst.len() {
            return true;
        }
        cur = lst[p].parent;
    }
    false
}

fn build_thread_subtree<T: Message>(
    root_idx: usize,
    cs: &mut Vec<Container<T>>,
    visited: &mut HashSet<usize>,
) -> Vec<Thread<T>> {
    // Break any cycle that slipped through: never descend into a container
    // twice. Without this, a parent/child cycle would recurse forever.
    if !visited.insert(root_idx) {
        return vec![];
    }
    // Take the child list out rather than cloning it: the old `cs` hierarchy
    // is discarded right after this pass, so moving the indices is safe and
    // avoids allocating a fresh Vec per container.
    let child_indices = std::mem::take(&mut cs[root_idx].children);
    let children: Vec<Thread<T>> = child_indices
        .into_iter()
        .flat_map(|child_idx| build_thread_subtree(child_idx, cs, visited))
        .collect();

    let msg = cs[root_idx].message.take();
    let is_root_ghost = cs[root_idx].parent.is_none();

    // * container w/o children or msg -> REMOVE
    // * container w/o msg, WITH children ->
    //    * remove, promote children
    //    * UNLESS promoting children to root level
    //        * IF there's just one child to promote this way, do it anyway
    match (msg, children.len(), is_root_ghost) {
        (Some(msg), _, _) => vec![Thread {
            message: Some(msg),
            children,
        }],
        (None, 0, _) => vec![],       // ghost node, remove
        (None, 1, _) => children,     // empty container, single child, always promote
        (None, _, false) => children, // promote
        (None, _, true) => vec![Thread {
            // top-level ghost, multiple children, retain
            message: None,
            children,
        }],
    }
}

fn _bare_subject(raw: &str) -> (String, usize) {
    let mut s = raw.trim().to_string();
    let mut depth = 0;
    loop {
        let trimmed = s.trim_start();
        let lower = trimmed.to_lowercase();
        let rest = lower.strip_prefix("re:").or_else(|| {
            let x = lower.strip_prefix("re")?.trim_start().strip_prefix('[')?;
            let close = x.find(']')?;
            x[close..].strip_prefix("]:")
        });
        match rest {
            Some(r) => {
                depth += 1;
                s = trimmed[trimmed.len() - r.len()..].trim().to_string();
            }
            None => break,
        }
    }
    (s, depth)
}

/// Strip leading "re:", "re[4]:" (case-insensitive)
/// May return empty string if nothing else remains in the subject line
pub fn bare_subject(raw: &str) -> String {
    let (s, _) = _bare_subject(raw);
    return s;
}

/// Find first occurrence of a message in subtree and extract its subject
fn subtree_subject<T: Message>(node: &Thread<T>) -> Option<String> {
    if let Some(ref msg) = node.message {
        msg.subject().map(|s| s.to_string())
    } else {
        node.children
            .iter()
            .find_map(|child| subtree_subject(child))
    }
}

fn re_depth(subject: &str) -> usize {
    let (_, depth) = _bare_subject(subject);
    depth
}

fn subject_re_order<T: Message>(a: &Thread<T>, b: &Thread<T>) -> Ordering {
    match (a.message.as_ref(), b.message.as_ref()) {
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (None, None) => Ordering::Equal,
        (Some(amsg), Some(bmsg)) => {
            let a_d = re_depth(amsg.subject().unwrap_or(""));
            let b_d = re_depth(bmsg.subject().unwrap_or(""));
            a_d.cmp(&b_d)
        }
    }
}

fn group_by_subject<T: Message>(threads: Vec<Thread<T>>) -> Vec<Thread<T>> {
    let mut subject_table: HashMap<String, Thread<T>> = HashMap::new();
    let mut orphans: Vec<Thread<T>> = Vec::new(); // no subject -> pass-through

    for node in threads {
        let bare = match subtree_subject(&node) {
            Some(s) => bare_subject(&s),
            None => {
                orphans.push(node);
                continue;
            }
        };
        if bare.is_empty() {
            orphans.push(node);
            continue;
        }

        match subject_table.remove(&bare) {
            None => {
                subject_table.insert(bare, node);
            }
            Some(existing) => {
                let (winner, loser) = match subject_re_order(&node, &existing) {
                    Ordering::Less => (node, existing),
                    _ => (existing, node),
                };
                subject_table.insert(bare, merge_threads(winner, loser));
            }
        }
    }

    subject_table.into_values().chain(orphans).collect()
}

fn merge_threads<T: Message>(winner: Thread<T>, loser: Thread<T>) -> Thread<T> {
    match (winner.message.is_none(), loser.message.is_none()) {
        // Both empty → merge children into winner
        (true, true) => {
            let mut winner = winner;
            winner.children.extend(loser.children);
            winner
        }
        // Winner is ghost, loser is real → real becomes child of ghost
        (true, false) => {
            let mut winner = winner;
            winner.children.push(loser);
            winner
        }
        // Both real → siblings under a synthetic empty root
        (false, false) => Thread {
            message: None,
            children: vec![winner, loser],
        },
        // Loser is ghost, winner is real — shouldn't happen, but handle gracefully
        (false, true) => {
            let mut loser = loser;
            loser.children.push(winner);
            loser
        }
    }
}

/// Recursively sort siblings within every thread subtree by
/// `received_ts` (oldest first).  Ghost nodes (no message) sort last.
pub fn sort_threads_by_date<T: Message>(threads: &mut [Thread<T>]) {
    for thread in threads.iter_mut() {
        sort_threads_by_date(&mut thread.children);
    }
    threads.sort_by(|a, b| {
        let a_ts = a.message.as_ref().map(|m| m.received_ts()).unwrap_or(i64::MAX);
        let b_ts = b.message.as_ref().map(|m| m.received_ts()).unwrap_or(i64::MAX);
        a_ts.cmp(&b_ts)
    });
}

/// Build a message-ID → top-level thread index lookup.
///
/// After calling `thread_messages`, use this to quickly find which
/// top-level thread a given message belongs to.  All messages in a
/// thread (including deeply nested replies) map to the same index.
pub fn build_thread_index<T: Message>(threads: &[Thread<T>]) -> HashMap<String, usize> {
    let mut index = HashMap::new();
    for (i, thread) in threads.iter().enumerate() {
        index_thread(thread, i, &mut index);
    }
    index
}

fn index_thread<T: Message>(thread: &Thread<T>, ndx: usize, index: &mut HashMap<String, usize>) {
    if let Some(ref msg) = thread.message {
        if let Some(id) = msg.message_id() {
            index.insert(id.to_string(), ndx);
        }
    }
    for child in &thread.children {
        index_thread(child, ndx, index);
    }
}

/// Run the JWZ threading algorithm on an unordered collection of messages.
///
/// Returns the root nodes of the thread tree — messages that have no
/// parent in the set. Messages sharing the same bare subject without
/// explicit reference links are grouped under a synthetic root.
pub fn thread_messages<T: Message>(messages: impl IntoIterator<Item = T>) -> Vec<Thread<T>> {
    let mut cs: Vec<Container<T>> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    for msg in messages {
        // For each message, ensure Container exists
        let c_idx = if let Some(id) = msg.message_id() {
            *seen.entry(id.to_string()).or_insert_with(|| {
                let idx = cs.len();
                cs.push(Container::new(None));
                idx
            })
        } else {
            // Message ID missing, assign synthetic one.
            // At later stage, will attempt to build threads from Subject line
            let idx = cs.len();
            cs.push(Container::new(None));
            idx
        };

        // (Create) and link all containers for messages identified in 'References' header
        let mut prev: Option<usize> = None;
        for ref_msgid in msg.references() {
            let ref_idx = *seen.entry(ref_msgid.clone()).or_insert_with(|| {
                let idx = cs.len();
                cs.push(Container::new(None));
                idx
            });

            // should link 'prev' (if set) as parent of current
            //   EXCEPT - we can't add a link if it introduces a loop
            //   Before A->B;
            //   * search down B to see if A is reachable
            //   * search Down A to see if B is reachable
            //
            //   If EITHER is reachable from the other, don't add a link
            if let Some(p) = prev
                && cs[p].parent.is_none()
                && !is_ancestor(p, ref_idx, &cs)
                && !is_ancestor(ref_idx, p, &cs)
            {
                cs[p].parent = Some(ref_idx);
                cs[ref_idx].children.push(p);
            }

            prev = Some(ref_idx)
        }
        //FINALLY; add LAST element in References (** augmented with In-Reply-To)
        //         as the parent of this message
        // Set this message's parent to the last reference — but only if doing
        // so would not create a cycle (skip self-parenting, and skip if this
        // container is already an ancestor of the proposed parent). The
        // reference-linking loop above guards itself; this final step must too.
        if let Some(actual_parent_idx) = prev
            && actual_parent_idx != c_idx
            && !is_ancestor(c_idx, actual_parent_idx, &cs)
        {
            if let Some(parent_idx) = cs[c_idx].parent {
                // already has a parent - ensure we remove this
                // container as a child before changing its parent value
                let cp = &mut cs[parent_idx];
                cp.children.retain(|&idx| idx != c_idx)
            }
            let c = &mut cs[c_idx];
            c.parent = Some(actual_parent_idx);
            let cp = &mut cs[actual_parent_idx];
            cp.children.push(c_idx);
        }

        cs[c_idx].message = Some(msg);
    }

    // Step 2 - find the root set
    let cs_roots: Vec<usize> = cs
        .iter()
        .enumerate()
        .filter_map(|(ndx, val)| match val.parent {
            None => Some(ndx),
            Some(_) => None,
        })
        .collect();

    // Step 3 - can discard `seen`
    drop(seen);

    // Step 4 - Prune empty containers
    //
    //  Removing a container:
    //  remove parent.children ref
    //  re-parent all its children to ITS parent
    let mut visited: HashSet<usize> = HashSet::new();
    let threads: Vec<Thread<T>> = cs_roots
        .into_iter()
        .flat_map(|idx| build_thread_subtree(idx, &mut cs, &mut visited))
        .collect();

    // old hierarchy in `cs` is stale - `build_thread_subtree`
    // calls built the new hierarchy.
    drop(cs);

    // Step 5 - Group root set by subject
    let mut threads = group_by_subject(threads);

    // Step 6 - Sort siblings by received date (oldest first) at every level
    sort_threads_by_date(&mut threads);

    threads
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

/// Simple test message for unit tests.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestMessage {
        id: String,
        refs: Vec<String>,
        subject: String,
        received_ts: i64,
    }

    impl Message for TestMessage {
        fn message_id(&self) -> Option<&str> {
            Some(&self.id)
        }

        fn references(&self) -> &[String] {
            &self.refs
        }

        fn subject(&self) -> Option<&str> {
            Some(&self.subject)
        }

        fn received_ts(&self) -> i64 {
            self.received_ts
        }
    }

    /// Helper: collect message IDs in DFS order for easy assertions.
    /// Skips ghost nodes (no message).
    fn collect_ids<T: Message>(threads: &[Thread<T>]) -> Vec<Vec<String>> {
        fn walk<T: Message>(t: &Thread<T>, acc: &mut Vec<String>) {
            if let Some(ref msg) = t.message {
                acc.push(msg.message_id().unwrap().to_string());
            }
            for child in &t.children {
                walk(child, acc);
            }
        }
        threads
            .iter()
            .map(|t| {
                let mut ids = Vec::new();
                walk(t, &mut ids);
                ids
            })
            .collect()
    }

    // ── Cycle safety ─────────────────────────────────────────────
    #[test]
    fn cyclic_references_terminate() {
        // Malformed data: A references B and B references A. Before the cycle
        // guards this would loop forever in is_ancestor / build_thread_subtree.
        let msgs = vec![
            TestMessage { id: "a".into(), refs: vec!["b".into()], subject: "x".into(), received_ts: 1 },
            TestMessage { id: "b".into(), refs: vec!["a".into()], subject: "x".into(), received_ts: 2 },
        ];
        let threads = thread_messages(msgs);
        // Both messages must appear exactly once across the forest.
        let mut ids: Vec<String> = collect_ids(&threads).into_iter().flatten().collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn self_reference_terminates() {
        let msgs = vec![TestMessage {
            id: "a".into(),
            refs: vec!["a".into()],
            subject: "x".into(),
            received_ts: 1,
        }];
        let threads = thread_messages(msgs);
        assert_eq!(collect_ids(&threads), vec![vec!["a".to_string()]]);
    }

    // ── Bare Subject Tests ───────────────────────────────────────
    #[test]
    fn subj_no_prefix() {
        assert_eq!(bare_subject("Hello world"), "Hello world");
    }

    #[test]
    fn subj_simple_re() {
        assert_eq!(bare_subject("Re: Hello"), "Hello");
    }

    #[test]
    fn subj_uppercase_re() {
        assert_eq!(bare_subject("RE: Hello"), "Hello");
    }

    #[test]
    fn subj_mixed_case_re() {
        assert_eq!(bare_subject("rE: Hello"), "Hello");
    }

    #[test]
    fn subj_numbered_re() {
        assert_eq!(bare_subject("Re[5]: Hello"), "Hello");
    }

    #[test]
    fn subj_chained_re_prefixes() {
        assert_eq!(bare_subject("Re: Re[4]: Re: Hello"), "Hello");
    }

    #[test]
    fn subj_only_prefix_no_content() {
        assert_eq!(bare_subject("Re: "), "");
    }

    #[test]
    fn subj_only_re_with_colon() {
        assert_eq!(bare_subject("Re:"), "");
    }

    #[test]
    fn subj_leading_whitespace() {
        assert_eq!(bare_subject("   Re: Hello"), "Hello");
    }

    #[test]
    fn subj_empty_string() {
        assert_eq!(bare_subject(""), "");
    }

    #[test]
    fn subj_whitespace_only() {
        assert_eq!(bare_subject("   "), "");
    }

    #[test]
    fn subj_not_a_re_prefix() {
        // "Regarding: Hello" starts with "Re" but isn't a Re: prefix
        assert_eq!(bare_subject("Regarding: Hello"), "Regarding: Hello");
    }

    #[test]
    fn subj_re_in_middle_of_word() {
        // "Re" must be at the start (after whitespace) to count
        assert_eq!(bare_subject("Care: Hello"), "Care: Hello");
    }

    // ── Threading: empty & single ──────────────────────────────

    #[test]
    fn empty_input() {
        let messages: Vec<TestMessage> = vec![];
        let threads = thread_messages(messages);
        assert!(threads.is_empty());
    }

    #[test]
    fn single_message() {
        let messages = vec![TestMessage {
            id: "a".into(),
            refs: vec![],
            subject: "Hello".into(),
            received_ts: 0,
        }];
        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        assert_eq!(ids, vec![vec!["a"]]);
    }

    // ── Threading: reference-based ──────────────────────────────

    #[test]
    fn linear_thread_by_references() {
        let messages = vec![
            TestMessage {
                id: "a".into(),
                refs: vec![],
                subject: "Hello".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "b".into(),
                refs: vec!["a".into()],
                subject: "Re: Hello".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "c".into(),
                refs: vec!["a".into(), "b".into()],
                subject: "Re: Hello".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // One thread: a → b → c
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], vec!["a", "b", "c"]);
    }

    #[test]
    fn diamond_references() {
        // A and B are independent, C references both → C should be sibling
        // of whichever was linked first. JWZ: A and B end up under B.
        let messages = vec![
            TestMessage {
                id: "c".into(),
                refs: vec!["a".into(), "b".into()],
                subject: "Re: Topic".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "a".into(),
                refs: vec![],
                subject: "Topic".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "b".into(),
                refs: vec![],
                subject: "Re: Topic".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // Subject grouping may merge these. We just check nothing is lost.
        let all_ids: Vec<String> = ids.iter().flatten().cloned().collect();
        assert_eq!(all_ids.len(), 3);
    }

    // ── Threading: ghost containers ─────────────────────────────

    #[test]
    fn ghost_parent_single_child() {
        // Only "b" exists, it references nonexistent "a".
        // Ghost "a" has 1 child → promoted away. Result: [b].
        let messages = vec![TestMessage {
            id: "b".into(),
            refs: vec!["a".into()],
            subject: "Re: Hello".into(),
            received_ts: 0,
        }];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], vec!["b"]);
    }

    #[test]
    fn ghost_chain_single_child() {
        // "c" refs "b", "b" refs "a". Only "c" exists.
        // Ghosts "a" and "b" each have 1 child → both promoted. Result: [c].
        let messages = vec![TestMessage {
            id: "c".into(),
            refs: vec!["a".into(), "b".into()],
            subject: "Re: Hello".into(),
            received_ts: 0,
        }];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], vec!["c"]);
    }

    #[test]
    fn ghost_with_multiple_children_kept() {
        // Two messages both reference the same nonexistent "a".
        // Ghost "a" has 2 children and is a root → kept as ghost root.
        let messages = vec![
            TestMessage {
                id: "x".into(),
                refs: vec!["a".into()],
                subject: "Re: Topic".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "y".into(),
                refs: vec!["a".into()],
                subject: "Re: Topic".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // Ghost "a" keeps x and y as siblings underneath.
        // After step 5 (subject grouping), they may end up under synthetic root.
        // Either way: one thread, both messages present.
        assert_eq!(ids.len(), 1);
        let all_ids: Vec<String> = ids.iter().flatten().cloned().collect();
        assert_eq!(all_ids.len(), 2);
    }

    // ── Threading: subject-based grouping ───────────────────────

    #[test]
    fn subject_based_grouping_siblings() {
        // Three messages, same bare subject, no references → siblings under
        // synthetic root.
        let messages = vec![
            TestMessage {
                id: "1".into(),
                refs: vec![],
                subject: "Meeting notes".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "2".into(),
                refs: vec![],
                subject: "Re: Meeting notes".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "3".into(),
                refs: vec![],
                subject: "Re: Re: Meeting notes".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // All three under one thread
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].len(), 3);
    }

    #[test]
    fn subject_grouping_with_ghost_root() {
        // Two messages reference nonexistent parent + a third standalone
        // message, all with same bare subject.
        // Ghost root (depth 0) wins table slot, absorbs the standalone.
        let messages = vec![
            TestMessage {
                id: "x".into(),
                refs: vec!["ghost".into()],
                subject: "Re: Topic".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "y".into(),
                refs: vec!["ghost".into()],
                subject: "Re: Re: Topic".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "z".into(),
                refs: vec![],
                subject: "Topic".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // All three should be in one thread
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].len(), 3);
    }

    #[test]
    fn different_subjects_not_grouped() {
        let messages = vec![
            TestMessage {
                id: "a".into(),
                refs: vec![],
                subject: "Hello".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "b".into(),
                refs: vec![],
                subject: "World".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        assert_eq!(ids.len(), 2);
    }

    // ── Threading: mixed ────────────────────────────────────────

    #[test]
    fn independent_threads() {
        let messages = vec![
            TestMessage {
                id: "x".into(),
                refs: vec![],
                subject: "Alpha".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "y".into(),
                refs: vec!["x".into()],
                subject: "Re: Alpha".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "p".into(),
                refs: vec![],
                subject: "Beta".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "q".into(),
                refs: vec![],
                subject: "Gamma".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // x and y linked by reference → one thread. p and q solo → two more.
        // Total: 3 threads. Order not guaranteed, so check by content.
        assert_eq!(ids.len(), 3);
        let mut sorted: Vec<Vec<String>> = ids
            .into_iter()
            .map(|mut v| {
                v.sort();
                v
            })
            .collect();
        sorted.sort_by(|a, b| a[0].cmp(&b[0]));
        assert_eq!(sorted[0], vec!["p"]);
        assert_eq!(sorted[1], vec!["q"]);
        assert_eq!(sorted[2], vec!["x", "y"]);
    }

    #[test]
    fn references_beat_subject() {
        // "a" references "b" explicitly → already threaded by reference.
        // They share bare subject "Hello" but subject grouping should NOT
        // create an extra synthetic root because they're already in a tree.
        // (In JWZ, subject grouping only operates on the root set, and in
        // this case only "b" is a root.)
        let messages = vec![
            TestMessage {
                id: "a".into(),
                refs: vec!["b".into()],
                subject: "Re: Hello".into(),
            received_ts: 0,
            },
            TestMessage {
                id: "b".into(),
                refs: vec![],
                subject: "Hello".into(),
            received_ts: 0,
            },
        ];

        let threads = thread_messages(messages);
        let ids = collect_ids(&threads);
        // One thread: b → a (not siblings under synthetic root)
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], vec!["b", "a"]);
    }

    #[test]
    fn message_without_id() {
        // Messages without Message-ID get synthetic IDs, can still be
        // grouped by subject.
        // This test documents current behavior — TestMessage always has an
        // ID. A real implementation with optional IDs needs its own test.
        // For now, verify the algorithm doesn't crash on normal messages.
        let messages = vec![TestMessage {
            id: "has_id".into(),
            refs: vec![],
            subject: "Topic".into(),
            received_ts: 0,
        }];
        let threads = thread_messages(messages);
        assert_eq!(threads.len(), 1);
    }
}
