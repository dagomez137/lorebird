//! Minimal maildir writer for Drafts and Sent backups.
//!
//! Drafts are keyed by a sanitised Message-ID so re-saving overwrites
//! rather than duplicating; Sent messages get unique maildir names.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn ensure_dirs(maildir: &Path) -> io::Result<()> {
    fs::create_dir_all(maildir.join("cur"))?;
    fs::create_dir_all(maildir.join("new"))?;
    fs::create_dir_all(maildir.join("tmp"))?;
    Ok(())
}

fn sanitize_id(id: &str) -> String {
    let id = id.trim();
    let id = id.strip_prefix('<').unwrap_or(id);
    let id = id.strip_suffix('>').unwrap_or(id);
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// True if `name` is the draft file for `base` (exact, or `base` + a maildir flag separator).
fn matches_base(name: &str, base: &str) -> bool {
    name == base || name.starts_with(&format!("{base}:"))
}

fn remove_matching(maildir: &Path, base: &str) -> io::Result<()> {
    for sub in ["new", "cur"] {
        let dir = maildir.join(sub);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && matches_base(name, base)
            {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

pub fn save_draft(maildir: &Path, draft_id: &str, raw: &[u8]) -> io::Result<PathBuf> {
    ensure_dirs(maildir)?;
    let base = sanitize_id(draft_id);
    remove_matching(maildir, &base)?;

    let tmp = maildir.join("tmp").join(&base);
    fs::write(&tmp, raw)?;
    let dest = maildir.join("new").join(&base);
    fs::rename(&tmp, &dest)?;
    Ok(dest)
}

pub fn delete_draft(maildir: &Path, draft_id: &str) -> io::Result<()> {
    let base = sanitize_id(draft_id);
    remove_matching(maildir, &base)
}

pub fn append_sent(maildir: &Path, raw: &[u8]) -> io::Result<PathBuf> {
    ensure_dirs(maildir)?;

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    let name = format!("{secs}.{seq}_{pid}.{host}");

    let tmp = maildir.join("tmp").join(&name);
    fs::write(&tmp, raw)?;
    let dest = maildir.join("cur").join(format!("{name}:2,S"));
    fs::rename(&tmp, &dest)?;
    Ok(dest)
}

pub fn list_drafts(maildir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for sub in ["new", "cur"] {
        let Ok(entries) = fs::read_dir(maildir.join(sub)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((entry.path(), mtime));
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.into_iter().map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_creates_dirs_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("Drafts");
        let path = save_draft(&md, "<id@host>", b"raw mail").unwrap();
        assert!(md.join("cur").is_dir());
        assert!(md.join("new").is_dir());
        assert!(md.join("tmp").is_dir());
        assert!(path.starts_with(md.join("new")));
        assert_eq!(fs::read(&path).unwrap(), b"raw mail");
    }

    #[test]
    fn save_twice_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("Drafts");
        save_draft(&md, "<id@host>", b"v1").unwrap();
        let path = save_draft(&md, "<id@host>", b"v2").unwrap();
        let n = list_drafts(&md).len();
        assert_eq!(n, 1);
        assert_eq!(fs::read(&path).unwrap(), b"v2");
    }

    #[test]
    fn delete_removes_draft() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("Drafts");
        save_draft(&md, "<id@host>", b"x").unwrap();
        delete_draft(&md, "<id@host>").unwrap();
        assert!(list_drafts(&md).is_empty());
        // Idempotent on a missing draft.
        delete_draft(&md, "<id@host>").unwrap();
    }

    #[test]
    fn append_sent_marks_seen() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("Sent");
        let path = append_sent(&md, b"sent mail").unwrap();
        assert!(path.starts_with(md.join("cur")));
        assert!(path.to_string_lossy().ends_with(":2,S"));
        assert_eq!(fs::read(&path).unwrap(), b"sent mail");
    }

    #[test]
    fn list_drafts_returns_saved() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("Drafts");
        assert!(list_drafts(&md).is_empty());
        save_draft(&md, "<a@host>", b"a").unwrap();
        save_draft(&md, "<b@host>", b"b").unwrap();
        assert_eq!(list_drafts(&md).len(), 2);
    }
}
