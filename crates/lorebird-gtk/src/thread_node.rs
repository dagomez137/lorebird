//! GObject model for a thread node in the mail list.
//!
//! Each `ThreadNode` represents a single message visible in the
//! `ColumnView` tree.  Child messages (replies) are stored in a
//! lazily-created `gio::ListStore` so that `TreeListModel` can
//! discover them.
//!
//! Two timestamp properties track thread activity:
//! - `started-ts`: unix timestamp of the root message
//! - `last-reply-ts`: unix timestamp of the most recent message
//!
//! The display strings (`started`, `last-reply`) are relative-time
//! labels like "2h ago" derived from the timestamps at creation time.

mod imp {
    use gio::ListStore;
    use glib::prelude::*;
    use glib::subclass::prelude::*;
    use glib::Properties;
    use std::cell::{Cell, OnceCell, RefCell};

    #[derive(Properties, Default)]
    #[properties(wrapper_type = super::ThreadNode)]
    pub struct ThreadNodeInner {
        #[property(get, set)]
        subject: RefCell<String>,
        #[property(get, set)]
        sender: RefCell<String>,
        #[property(get, set)]
        to_addrs: RefCell<String>,
        #[property(get, set)]
        cc_addrs: RefCell<String>,
        /// "Started" column: relative-time display string.
        #[property(get, set)]
        started: RefCell<String>,
        /// "Last Reply" column: relative-time display string.
        #[property(get, set)]
        last_reply: RefCell<String>,
        /// Unix timestamp of the root message — used for sorting.
        #[property(get, set)]
        started_ts: Cell<i64>,
        /// Unix timestamp of the most recent message — used for sorting.
        #[property(get, set)]
        last_reply_ts: Cell<i64>,
        #[property(get, set)]
        has_children: Cell<bool>,
        /// Set while this node is part of the selected conversation; drives
        /// the whole-thread tint.
        #[property(get, set)]
        in_selected_thread: Cell<bool>,
        #[property(get, set)]
        body_preview: RefCell<String>,
        /// The Message-ID header of this message (for In-Reply-To).
        #[property(get, set)]
        message_id: RefCell<String>,
        /// Space-separated References header value (for threading Replies).
        #[property(get, set)]
        references_str: RefCell<String>,
        /// The In-Reply-To header value (single message-id).
        #[property(get, set)]
        in_reply_to: RefCell<String>,
        /// Formatted date string (for quote attribution: "On , wrote:").
        #[property(get, set)]
        date_str: RefCell<String>,
        /// Filename of the message in the maildir (for re-reading raw headers).
        #[property(get, set)]
        filename: RefCell<String>,
        /// Children list — not a GObject property, only used by
        /// `TreeListModel` to discover child rows.
        pub children: OnceCell<ListStore>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ThreadNodeInner {
        const NAME: &'static str = "LorebirdThreadNode";
        type Type = super::ThreadNode;
    }

    #[glib::derived_properties]
    impl ObjectImpl for ThreadNodeInner {}
}

glib::wrapper! {
    pub struct ThreadNode(ObjectSubclass<imp::ThreadNodeInner>);
}

impl ThreadNode {
    /// Create a new thread node.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        subject: &str,
        from_addr: &str,
        to_addrs: &str,
        cc_addrs: &str,
        started: &str,
        last_reply: &str,
        started_ts: i64,
        last_reply_ts: i64,
        message_id: &str,
        references_str: &str,
        in_reply_to: &str,
        date_str: &str,
        filename: &str,
    ) -> Self {
        glib::Object::builder()
            .property("subject", subject)
            .property("sender", from_addr)
            .property("to-addrs", to_addrs)
            .property("cc-addrs", cc_addrs)
            .property("started", started)
            .property("last-reply", last_reply)
            .property("started-ts", started_ts)
            .property("last-reply-ts", last_reply_ts)
            .property("has-children", false)
            .property("body-preview", String::new())
            .property("message-id", message_id)
            .property("references-str", references_str)
            .property("in-reply-to", in_reply_to)
            .property("date-str", date_str)
            .property("filename", filename)
            .build()
    }

    /// Return the `ListStore` of child nodes, creating it on first access.
    pub fn children_store(&self) -> &gio::ListStore {
        use glib::subclass::types::ObjectSubclassIsExt;
        self.imp().children.get_or_init(gio::ListStore::new::<ThreadNode>)
    }

    /// Append a child node and mark this node as having children.
    pub fn add_child(&self, child: &ThreadNode) {
        self.children_store().append(child);
        self.set_has_children(true);
    }
}