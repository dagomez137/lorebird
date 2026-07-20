//! Compose window — email editor with header fields and SourceView body.
//!
//! Opens a secondary GTK window with editable From/To/Cc/Bcc/Subject
//! entries and a SourceView body editor pre-filled from a `Mail`.
//! The Send button dispatches `on_send` via the Lua thread.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{
    ApplicationWindow, Box, Button, Entry, HeaderBar, Label, Orientation, ScrolledWindow,
    Separator, Spinner,
};
use sourceview5 as sv;
use sourceview5::prelude::*;

use lorebird_core::compose::Mail;
use lorebird_lua::EditorConfig;

use crate::app_state::AppState;
use crate::lua_thread::LuaCommand;
use crate::lua_thread::LuaResult;
use crate::thread_node::ThreadNode;

/// Unique-suffix counter for compose temp files handed to the external editor.
static EDITOR_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Data passed from the reply trigger to the compose window ────────

/// Everything the compose window needs to open and send.
pub struct ComposeContext {
    /// The profile label (for on_send).
    pub profile_label: String,
    /// The pre-filled mail data (possibly modified by on_reply).
    pub mail: Mail,
    /// Whether dark theme is active (for SourceView scheme).
    pub is_dark: bool,
}

// ── Public entry point ─────────────────────────────────────────────

/// Open a compose window with the given context.
///
/// The window is a separate `ApplicationWindow` so the user can
/// still interact with the main window while composing.
pub fn open_compose_window(app: &gtk4::Application, state: &Rc<RefCell<AppState>>, ctx: ComposeContext) {
    let is_dark = ctx.is_dark;
    let profile_label = ctx.profile_label.clone();
    let mail = ctx.mail;
    let editor_cfg = state.borrow().editor.clone();

    // ── Window ───────────────────────────────────────────────────
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Compose")
        .default_width(800)
        .default_height(600)
        .build();

    // ── Header bar ──────────────────────────────────────────────
    let header = HeaderBar::new();

    let send_btn = gtk4::Button::with_label("Send");
    send_btn.add_css_class("suggested-action");
    send_btn.set_tooltip_text(Some("Send this message"));

    let save_draft_btn = gtk4::Button::with_label("Save Draft");
    save_draft_btn.set_tooltip_text(Some("Save this message as a draft"));

    let discard_btn = gtk4::Button::with_label("Discard");
    discard_btn.add_css_class("destructive-action");
    discard_btn.set_tooltip_text(Some("Discard this message"));

    // Only shown when an external editor is configured.
    let edit_btn = gtk4::Button::with_label("Edit body");
    edit_btn.set_tooltip_text(Some("Edit the message body in your external editor"));

    let spinner = Spinner::new();
    spinner.set_spinning(false);

    let status_label = Label::new(Some(""));
    status_label.add_css_class("dim-label");
    status_label.add_css_class("caption");

    // pack_end stacks right-to-left, so order reads Send | Save Draft | Discard.
    header.pack_end(&discard_btn);
    header.pack_end(&save_draft_btn);
    header.pack_end(&send_btn);
    if editor_cfg.is_some() {
        header.pack_end(&edit_btn);
    }
    header.pack_end(&spinner);
    window.set_titlebar(Some(&header));

    // ── Main layout ─────────────────────────────────────────────
    let vbox = Box::new(Orientation::Vertical, 0);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(8);

    // Header fields
    let from_entry = make_header_row(&vbox, "From:", &mail.from);
    let to_entry = make_header_row(&vbox, "To:", &mail.to);
    let cc_entry = make_header_row(&vbox, "Cc:", &mail.cc);
    let bcc_entry = make_header_row(&vbox, "Bcc:", &mail.bcc);
    let subject_entry = make_header_row(&vbox, "Subject:", &mail.subject);

    // Separator
    let sep = Separator::new(Orientation::Horizontal);
    sep.set_margin_top(4);
    sep.set_margin_bottom(4);
    vbox.append(&sep);

    // Body editor (SourceView)
    let body_buffer = sv::Buffer::new(None::<&gtk4::TextTagTable>);
    body_buffer.set_highlight_syntax(true);
    let style_mgr = sv::StyleSchemeManager::default();
    let scheme_name = if is_dark { "Adwaita-dark" } else { "kate" };
    let fallback_name = if is_dark { "oblivion" } else { "Adwaita" };
    if let Some(scheme) = style_mgr.scheme(scheme_name)
        .or_else(|| style_mgr.scheme(fallback_name))
    {
        body_buffer.set_style_scheme(Some(&scheme));
    }
    // Use "diff" language for syntax highlighting (good for patch emails)
    let lm = sv::LanguageManager::default();
    if let Some(lang) = lm.language("diff") {
        body_buffer.set_language(Some(&lang));
    }
    body_buffer.set_text(&mail.body_text);

    let body_view = sv::View::with_buffer(&body_buffer);
    body_view.set_editable(true);
    body_view.set_cursor_visible(true);
    body_view.set_wrap_mode(gtk4::WrapMode::WordChar);
    body_view.set_left_margin(4);
    body_view.set_right_margin(4);
    body_view.set_top_margin(4);
    body_view.set_bottom_margin(4);
    body_view.set_show_line_numbers(true);
    body_view.set_monospace(true);

    let scrolled = ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_child(Some(&body_view));
    vbox.append(&scrolled);

    // Status bar
    let status_bar = Box::new(Orientation::Horizontal, 8);
    status_bar.set_margin_top(4);
    status_bar.append(&status_label);
    vbox.append(&status_bar);

    window.set_child(Some(&vbox));

    // Focus the body editor and place the cursor at the end.
    body_view.grab_focus();
    let end_iter = body_buffer.end_iter();
    body_buffer.place_cursor(&end_iter);
    // Scroll after the window is laid out (scroll_to_mark has no effect before realization).
    let scroll_buffer = body_buffer.clone();
    let scroll_view = body_view.clone();
    glib::idle_add_local_once(move || {
        let mark = scroll_buffer.get_insert();
        scroll_view.scroll_to_mark(&mark, 0.0, true, 0.0, 1.0);
    });

    // ── External editor (body only) ─────────────────────────────
    // Round-trip the body through the user's editor. Headers stay in the UI.
    if let Some(cfg) = editor_cfg.clone() {
        let ui = EditorUi {
            body_buffer: body_buffer.clone(),
            body_view: body_view.clone(),
            status: status_label.clone(),
            send: send_btn.clone(),
            save: save_draft_btn.clone(),
            discard: discard_btn.clone(),
            edit: edit_btn.clone(),
            editing: Rc::new(Cell::new(false)),
            tmp: Rc::new(RefCell::new(None)),
        };

        let ui_click = ui.clone();
        let cfg_click = cfg.clone();
        edit_btn.connect_clicked(move |_btn| launch_external_editor(&ui_click, &cfg_click));

        // Delete a leftover temp file if the window closes while editing.
        let tmp_close = ui.tmp.clone();
        window.connect_close_request(move |_win| {
            if let Some(p) = tmp_close.borrow_mut().take() {
                let _ = std::fs::remove_file(p);
            }
            glib::Propagation::Proceed
        });

        // Auto-launch once the window is laid out, if configured.
        if cfg.on_open {
            let ui_open = ui.clone();
            glib::idle_add_local_once(move || launch_external_editor(&ui_open, &cfg));
        }
    }

    // ── Save Draft button handler ───────────────────────────────
    // Clone the field widgets first; the Send closure moves its copies.
    let save_from = from_entry.clone();
    let save_to = to_entry.clone();
    let save_cc = cc_entry.clone();
    let save_bcc = bcc_entry.clone();
    let save_subject = subject_entry.clone();
    let save_buffer = body_buffer.clone();
    let save_state = state.clone();
    let save_profile = profile_label.clone();
    let save_status = status_label.clone();
    let save_mail = mail.clone();
    let save_window = window.clone();
    save_draft_btn.connect_clicked(move |_btn| {
        let body_start = save_buffer.start_iter();
        let body_end = save_buffer.end_iter();
        let body_text = save_buffer.text(&body_start, &body_end, false).to_string();

        let final_mail = Mail {
            from: save_from.text().to_string(),
            to: save_to.text().to_string(),
            cc: save_cc.text().to_string(),
            bcc: save_bcc.text().to_string(),
            subject: save_subject.text().to_string(),
            date: save_mail.date.clone(),
            message_id: save_mail.message_id.clone(),
            in_reply_to: save_mail.in_reply_to.clone(),
            references: save_mail.references.clone(),
            body_text,
            headers: save_mail.headers.clone(),
        };

        let maildir = {
            let s = save_state.borrow();
            s.profiles.get(&save_profile).map(|p| p.maildir.clone())
        };
        let Some(maildir) = maildir else {
            save_status.set_text("Cannot save draft: profile not found");
            save_status.remove_css_class("dim-label");
            save_status.add_css_class("error");
            return;
        };

        let raw = final_mail.to_rfc2822();
        let id = final_mail.draft_id();
        match lorebird_core::maildir::save_draft(&maildir.join("Drafts"), &id, raw.as_bytes()) {
            Ok(_) => {
                save_status.set_text("Draft saved");
                save_status.remove_css_class("error");
                save_status.add_css_class("dim-label");
                save_window.close();
            }
            Err(e) => {
                save_status.set_text(&format!("Draft save failed: {}", e));
                save_status.remove_css_class("dim-label");
                save_status.add_css_class("error");
            }
        }
    });

    // ── Send button handler ─────────────────────────────────────
    let send_state = state.clone();
    let send_profile = profile_label.clone();
    let send_window = window.clone();
    let send_spinner = spinner.clone();
    let send_status = status_label.clone();
    let send_btn_ref = send_btn.clone();

    send_btn.connect_clicked(move |_btn| {
        // Collect field values from the entries
        let from = from_entry.text().to_string();
        let to = to_entry.text().to_string();
        let cc = cc_entry.text().to_string();
        let bcc = bcc_entry.text().to_string();
        let subject = subject_entry.text().to_string();

        let body_start = body_buffer.start_iter();
        let body_end = body_buffer.end_iter();
        let body_text = body_buffer.text(&body_start, &body_end, false).to_string();

        let final_mail = Mail {
            from,
            to,
            cc,
            bcc,
            subject,
            date: mail.date.clone(),
            message_id: mail.message_id.clone(),
            in_reply_to: mail.in_reply_to.clone(),
            references: mail.references.clone(),
            body_text,
            headers: mail.headers.clone(),
        };

        let profile = send_profile.clone();

        // on_send hook is required for sending
        let s = send_state.borrow();
        if !s.has_on_send {
            send_status.set_text("Cannot send: on_send hook not defined");
            send_status.remove_css_class("dim-label");
            send_status.add_css_class("error");
            return;
        }

        send_btn_ref.set_sensitive(false);
        send_spinner.set_spinning(true);
        send_status.set_text("Sending…");
        send_status.remove_css_class("error");
        send_status.add_css_class("dim-label");

        match s.lua_thread.send(LuaCommand::Send {
            profile_label: profile,
            mail: final_mail,
        }) {
            Ok(()) => {}
            Err(e) => {
                send_btn_ref.set_sensitive(true);
                send_spinner.set_spinning(false);
                send_status.set_text(&format!("Send error: {}", e));
                send_status.remove_css_class("dim-label");
                send_status.add_css_class("error");
                return;
            }
        }

        // Poll for the result
        let poll_state = send_state.clone();
        let poll_spinner = spinner.clone();
        let poll_status = status_label.clone();
        let poll_btn = send_btn_ref.clone();
        let poll_window = send_window.clone();
        // If this compose was for a draft, clean it up after successful send.
        let draft_id = mail.draft_id();
        let drafts_dir = {
            let s = send_state.borrow();
            s.profiles.get(&send_profile).map(|p| p.maildir.join("Drafts"))
        };
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let s = poll_state.borrow();
            match s.poll_fetch_result() {
                Some(LuaResult::SendDone { error }) => {
                    poll_spinner.set_spinning(false);
                    poll_btn.set_sensitive(true);
                    if let Some(e) = error {
                        poll_status.set_text(&format!("Send failed: {}", e));
                        poll_status.remove_css_class("dim-label");
                        poll_status.add_css_class("error");
                    } else {
                        poll_status.set_text("Message sent successfully");
                        poll_status.remove_css_class("error");
                        poll_status.add_css_class("dim-label");
                        // Remove the draft if this was composed from one.
                        if let Some(ref dir) = drafts_dir {
                            let _ = lorebird_core::maildir::delete_draft(dir, &draft_id);
                        }
                        // Also remove it from the in-memory model.
                        {
                            let s = poll_state.borrow();
                            for i in (0..s.root_model.n_items()).rev() {
                                if let Some(item) = s.root_model.item(i).and_downcast::<ThreadNode>() {
                                    if item.message_id() == draft_id {
                                        s.root_model.remove(i);
                                        break;
                                    }
                                }
                            }
                        }
                        // Close the compose window after successful send
                        poll_window.close();
                    }
                    glib::ControlFlow::Break
                }
                Some(_) => {
                    // Unexpected result, keep polling
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Continue,
            }
        });
    });

    // ── Discard button handler ──────────────────────────────────
    let discard_window = window.clone();
    discard_btn.connect_clicked(move |_btn| {
        discard_window.close();
    });

    window.present();
}

// ── External editor ─────────────────────────────────────────────────

/// Widgets the external-editor round-trip touches, bundled so the launch
/// helper and its async callback can share one handle.
#[derive(Clone)]
struct EditorUi {
    body_buffer: sv::Buffer,
    body_view: sv::View,
    status: Label,
    send: Button,
    save: Button,
    discard: Button,
    edit: Button,
    /// Guards against launching a second editor while one is open.
    editing: Rc<Cell<bool>>,
    /// The temp file currently being edited, for cleanup on window close.
    tmp: Rc<RefCell<Option<PathBuf>>>,
}

impl EditorUi {
    /// While the editor is open the body is read-only and the actions are
    /// disabled, so the two buffers cannot diverge.
    fn set_busy(&self, busy: bool) {
        self.body_view.set_editable(!busy);
        for b in [&self.send, &self.save, &self.discard, &self.edit] {
            b.set_sensitive(!busy);
        }
    }

    fn set_error(&self, msg: &str) {
        self.status.set_text(msg);
        self.status.remove_css_class("dim-label");
        self.status.add_css_class("error");
    }
}

/// Write the body to a temp file, spawn the configured editor command on it,
/// and read the file back into the body buffer when the editor exits. Only the
/// body is edited; headers stay in the UI.
fn launch_external_editor(ui: &EditorUi, cfg: &EditorConfig) {
    if ui.editing.get() {
        return;
    }

    let start = ui.body_buffer.start_iter();
    let end = ui.body_buffer.end_iter();
    let body = ui.body_buffer.text(&start, &end, false).to_string();

    let path = match write_body_tmpfile(&body, &cfg.file_suffix) {
        Ok(p) => p,
        Err(e) => {
            ui.set_error(&format!("Editor temp file failed: {}", e));
            return;
        }
    };

    let argv = build_argv(&cfg.command, &path);
    let argv_os: Vec<OsString> = argv.iter().map(OsString::from).collect();
    let argv_ref: Vec<&OsStr> = argv_os.iter().map(OsString::as_os_str).collect();
    let proc = match gtk4::gio::Subprocess::newv(&argv_ref, gtk4::gio::SubprocessFlags::NONE) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            ui.set_error(&format!("Cannot launch editor: {}", e));
            return;
        }
    };

    ui.editing.set(true);
    *ui.tmp.borrow_mut() = Some(path.clone());
    ui.set_busy(true);
    ui.status.remove_css_class("error");
    ui.status.add_css_class("dim-label");
    ui.status.set_text("Editing in external editor\u{2026}");

    let ui = ui.clone();
    // Keep the subprocess alive until the wait completes; ignore its exit code
    // and read the file back regardless (the user may have written then quit
    // with a non-zero status).
    let keep = proc.clone();
    proc.wait_async(None::<&gtk4::gio::Cancellable>, move |_res| {
        let _ = &keep;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                ui.body_buffer.set_text(&text);
                ui.status.set_text("");
            }
            Err(e) => ui.set_error(&format!("Could not read edited body: {}", e)),
        }
        let _ = std::fs::remove_file(&path);
        *ui.tmp.borrow_mut() = None;
        ui.editing.set(false);
        ui.set_busy(false);
    });
}

/// Expand a command template into an argv, substituting `{file}` with the temp
/// file path. If no element contains the placeholder the path is appended.
fn build_argv(command: &[String], file: &std::path::Path) -> Vec<String> {
    let file_str = file.to_string_lossy();
    let mut used = false;
    let mut argv: Vec<String> = command
        .iter()
        .map(|a| {
            if a.contains("{file}") {
                used = true;
                a.replace("{file}", &file_str)
            } else {
                a.clone()
            }
        })
        .collect();
    if !used {
        argv.push(file_str.into_owned());
    }
    argv
}

/// Write the body to a fresh temp file and return its path.
///
/// Tries the system temp dir first, creating it if missing: under a `nix
/// develop` shell `$TMPDIR` points at a per-shell directory that may already
/// be gone, so a bare write there fails with ENOENT. Falls back to the app
/// config dir, which always exists, so the feature works regardless of how the
/// process was launched.
fn write_body_tmpfile(body: &str, suffix: &str) -> std::io::Result<PathBuf> {
    let n = EDITOR_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("lorebird-compose-{}-{}{}", std::process::id(), n, suffix);

    let mut candidates: Vec<PathBuf> = vec![std::env::temp_dir()];
    if let Some(confdir) = lorebird_core::config_dir::lorebird_confdir() {
        candidates.push(confdir.join("compose-tmp"));
    }

    let mut last_err = None;
    for dir in candidates {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            last_err = Some(e);
            continue;
        }
        let path = dir.join(&name);
        match std::fs::write(&path, body) {
            Ok(()) => return Ok(path),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| std::io::Error::other("no writable temp directory")))
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Create a labelled header row (e.g. "From: [...]") and return the entry.
fn make_header_row(parent: &Box, label: &str, value: &str) -> Entry {
    let hbox = Box::new(Orientation::Horizontal, 8);
    hbox.set_margin_top(2);
    hbox.set_margin_bottom(2);

    let lbl = Label::new(Some(label));
    lbl.set_width_chars(8);
    lbl.set_xalign(1.0);
    lbl.add_css_class("dim-label");
    hbox.append(&lbl);

    let entry = Entry::new();
    entry.set_hexpand(true);
    entry.set_text(value);
    if label == "Subject:" {
        entry.add_css_class("heading");
    }
    hbox.append(&entry);

    parent.append(&hbox);
    entry
}

#[cfg(test)]
mod tests {
    use super::build_argv;
    use std::path::Path;

    #[test]
    fn build_argv_substitutes_placeholder() {
        let cmd = vec![
            "alacritty".to_string(),
            "--command".to_string(),
            "hx".to_string(),
            "{file}".to_string(),
        ];
        let argv = build_argv(&cmd, Path::new("/tmp/body.eml"));
        assert_eq!(argv, ["alacritty", "--command", "hx", "/tmp/body.eml"]);
    }

    #[test]
    fn build_argv_appends_when_placeholder_absent() {
        let cmd = vec!["hx".to_string()];
        let argv = build_argv(&cmd, Path::new("/tmp/body.eml"));
        assert_eq!(argv, ["hx", "/tmp/body.eml"]);
    }

    #[test]
    fn build_argv_substitutes_within_argument() {
        let cmd = vec!["wrapper".to_string(), "--edit={file}".to_string()];
        let argv = build_argv(&cmd, Path::new("/tmp/b.eml"));
        assert_eq!(argv, ["wrapper", "--edit=/tmp/b.eml"]);
    }
}