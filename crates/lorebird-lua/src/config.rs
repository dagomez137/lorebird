//! Configuration types for lorebird.
//!
//! These types represent the data portion of a Lua config file,
//! deserialised via `mlua::serde`.  Lua function hooks (`on_fetch`,
//! `on_reply`, `on_send`) are stored separately in [`ProfileHooks`]
//! and [`GlobalHooks`] since they cannot be serde-deserialised.
//!
//! Name/email resolution follows a cascade: per-profile values
//! override global `user` defaults.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use lorebird_sendmail::SmtpConfig;

// ── Data types (serde-deserialisable) ─────────────────────────────

/// Global user identity — provides default `name` and `email`
/// for profiles that don't define their own.
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// A saved view — a named query string shown in the sidebar.
#[derive(Debug, Clone, Deserialize)]
pub struct ViewConfig {
    pub label: String,
    pub query: String,
    /// When true, followed series marked "add to inbox" are OR-ed into this
    /// view's query at runtime. Set on the profile's inbox view.
    #[serde(default)]
    pub inbox: bool,
}

/// Per-profile data (deserialisable from Lua, **excludes** hooks).
///
/// The `on_fetch` function is extracted separately as an
/// [`mlua::Function`] and stored in [`ProfileHooks`].
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileData {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    pub maildir: String,
    #[serde(default)]
    pub views: Vec<ViewConfig>,
    #[serde(default)]
    pub smtp: Option<SmtpConfig>,
}

/// A named group of addresses that stand out as a coloured pill in the
/// recipient fields. `color` is a palette name (blue, green, orange, red,
/// purple, teal, yellow) or a `#rrggbb` hex value; `match` is a list of
/// case-insensitive substrings tested against each address.
#[derive(Debug, Clone, Deserialize)]
pub struct ContactGroup {
    #[serde(default)]
    pub name: String,
    pub color: String,
    #[serde(default, rename = "match")]
    pub patterns: Vec<String>,
}

/// External editor for the compose body. `command` is an argv template that
/// must open a terminal running the editor, since a TUI editor like `hx` needs
/// a tty the GTK process cannot provide (e.g. `{ "alacritty", "--command",
/// "hx", "{file}" }`). The `{file}` element is replaced with the temp-file
/// path; if it is absent the path is appended as the last argument. Only the
/// body round-trips through the editor; headers stay in the UI.
#[derive(Debug, Clone, Deserialize)]
pub struct EditorConfig {
    pub command: Vec<String>,
    /// Launch the editor automatically when the compose window opens.
    #[serde(default)]
    pub on_open: bool,
    /// Temp-file suffix so the editor can pick a filetype. Defaults to `.eml`.
    #[serde(default = "default_editor_file_suffix")]
    pub file_suffix: String,
}

/// Top-level config data (deserialisable from Lua, **excludes** hooks).
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub user: Option<UserInfo>,
    /// "light" or "dark".  Defaults to "light".
    #[serde(default = "default_theme")]
    pub theme: String,
    /// UI scale factor (1.0 = no scaling).  Applied as a multiplier to
    /// the GTK Xft DPI setting.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f64,
    /// Maximum number of recent messages loaded and threaded into the query
    /// working set. The query worker loads this many newest messages, threads
    /// them once, and filters views in memory against that cache. A value
    /// below the maildir's message count leaves older messages unthreaded, so
    /// replies whose parent falls outside the window show up orphaned; keep it
    /// above the maildir size. Larger values cost memory and a slower first
    /// cache build. Defaults to [`DEFAULT_WORKING_SET_LIMIT`].
    #[serde(default = "default_working_set_limit")]
    pub working_set_limit: usize,
    /// Show full From/To/Cc in the preview by default instead of truncating
    /// long recipient lists behind the expand toggle.
    #[serde(default)]
    pub expand_headers: bool,
    /// Default reading-pane width in monospace columns. 100 fits a kernel
    /// patch (`checkpatch.pl` caps lines at 100), an 80-column quoted reply
    /// and a 75-column commit log without wrapping. The divider stays draggable.
    #[serde(default = "default_reading_pane_columns")]
    pub reading_pane_columns: usize,
    /// Tighten the thread list: drop the inter-row spacing for a denser,
    /// subject-focused list. Off by default (the roomier two-line layout).
    #[serde(default)]
    pub compact_list: bool,
    /// Address groups coloured as pills in the recipient fields.
    #[serde(default)]
    pub contact_groups: Vec<ContactGroup>,
    /// Optional external editor for the compose body. Absent disables the
    /// feature and the body is edited in the in-window editor only.
    #[serde(default)]
    pub editor: Option<EditorConfig>,
    pub profiles: HashMap<String, ProfileData>,
}

/// Default working-set size when `working_set_limit` is unset. Chosen well
/// above a typical bounded maildir so every message threads, while still
/// capping a runaway maildir from threading millions of rows in memory.
pub const DEFAULT_WORKING_SET_LIMIT: usize = 500_000;

/// Default reading-pane width when `reading_pane_columns` is unset.
pub const DEFAULT_READING_PANE_COLUMNS: usize = 100;

fn default_theme() -> String {
    "light".to_string()
}

fn default_ui_scale() -> f64 {
    1.0
}

fn default_working_set_limit() -> usize {
    DEFAULT_WORKING_SET_LIMIT
}

fn default_reading_pane_columns() -> usize {
    DEFAULT_READING_PANE_COLUMNS
}

fn default_editor_file_suffix() -> String {
    ".eml".to_string()
}

// ── Hook types (Lua function handles) ──────────────────────────────

/// Per-profile hook handles (not serde-deserialisable).
pub struct ProfileHooks {
    /// Called when the user triggers a mail fetch for this profile.
    /// Should return a truthy value on success.
    pub on_fetch: Option<mlua::Function>,
}

/// Global hook handles (not serde-deserialisable).
pub struct GlobalHooks {
    /// Optional reply hook — receives a pre-filled mail table and
    /// can modify it before the compose window opens.
    pub on_reply: Option<mlua::Function>,
    /// Required send hook — delivers the composed mail file.
    pub on_send: Option<mlua::Function>,
}

// ── Fully resolved profile ─────────────────────────────────────────

/// A profile with inherited defaults filled in.
///
/// Created by calling [`ProfileData::resolve`] with the global
/// `UserInfo` cascade.
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub label: String,
    pub name: String,
    pub email: String,
    pub maildir: PathBuf,
    pub views: Vec<ViewConfig>,
    pub smtp: Option<SmtpConfig>,
}

impl ProfileData {
    /// Resolve name and email, falling back to global defaults.
    ///
    /// If neither the profile nor the global config provides a value,
    /// sensible defaults are used ("Anonymous" / "unknown@localhost").
    pub fn resolve(&self, label: &str, global: Option<&UserInfo>) -> ResolvedProfile {
        ResolvedProfile {
            label: label.to_string(),
            name: self
                .name
                .as_deref()
                .or(global.and_then(|u| u.name.as_deref()))
                .unwrap_or("Anonymous")
                .to_string(),
            email: self
                .email
                .as_deref()
                .or(global.and_then(|u| u.email.as_deref()))
                .unwrap_or("unknown@localhost")
                .to_string(),
            maildir: PathBuf::from(&self.maildir),
            views: self.views.clone(),
            smtp: self.smtp.clone(),
        }
    }
}

impl AppConfig {
    /// Resolve all profiles, filling in name/email from global defaults.
    pub fn resolve_all(&self) -> HashMap<String, ResolvedProfile> {
        let global = self.user.as_ref();
        self.profiles
            .iter()
            .map(|(label, data)| (label.clone(), data.resolve(label, global)))
            .collect()
    }
}

// ── Loaded config (data + hooks together) ───────────────────────────

/// The result of loading a Lua config file: data + extracted hooks.
pub struct LoadedConfig {
    /// Deserialisable config data.
    pub config: AppConfig,
    /// Per-profile hook handles (keyed by profile label).
    pub profile_hooks: HashMap<String, ProfileHooks>,
    /// Global hook handles.
    pub global_hooks: GlobalHooks,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_global() -> UserInfo {
        UserInfo {
            name: Some("Global User".to_string()),
            email: Some("global@example.com".to_string()),
        }
    }

    #[test]
    fn resolve_profile_with_local_overrides() {
        let data = ProfileData {
            name: Some("Local User".to_string()),
            email: Some("local@example.com".to_string()),
            maildir: "/tmp/mail".to_string(),
            views: vec![ViewConfig {
                label: "patches".to_string(),
                query: "subject:[PATCH]".to_string(),
                inbox: false,
            }],
            smtp: None,
        };
        let resolved = data.resolve("test", Some(&make_global()));
        assert_eq!(resolved.name, "Local User");
        assert_eq!(resolved.email, "local@example.com");
        assert_eq!(resolved.label, "test");
        assert_eq!(resolved.maildir, PathBuf::from("/tmp/mail"));
        assert_eq!(resolved.views.len(), 1);
    }

    #[test]
    fn resolve_profile_falls_back_to_global() {
        let data = ProfileData {
            name: None,
            email: None,
            maildir: "/tmp/mail".to_string(),
            views: vec![],
            smtp: None,
        };
        let resolved = data.resolve("test", Some(&make_global()));
        assert_eq!(resolved.name, "Global User");
        assert_eq!(resolved.email, "global@example.com");
    }

    #[test]
    fn resolve_profile_with_no_global() {
        let data = ProfileData {
            name: None,
            email: None,
            maildir: "/tmp/mail".to_string(),
            views: vec![],
            smtp: None,
        };
        let resolved = data.resolve("test", None);
        assert_eq!(resolved.name, "Anonymous");
        assert_eq!(resolved.email, "unknown@localhost");
    }

    #[test]
    fn resolve_profile_partial_override() {
        // Profile sets name but not email → inherits email from global
        let data = ProfileData {
            name: Some("Local Name".to_string()),
            email: None,
            maildir: "/tmp/mail".to_string(),
            views: vec![],
            smtp: None,
        };
        let resolved = data.resolve("test", Some(&make_global()));
        assert_eq!(resolved.name, "Local Name");
        assert_eq!(resolved.email, "global@example.com");
    }

    #[test]
    fn resolve_all_profiles() {
        let config = AppConfig {
            user: Some(UserInfo {
                name: Some("Default".to_string()),
                email: Some("default@example.com".to_string()),
            }),
            theme: "light".to_string(),
            ui_scale: 1.0,
            working_set_limit: DEFAULT_WORKING_SET_LIMIT,
            expand_headers: false,
            reading_pane_columns: DEFAULT_READING_PANE_COLUMNS,
            compact_list: false,
            contact_groups: Vec::new(),
            editor: None,
            profiles: {
                let mut m = HashMap::new();
                m.insert(
                    "work".to_string(),
                    ProfileData {
                        name: Some("Work User".to_string()),
                        email: Some("work@example.com".to_string()),
                        maildir: "/tmp/work".to_string(),
                        views: vec![],
                        smtp: None,
                    },
                );
                m.insert(
                    "personal".to_string(),
                    ProfileData {
                        name: None,
                        email: None,
                        maildir: "/tmp/personal".to_string(),
                        views: vec![ViewConfig {
                            label: "inbox".to_string(),
                            query: "date:1w..".to_string(),
                            inbox: false,
                        }],
                        smtp: None,
                    },
                );
                m
            },
        };

        let resolved = config.resolve_all();
        assert_eq!(resolved["work"].name, "Work User");
        assert_eq!(resolved["work"].email, "work@example.com");
        assert_eq!(resolved["personal"].name, "Default");
        assert_eq!(resolved["personal"].email, "default@example.com");
        assert_eq!(resolved["personal"].views.len(), 1);
    }

    #[test]
    fn resolve_profile_with_smtp() {
        let data = ProfileData {
            name: None,
            email: None,
            maildir: "/tmp/work".to_string(),
            views: vec![],
            smtp: Some(SmtpConfig {
                host: "smtp.gmail.com".to_string(),
                port: 587,
                username: "me@gmail.com".to_string(),
                password: "secret".to_string(),
                starttls: Some(true),
            }),
        };
        let resolved = data.resolve("work", None);
        assert!(resolved.smtp.is_some());
        let smtp = resolved.smtp.as_ref().unwrap();
        assert_eq!(smtp.host, "smtp.gmail.com");
        assert_eq!(smtp.username, "me@gmail.com");
        assert_eq!(smtp.starttls, Some(true));
    }

    #[test]
    fn resolve_profile_without_smtp() {
        let data = ProfileData {
            name: None,
            email: None,
            maildir: "/tmp/work".to_string(),
            views: vec![],
            smtp: None,
        };
        let resolved = data.resolve("work", None);
        assert!(resolved.smtp.is_none());
    }
}