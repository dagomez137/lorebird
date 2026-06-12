//! Compose window — email editor with header fields and SourceView body.
//!
//! Opens a secondary GTK window with editable From/To/Cc/Bcc/Subject
//! entries and a SourceView body editor pre-filled from a `ComposeMail`.
//! The Send button dispatches `on_send` via the Lua thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{
    ApplicationWindow, Box, Entry, HeaderBar, Label, Orientation, ScrolledWindow,
    Separator, Spinner,
};
use sourceview5 as sv;
use sourceview5::prelude::*;

use lorebird_core::compose::ComposeMail;

use crate::app_state::AppState;
use crate::lua_thread::LuaCommand;
use crate::lua_thread::LuaResult;

// ── Data passed from the reply trigger to the compose window ────────

/// Everything the compose window needs to open and send.
pub struct ComposeContext {
    /// The profile label (for on_send).
    pub profile_label: String,
    /// The pre-filled mail data (possibly modified by on_reply).
    pub mail: ComposeMail,
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

    let spinner = Spinner::new();
    spinner.set_spinning(false);

    let status_label = Label::new(Some(""));
    status_label.add_css_class("dim-label");
    status_label.add_css_class("caption");

    // pack_end stacks right-to-left, so order reads Send | Save Draft | Discard.
    header.pack_end(&discard_btn);
    header.pack_end(&save_draft_btn);
    header.pack_end(&send_btn);
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
    save_draft_btn.connect_clicked(move |_btn| {
        let body_start = save_buffer.start_iter();
        let body_end = save_buffer.end_iter();
        let body_text = save_buffer.text(&body_start, &body_end, false).to_string();

        let final_mail = ComposeMail {
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

        let final_mail = ComposeMail {
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