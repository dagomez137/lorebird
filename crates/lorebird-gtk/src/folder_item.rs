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
        /// Row kind, stored as the string representation of [`FolderKind`].
        #[property(get, set)]
        row_kind: RefCell<String>,
        /// True for the inbox view — followed series flagged "add to inbox"
        /// are merged into its query at runtime.
        #[property(get, set)]
        is_inbox: Cell<bool>,
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

/// The kind of sidebar row. Used instead of bare strings so the
/// compiler catches typos and every variant is documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    ProfileHeader,
    AllMail,
    Drafts,
    View,
    /// The "Followed" section header (non-selectable).
    FollowHeader,
    /// A followed-series row (behaves like a view).
    Follow,
    Separator,
    Placeholder,
}

impl FolderKind {
    /// The string stored in the GObject `row-kind` property.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProfileHeader => "profile-header",
            Self::AllMail => "all-mail",
            Self::Drafts => "drafts",
            Self::View => "view",
            Self::FollowHeader => "follow-header",
            Self::Follow => "follow",
            Self::Separator => "separator",
            Self::Placeholder => "placeholder",
        }
    }

    /// Parse a row-kind string back into a FolderKind.
    /// Returns None for unrecognised values.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "profile-header" => Some(Self::ProfileHeader),
            "all-mail" => Some(Self::AllMail),
            "drafts" => Some(Self::Drafts),
            "view" => Some(Self::View),
            "follow-header" => Some(Self::FollowHeader),
            "follow" => Some(Self::Follow),
            "separator" => Some(Self::Separator),
            "placeholder" => Some(Self::Placeholder),
            _ => None,
        }
    }
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
            .property("row-kind", FolderKind::ProfileHeader.as_str())
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
            .property("row-kind", FolderKind::AllMail.as_str())
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
            .property("row-kind", FolderKind::Drafts.as_str())
            .build()
    }

    /// Create a saved view row under a profile.
    pub fn view(profile_label: &str, view_label: &str, query: &str, is_inbox: bool) -> Self {
        glib::Object::builder()
            .property("name", view_label)
            .property("icon-name", "folder-saved-search")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", profile_label)
            .property("query", query)
            .property("row-kind", FolderKind::View.as_str())
            .property("is-inbox", is_inbox)
            .build()
    }

    /// Create the "Followed" section header (non-selectable).
    pub fn follow_header() -> Self {
        glib::Object::builder()
            .property("name", "Followed")
            .property("icon-name", "starred-symbolic")
            .property("count", 0u32)
            .property("is-header", true)
            .property("profile-label", "")
            .property("query", "")
            .property("row-kind", FolderKind::FollowHeader.as_str())
            .build()
    }

    /// Create a followed-series row. Behaves like a view, run against the
    /// given profile's maildir.
    pub fn follow(profile_label: &str, label: &str, query: &str) -> Self {
        glib::Object::builder()
            .property("name", label)
            .property("icon-name", "non-starred-symbolic")
            .property("count", 0u32)
            .property("is-header", false)
            .property("profile-label", profile_label)
            .property("query", query)
            .property("row-kind", FolderKind::Follow.as_str())
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
            .property("row-kind", FolderKind::Separator.as_str())
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
            .property("row-kind", FolderKind::Placeholder.as_str())
            .build()
    }
}