//! Persistent "followed series" — app-managed state, stored separately from
//! the user's hand-written `config.lua`.
//!
//! Each [`Follow`] is a saved view (label + query) the user subscribed to via
//! the UI, optionally folded into the inbox. Stored as JSON at
//! `<config dir>/lorebird/follows.json`.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config_dir;

/// A followed series: a labelled saved query, optionally merged into the inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Follow {
    /// Display label shown in the sidebar.
    pub label: String,
    /// The view query (e.g. `subject:"nvme updates for Linux"`).
    pub query: String,
    /// When true, this series is OR-ed into the inbox view.
    #[serde(default = "default_true")]
    pub in_inbox: bool,
}

fn default_true() -> bool {
    true
}

/// Path to the follows file, if a config dir can be resolved.
pub fn follows_path() -> Option<PathBuf> {
    config_dir::lorebird_confdir().map(|d| d.join("follows.json"))
}

/// Load the followed series. Returns an empty list if the file is missing or
/// unreadable/unparseable (follows are non-critical state).
pub fn load() -> Vec<Follow> {
    let Some(path) = follows_path() else {
        return Vec::new();
    };
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Persist the followed series, creating the config dir if needed.
pub fn save(follows: &[Follow]) -> io::Result<()> {
    let path = follows_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config dir"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(follows)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let follows = vec![Follow {
            label: "nvme updates for Linux".into(),
            query: "subject:\"nvme updates for Linux\"".into(),
            in_inbox: true,
        }];
        let json = serde_json::to_vec(&follows).unwrap();
        let back: Vec<Follow> = serde_json::from_slice(&json).unwrap();
        assert_eq!(follows, back);
    }

    #[test]
    fn in_inbox_defaults_true() {
        let json = br#"[{"label":"x","query":"subject:\"x\""}]"#;
        let back: Vec<Follow> = serde_json::from_slice(json).unwrap();
        assert!(back[0].in_inbox);
    }
}
