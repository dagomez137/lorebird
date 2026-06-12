//! GObject model for a sidebar folder entry.
//!
//! Each `FolderItem` represents a row in the folder sidebar:
//! a profile header, "All Mail", a saved view, a separator, or a placeholder.

mod imp {
    use glib::prelude::*;
    use glib::subclass::prelude::*;
    use glib::Properties;
    use std::cell::{Cell, RefCell};

    #[derive(Properties, Default)]
    #[properties(wrapper_type = super::FolderItem)]
    pub struct FolderItemInner {
        #[property(get, set)]
        name: RefCell<String>,
        #[property(get, set)]
        icon_name: RefCell<String>,
        #[property(get, set)]
        count: Cell<u32>,
        /// Profile headers are styled differently.
        #[property(get, set)]
        is_header: Cell<bool>,
        /// The profile label this row belongs to.
        #[property(get, set)]
        profile_label: RefCell<String>,
        /// Query string for view rows (empty for non-view rows).
        #[property(get, set)]
        query: RefCell<String>,
        /// Row kind: "profile-header", "all-mail", "view",
        /// "separator", or "placeholder".
        #[property(get, set)]
        row_kind: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FolderItemInner {
        const NAME: &'static str = "LorebirdFolderItem";
        type Type = super::FolderItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for FolderItemInner {}
}

glib::wrapper! {
    pub struct FolderItem(ObjectSubclass<imp::FolderItemInner>);
}

impl FolderItem {
    /// Create a profile header row.
    pub fn profile_header(label: &str) -> Self {
        glib::Object::builder()
            .property("name", label)
            .property("icon-name", "network-workgroup")
            .property("count", 0u32)
            .property("is-header", true)
            .property("profile-label", label)
            .property("query", "")
            .property("row-kind", "profile-header")
            .build()
    }

    /// Create an "All Mail" row under a profile.
    pub fn all_mail(profile_label: &str) -> Self {
        glib::Object::builder()
            .property("name", "All Mail")
            .property("icon-name", "mail-mailbox")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", profile_label)
            .property("query", "")
            .property("row-kind", "all-mail")
            .build()
    }

    /// Create a "Drafts" row under a profile.
    pub fn drafts(profile_label: &str) -> Self {
        glib::Object::builder()
            .property("name", "Drafts")
            .property("icon-name", "document-edit-symbolic")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", profile_label)
            .property("query", "")
            .property("row-kind", "drafts")
            .build()
    }

    /// Create a saved view row under a profile.
    pub fn view(profile_label: &str, view_label: &str, query: &str) -> Self {
        glib::Object::builder()
            .property("name", view_label)
            .property("icon-name", "folder-saved-search")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", profile_label)
            .property("query", query)
            .property("row-kind", "view")
            .build()
    }

    /// Create a visual separator (non-selectable).
    pub fn separator() -> Self {
        glib::Object::builder()
            .property("name", "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}")
            .property("icon-name", "")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", "")
            .property("query", "")
            .property("row-kind", "separator")
            .build()
    }

    /// Create a placeholder message (non-selectable).
    pub fn placeholder(text: &str) -> Self {
        glib::Object::builder()
            .property("name", text)
            .property("icon-name", "")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", "")
            .property("query", "")
            .property("row-kind", "placeholder")
            .build()
    }
}