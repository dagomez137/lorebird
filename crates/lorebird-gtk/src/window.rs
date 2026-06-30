//! Main application window — Thunderbird-style tri-pane layout.
//!
//! The sidebar is built from the loaded config: each profile appears
//! as a header with "All Mail" and its views underneath. Clicking a
//! row sets the active profile (and optionally the view query).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gio::ListStore;
use glib::Object;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box, ColumnView, ColumnViewColumn, CustomSorter, Grid,
    HeaderBar, IconSize, Image, Label, ListBoxRow, ListItem, Ordering, Orientation, Paned,
    PolicyType, ScrolledWindow, SearchEntry, SignalListItemFactory, SingleSelection, SortListModel,
    SortType, Spinner, ToggleButton, TreeExpander, TreeListModel, TreeListRow, WrapMode,
};
use sourceview5 as sv;
use sourceview5::prelude::*;

use crate::app_state::{AppState, PendingDesc};
use crate::compose::{self, ComposeContext};
use crate::folder_item::{FolderItem, FolderKind};
use crate::lua_thread::LuaCommand;
use crate::thread_node::ThreadNode;
use lorebird_core::compose::Mail;
use lorebird_core::follows::Follow;

// ── Public entry point ─────────────────────────────────────────────

/// Build and present the main lorebird window.
pub fn build_window(app: &Application, state: &Rc<RefCell<AppState>>) {
    let state_ref = state.borrow();

    // ── Apply theme (dark/light) ──────────────────────────────
    let is_dark = state_ref.theme == "dark";
    if let Some(settings) = gtk4::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(is_dark);
    }

    // Subtle whole-thread tint, kept distinct from the selection blue (which
    // the one selected row keeps) and the hover grey; lighter on dark.
    let tint = if is_dark {
        "rgba(120, 170, 255, 0.14)"
    } else {
        "rgba(53, 132, 228, 0.12)"
    };
    let css = gtk4::CssProvider::new();
    css.load_from_data(&format!(".thread-active {{ background-color: {tint}; }}"));
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("lorebird")
        .icon_name("org.lorebird.app")
        .default_width(1200)
        .default_height(700)
        .build();

    // ── Apply UI scale ──────────────────────────────────────────
    // Multiply the Xft DPI by the user's scale factor (default 1.0).
    // A value of 1.0 means no change; 2.0 doubles the DPI, etc.
    // Only applied if ui_scale differs from 1.0, so unconfigured
    // environments are left untouched.
    let scale = state_ref.ui_scale;
    if scale != 1.0 {
        let ws = window.settings();
        let dpi = ws.gtk_xft_dpi();
        ws.set_gtk_xft_dpi((scale * dpi as f64) as i32);
    }

    // ── Header bar ────────────────────────────────────────────
    let header = HeaderBar::new();
    let title_label = Label::new(Some("lorebird"));
    title_label.add_css_class("title");
    header.set_title_widget(Some(&title_label));

    let refresh_btn = gtk4::Button::from_icon_name("view-refresh");
    refresh_btn.set_tooltip_text(Some("Refresh mail"));

    let reply_btn = gtk4::Button::with_label("Reply");
    reply_btn.set_tooltip_text(Some("Reply (Ctrl+R)"));
    reply_btn.set_sensitive(false); // greyed out until a message is selected

    // ── Spinner (shown during async fetch) ─────────────────────
    let spinner = Spinner::new();
    spinner.set_spinning(false);

    // ── Status bar (created early so callbacks can clone it) ──
    let status_label = Label::new(Some("Ready \u{2014} select a profile, then Refresh"));
    status_label.set_margin_start(8);
    status_label.set_margin_top(4);
    status_label.set_margin_bottom(4);
    status_label.add_css_class("dim-label");
    status_label.add_css_class("caption");

    header.pack_end(&refresh_btn);
    header.pack_end(&reply_btn);
    header.pack_end(&spinner);
    window.set_titlebar(Some(&header));

    // ── Main vertical box: paned + status ──────────────────────
    let main_vbox = Box::new(Orientation::Vertical, 0);

    // ── Tri-pane: sidebar | center | preview ──────────────────
    let outer_paned = Paned::new(Orientation::Horizontal);
    let inner_paned = Paned::new(Orientation::Horizontal);

    // Sidebar (built from config)
    let (sidebar_scrolled, sidebar_model, sidebar_lb) = build_sidebar(&state_ref);

    // Collapse/show the folder sidebar. Uses the same chevron toggle the
    // To/Cc header expander does (those icons are known to render here).
    let sidebar_toggle = ToggleButton::new();
    sidebar_toggle.set_icon_name("pan-start-symbolic");
    sidebar_toggle.set_tooltip_text(Some("Collapse or show the folder sidebar"));
    sidebar_toggle.add_css_class("flat");
    sidebar_toggle.set_active(true);
    let sidebar_for_toggle = sidebar_scrolled.clone();
    sidebar_toggle.connect_toggled(move |b| {
        let shown = b.is_active();
        sidebar_for_toggle.set_visible(shown);
        b.set_icon_name(if shown {
            "pan-start-symbolic"
        } else {
            "pan-end-symbolic"
        });
    });
    header.pack_start(&sidebar_toggle);

    // Clones of the sidebar model for live follow/unfollow mutation, and the
    // profile that followed-series rows run against (the first, alphabetically).
    let sidebar_model_for_follow = sidebar_model.clone();
    let sidebar_model_for_unfollow = sidebar_model.clone();
    let default_profile = {
        let mut v: Vec<String> = state_ref.profiles.keys().cloned().collect();
        v.sort();
        v.into_iter().next().unwrap_or_default()
    };

    outer_paned.set_start_child(Some(&sidebar_scrolled));
    outer_paned.set_shrink_start_child(false);

    // Center + preview
    let (center, selection, column_view, preview_labels, search_entry, expand_guard) =
        build_center_pane(&state_ref.root_model, is_dark, state_ref.expand_headers);
    inner_paned.set_start_child(Some(&center));
    inner_paned.set_shrink_start_child(false);

    let preview = build_preview_pane(&preview_labels);
    // The reading pane opens at `reading_pane_columns` of monospace text and
    // can be dragged down to a small floor. Both panes have a hard minimum
    // width with shrink disabled, so neither can be squeezed away and the
    // window cannot shrink small enough to overflow.
    let cols = state_ref.reading_pane_columns.clamp(40, 400) as i32;
    let default_px = reading_pane_width_px(&window, cols);
    let min_px = reading_pane_width_px(&window, cols.min(READING_PANE_MIN_COLUMNS));
    preview.set_width_request(min_px);
    inner_paned.set_end_child(Some(&preview));
    inner_paned.set_resize_start_child(true);
    inner_paned.set_resize_end_child(false);
    inner_paned.set_shrink_end_child(false);
    center.set_width_request(THREAD_LIST_MIN_WIDTH);

    outer_paned.set_end_child(Some(&inner_paned));
    outer_paned.set_position(SIDEBAR_WIDTH);
    // Open the split with the reading pane at its full column width and a
    // usable list beside it, and size the window to match.
    inner_paned.set_position(THREAD_LIST_DEFAULT_WIDTH);
    window.set_default_size(SIDEBAR_WIDTH + THREAD_LIST_DEFAULT_WIDTH + default_px, 760);
    outer_paned.set_vexpand(true);
    outer_paned.set_hexpand(true);

    main_vbox.append(&outer_paned);
    main_vbox.append(&status_label);
    window.set_child(Some(&main_vbox));

    // ── Track the currently selected node ────────────
    let selected_node: Rc<RefCell<Option<ThreadNode>>> = Rc::new(RefCell::new(None));

    // ── Track the active folder kind (for context menu sensitivity) ──
    let active_folder_kind: Rc<RefCell<FolderKind>> = Rc::new(RefCell::new(FolderKind::AllMail));

    // Context menu buttons (created early so the sidebar callback can update sensitivity).
    let reply_menu_btn = gtk4::Button::with_label("Reply");
    reply_menu_btn.add_css_class("flat");
    reply_menu_btn.set_margin_top(4);
    reply_menu_btn.set_margin_bottom(4);
    reply_menu_btn.set_margin_start(8);
    reply_menu_btn.set_margin_end(8);
    let edit_draft_btn = gtk4::Button::with_label("Edit Draft");
    edit_draft_btn.add_css_class("flat");
    edit_draft_btn.set_margin_top(4);
    edit_draft_btn.set_margin_bottom(4);
    edit_draft_btn.set_margin_start(8);
    edit_draft_btn.set_margin_end(8);
    let delete_draft_btn = gtk4::Button::with_label("Delete Draft");
    delete_draft_btn.add_css_class("flat");
    delete_draft_btn.set_margin_top(4);
    delete_draft_btn.set_margin_bottom(4);
    delete_draft_btn.set_margin_start(8);
    delete_draft_btn.set_margin_end(8);

    // ── Wire Refresh button (async via Lua thread) ───────────────
    let state_for_refresh = state.clone();
    let status_for_refresh = status_label.clone();
    let spinner_for_refresh = spinner.clone();
    let refresh_btn_ref = refresh_btn.clone();
    refresh_btn.connect_clicked(move |_btn| {
        refresh_btn_ref.set_sensitive(false);
        let s = state_for_refresh.borrow();
        match s.request_fetch() {
            Ok(()) => {
                spinner_for_refresh.set_spinning(true);
                status_for_refresh.set_text("Refreshing\u{2026}");
                let state_poll = state_for_refresh.clone();
                let status_poll = status_for_refresh.clone();
                let spinner_poll = spinner_for_refresh.clone();
                let btn_poll = refresh_btn_ref.clone();
                glib::timeout_add_local(Duration::from_millis(100), move || {
                    let s = state_poll.borrow();
                    match s.poll_fetch_result() {
                        Some(result) => {
                            btn_poll.set_sensitive(true);
                            match s.handle_fetch_result(&result) {
                                Ok(()) => {
                                    // The list rebuild was dispatched to the
                                    // query worker; the persistent query poller
                                    // stops the spinner and sets the final
                                    // status (and scrolls to the top).
                                    status_poll.set_text("Indexing\u{2026}");
                                }
                                Err(e) => {
                                    spinner_poll.set_spinning(false);
                                    status_poll.set_text(&format!("Refresh error: {}", e));
                                }
                            }
                            glib::ControlFlow::Break
                        }
                        None => glib::ControlFlow::Continue,
                    }
                });
            }
            Err(e) => {
                refresh_btn_ref.set_sensitive(true);
                status_for_refresh.set_text(&format!("Refresh error: {}", e));
            }
        }
    });

    // ── Persistent query poller ──────────────────────────────────
    // The background query worker delivers `PlainNode` trees here in
    // batches (newest-first). This poller applies current batches, discards
    // stale ones, and updates the status bar. On the first batch it scrolls
    // to the top so the newest mail is visible immediately; the spinner
    // keeps running until the final batch arrives.
    let state_for_qpoll = state.clone();
    let status_for_qpoll = status_label.clone();
    let spinner_for_qpoll = spinner.clone();
    let column_view_for_qpoll = column_view.clone();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let s = state_for_qpoll.borrow();
        let mut scroll_to_top = false;
        while let Some(result) = s.poll_query_result() {
            if let Some(outcome) = s.apply_query_result(&result) {
                status_for_qpoll.set_text(&outcome.status);
                if outcome.first {
                    scroll_to_top = true;
                }
                if outcome.done {
                    spinner_for_qpoll.set_spinning(false);
                }
            }
        }
        if scroll_to_top {
            let cv = column_view_for_qpoll.clone();
            glib::idle_add_local_once(move || {
                if let Some(adj) = cv.vadjustment() {
                    adj.set_value(adj.lower());
                }
            });
        }
        glib::ControlFlow::Continue
    });

    // ── Wire sidebar selection → profile + view/search ──────
    let state_for_sidebar = state.clone();
    let status_for_sidebar = status_label.clone();
    let search_for_sidebar = search_entry.clone();
    let active_folder_kind_sidebar = active_folder_kind.clone();
    let selected_node_sidebar = selected_node.clone();
    let reply_btn_sidebar = reply_btn.clone();
    let column_view_for_sidebar = column_view.clone();
    let spinner_for_sidebar = spinner.clone();
    let model = sidebar_model;
    sidebar_lb.connect_row_selected(move |_lb, row| {
        let Some(row) = row else { return };
        let idx = row.index() as u32;
        // Retrieve the FolderItem from the sidebar model
        let item: Option<FolderItem> = model.item(idx).and_downcast::<FolderItem>();
        let Some(item) = item else { return };

        let profile = item.profile_label();
        let query = item.query();
        let kind = FolderKind::from_str(&item.row_kind()).unwrap_or(FolderKind::Placeholder);

        // Filter out non-interactive rows
        if matches!(kind, FolderKind::Separator | FolderKind::Placeholder) {
            return;
        }

        *active_folder_kind_sidebar.borrow_mut() = kind;

        // Clear stale selection and update header button when switching folders.
        *selected_node_sidebar.borrow_mut() = None;
        reply_btn_sidebar.set_sensitive(!matches!(kind, FolderKind::Drafts));

        let s = state_for_sidebar.borrow();
        s.set_active_is_inbox(false);

        match kind {
            FolderKind::ProfileHeader => {
                s.select_profile(&profile);
                // Clear any active search
                search_for_sidebar.set_text("");
                status_for_sidebar.set_text(&format!(
                    "Selected profile: {} \u{2014} click Refresh to load",
                    profile
                ));
            }
            FolderKind::AllMail => {
                s.select_profile(&profile);

                // Open existing DB if available
                if s.db.borrow().is_none() {
                    let maildir = s.active_maildir.borrow().clone();
                    if !maildir.as_os_str().is_empty() {
                        let _ = s.open_db(&maildir);
                    }
                }
                if s.db.borrow().is_some() {
                    spinner_for_sidebar.set_spinning(true);
                    status_for_sidebar.set_text("Loading\u{2026}");
                    if let Err(e) = s.request_load_all(PendingDesc::AllMail {
                        profile: profile.to_string(),
                    }) {
                        spinner_for_sidebar.set_spinning(false);
                        status_for_sidebar.set_text(&format!("Error: {}", e));
                    }
                } else {
                    status_for_sidebar
                        .set_text(&format!("No index for {} \u{2014} click Refresh", profile));
                }
                search_for_sidebar.set_text("");
            }
            FolderKind::Drafts => {
                s.select_profile(&profile);
                match s.show_drafts() {
                    Ok(n) => status_for_sidebar
                        .set_text(&format!("Drafts: {} draft(s)", n)),
                    Err(e) => status_for_sidebar.set_text(&format!("Error: {}", e)),
                }
                search_for_sidebar.set_text("");
            }
            FolderKind::View | FolderKind::Follow => {
                s.select_profile(&profile);

                // Open existing DB if available
                if s.db.borrow().is_none() {
                    let maildir = s.active_maildir.borrow().clone();
                    if !maildir.as_os_str().is_empty() {
                        let _ = s.open_db(&maildir);
                    }
                }
                if s.db.borrow().is_none() {
                    status_for_sidebar
                        .set_text(&format!("No index for {} \u{2014} click Refresh", profile));
                    return;
                }

                // The inbox view folds in followed series flagged "add to inbox".
                let is_inbox = item.is_inbox();
                s.set_active_is_inbox(is_inbox);
                let effective = if is_inbox {
                    s.augment_inbox_query(&query)
                } else {
                    query.to_string()
                };

                // Run the (possibly augmented) query
                s.select_view(effective.clone());
                search_for_sidebar.set_text(&query);
                spinner_for_sidebar.set_spinning(true);
                status_for_sidebar.set_text("Searching\u{2026}");
                if let Err(e) = s.request_search(
                    effective,
                    PendingDesc::View {
                        name: item.name().to_string(),
                        profile: profile.to_string(),
                    },
                ) {
                    spinner_for_sidebar.set_spinning(false);
                    status_for_sidebar.set_text(&format!("Search error: {}", e));
                }
            }
            _ => {}
        }

        // Drafts repopulate synchronously here, so scroll to the top
        // directly.  All-mail and views are repopulated asynchronously by
        // the query worker; that path scrolls to the top in the poller.
        if kind.as_str() == "drafts" {
            let cv = column_view_for_sidebar.clone();
            glib::idle_add_local_once(move || {
                if let Some(adj) = cv.vadjustment() {
                    adj.set_value(adj.lower());
                }
            });
        }
    });

    // ── Wire selection → preview ──────────────────────────────
    let pl = preview_labels;
    // Shared so the toggle handler can re-render the same labels the selection
    // handler writes, without re-reading the node.
    let header_full: Rc<RefCell<(String, String, String)>> =
        Rc::new(RefCell::new((String::new(), String::new(), String::new())));
    let toggle_from = pl.from_label.clone();
    let toggle_to = pl.to_label.clone();
    let toggle_cc = pl.cc_label.clone();
    let toggle_btn = pl.expand_toggle.clone();
    let header_full_sel = header_full.clone();
    let header_full_tog = header_full.clone();
    let state_for_preview = state.clone();
    selection.connect_selection_changed(move |sel, _pos, _n| {
        if let Some(obj) = sel.selected_item()
            && let Some(row) = obj.downcast_ref::<TreeListRow>()
            && let Some(node) = row.item().and_downcast::<ThreadNode>()
        {
            // Lazily read the rich fields (To/Cc/body/In-Reply-To) from disk
            // the first time a message is previewed, then cache them on the
            // node so re-selection is instant.  The thread list itself is
            // built from the index alone, with no per-message disk reads.
            if node.body_preview().is_empty() && !node.filename().is_empty() {
                let maildir = state_for_preview.borrow().active_maildir.borrow().clone();
                if let Some(m) =
                    lorebird_core::store::read_raw_message(&maildir, &node.filename())
                {
                    node.set_to_addrs(m.to_addr.unwrap_or_default());
                    node.set_cc_addrs(m.cc_addr.unwrap_or_default());
                    node.set_in_reply_to(m.in_reply_to.unwrap_or_default());
                    if let Some(b) = m.body_text {
                        node.set_body_preview(b);
                    }
                }
            }

            let from_full = node.sender();
            let to_full = node.to_addrs();
            let cc_full = node.cc_addrs();
            let expanded = pl.expand_toggle.is_active();
            apply_header_field(&pl.from_label, &from_full, expanded);
            apply_header_field(&pl.to_label, &to_full, expanded);
            apply_header_field(&pl.cc_label, &cc_full, expanded);
            // Offer the toggle only when something is actually clipped.
            let has_long = from_full.len() > HEADER_TRUNCATE_MAX
                || to_full.len() > HEADER_TRUNCATE_MAX
                || cc_full.len() > HEADER_TRUNCATE_MAX;
            pl.expand_toggle.set_visible(has_long);
            *header_full_sel.borrow_mut() = (from_full, to_full, cc_full);
            pl.subject_label.set_text(&node.subject());
            pl.date_label.set_text(&node.last_reply());

            let body = node.body_preview();
            if body.is_empty() {
                set_body_with_highlight(&pl.body_buffer, "(no preview available)");
            } else {
                set_body_with_highlight(&pl.body_buffer, &body);
            }
            return;
        }
        pl.from_label.set_text("");
        pl.to_label.set_text("");
        pl.to_label.set_tooltip_text(None);
        pl.cc_label.set_text("");
        pl.cc_label.set_tooltip_text(None);
        pl.expand_toggle.set_visible(false);
        pl.subject_label.set_text("");
        pl.date_label.set_text("");
        *header_full_sel.borrow_mut() = (String::new(), String::new(), String::new());
        set_body_with_highlight(&pl.body_buffer, "");
    });

    toggle_btn.connect_toggled(move |btn| {
        let expanded = btn.is_active();
        btn.set_icon_name(header_toggle_icon(expanded));
        let full = header_full_tog.borrow();
        apply_header_field(&toggle_from, &full.0, expanded);
        apply_header_field(&toggle_to, &full.1, expanded);
        apply_header_field(&toggle_cc, &full.2, expanded);
    });

    // ── Track the currently selected node for Reply ────────────
    let selected_node_clone = selected_node.clone();
    let reply_btn_ref = reply_btn.clone();
    let kind_ref = active_folder_kind.clone();
    selection.connect_selection_changed(move |sel, _pos, _n| {
        if let Some(obj) = sel.selected_item()
            && let Some(row) = obj.downcast_ref::<TreeListRow>()
            && let Some(node) = row.item().and_downcast::<ThreadNode>()
        {
            *selected_node_clone.borrow_mut() = Some(node);
            // Reply is only sensitive when viewing mail, not drafts.
            reply_btn_ref
                .set_sensitive(!matches!(*kind_ref.borrow(), FolderKind::Drafts));
        } else {
            *selected_node_clone.borrow_mut() = None;
            reply_btn_ref.set_sensitive(false);
        }
    });

    // ── Whole-thread tint + accordion expansion ────────────────────
    let active_thread_root: Rc<RefCell<Option<ThreadNode>>> = Rc::new(RefCell::new(None));
    let expanded_root: Rc<RefCell<Option<TreeListRow>>> = Rc::new(RefCell::new(None));
    let in_select_handler: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let guard_for_select = expand_guard.clone();
    selection.connect_selection_changed(move |sel, _pos, _n| {
        // Collapsing rows can shift the selected position and re-emit this
        // signal; ignore the re-entry so our mutations stay coherent.
        if in_select_handler.get() {
            return;
        }
        in_select_handler.set(true);

        let selected_row = sel.selected_item().and_downcast::<TreeListRow>();

        // Tint the whole conversation.
        let new_root_node = selected_row
            .as_ref()
            .map(root_row_of)
            .and_then(|r| r.item().and_downcast::<ThreadNode>());
        {
            let mut cur = active_thread_root.borrow_mut();
            if *cur != new_root_node {
                if let Some(old) = cur.as_ref() {
                    set_subtree_active(old, false);
                }
                if let Some(new) = new_root_node.as_ref() {
                    set_subtree_active(new, true);
                }
                *cur = new_root_node;
            }
        }

        // Accordion: expand the clicked top-level thread, collapse the last.
        if let Some(row) = selected_row.as_ref()
            && row.parent().is_none()
            && row.is_expandable()
        {
            let already = expanded_root.borrow().as_ref() == Some(row);
            if !already {
                guard_for_select.set(true);
                if let Some(old) = expanded_root.borrow_mut().take() {
                    old.set_expanded(false);
                }
                expand_recursive(row);
                *expanded_root.borrow_mut() = Some(row.clone());
                guard_for_select.set(false);
            }
        }

        in_select_handler.set(false);
    });

    // ── Wire search bar ──────────────────────────────────────────
    // Enter / activate → run search query
    let state_for_search = state.clone();
    let status_for_search = status_label.clone();
    let spinner_for_search = spinner.clone();
    search_entry.connect_activate(move |entry| {
        let query = entry.text().to_string();
        let s = state_for_search.borrow();
        spinner_for_search.set_spinning(true);
        let dispatch = if query.is_empty() {
            status_for_search.set_text("Loading\u{2026}");
            s.request_load_all(PendingDesc::ShowAll)
        } else {
            status_for_search.set_text("Searching\u{2026}");
            s.request_search(query, PendingDesc::Search)
        };
        if let Err(e) = dispatch {
            spinner_for_search.set_spinning(false);
            status_for_search.set_text(&format!("Search error: {}", e));
        }
    });

    // Escape / stop-search → clear search, show all
    let state_for_clear = state.clone();
    let status_for_clear = status_label.clone();
    let spinner_for_clear = spinner.clone();
    search_entry.connect_stop_search(move |entry| {
        entry.set_text("");
        let s = state_for_clear.borrow();
        spinner_for_clear.set_spinning(true);
        status_for_clear.set_text("Loading\u{2026}");
        if let Err(e) = s.request_load_all(PendingDesc::ShowAll) {
            spinner_for_clear.set_spinning(false);
            status_for_clear.set_text(&format!("Error: {}", e));
        }
    });

    // ── Context menu (right-click on thread list) ─────────────────
    let context_menu = gtk4::Popover::new();
    let menu_box = Box::new(Orientation::Vertical, 0);
    context_menu.set_parent(&column_view);
    // Follow / Archive / Unarchive buttons (reply / edit-draft / delete-draft
    // are created near the top of the function so the sidebar callback can
    // toggle their sensitivity).
    let follow_menu_btn = gtk4::Button::with_label("Follow series\u{2026}");
    follow_menu_btn.add_css_class("flat");
    follow_menu_btn.set_margin_top(4);
    follow_menu_btn.set_margin_bottom(4);
    follow_menu_btn.set_margin_start(8);
    follow_menu_btn.set_margin_end(8);
    let archive_menu_btn = gtk4::Button::with_label("Archive series");
    archive_menu_btn.add_css_class("flat");
    archive_menu_btn.set_margin_top(4);
    archive_menu_btn.set_margin_bottom(4);
    archive_menu_btn.set_margin_start(8);
    archive_menu_btn.set_margin_end(8);
    let unarchive_menu_btn = gtk4::Button::with_label("Unarchive series");
    unarchive_menu_btn.add_css_class("flat");
    unarchive_menu_btn.set_margin_top(4);
    unarchive_menu_btn.set_margin_bottom(4);
    unarchive_menu_btn.set_margin_start(8);
    unarchive_menu_btn.set_margin_end(8);
    let archive_separator = gtk4::Separator::new(Orientation::Horizontal);
    menu_box.append(&reply_menu_btn);
    menu_box.append(&edit_draft_btn);
    menu_box.append(&delete_draft_btn);
    menu_box.append(&follow_menu_btn);
    menu_box.append(&archive_separator);
    menu_box.append(&archive_menu_btn);
    menu_box.append(&unarchive_menu_btn);
    context_menu.set_child(Some(&menu_box));

    let state_for_ctx = state.clone();
    let selected_for_ctx = selected_node.clone();
    let kind_for_ctx = active_folder_kind.clone();
    let status_for_ctx = status_label.clone();
    let app_for_ctx = app.clone();
    let context_menu_for_btn = context_menu.clone();
    reply_menu_btn.connect_clicked(move |_btn| {
        context_menu_for_btn.popdown();
        trigger_reply(
            &state_for_ctx,
            &selected_for_ctx,
            &kind_for_ctx,
            &app_for_ctx,
            &status_for_ctx,
            is_dark,
        );
    });

    let state_for_edit = state.clone();
    let selected_for_edit = selected_node.clone();
    let folder_kind_for_edit = active_folder_kind.clone();
    let status_for_edit = status_label.clone();
    let app_for_edit = app.clone();
    let context_menu_for_edit = context_menu.clone();
    edit_draft_btn.connect_clicked(move |_btn| {
        context_menu_for_edit.popdown();
        trigger_edit_draft(
            &state_for_edit,
            &selected_for_edit,
            &folder_kind_for_edit,
            &app_for_edit,
            &status_for_edit,
            is_dark,
        );
    });

    let state_for_del = state.clone();
    let selected_for_del = selected_node.clone();
    let folder_kind_for_del = active_folder_kind.clone();
    let status_for_del = status_label.clone();
    let context_menu_for_del = context_menu.clone();
    delete_draft_btn.connect_clicked(move |_btn| {
        // Force-destroy the popover surface — model changes prevent
        // popdown() from closing it.  It will be re-parented on the
        // next right-click (see gesture handler).
        context_menu_for_del.unparent();
        trigger_delete_draft(
            &state_for_del,
            &selected_for_del,
            &folder_kind_for_del,
            &status_for_del,
        );
    });

    // "Follow series" → confirm dialog, then persist + add sidebar row.
    let state_for_follow = state.clone();
    let selected_for_follow = selected_node.clone();
    let status_for_follow = status_label.clone();
    let window_for_follow = window.clone();
    let sbmodel_for_follow_btn = sidebar_model_for_follow.clone();
    let defprofile_for_follow = default_profile.clone();
    let spinner_for_follow = spinner.clone();
    let context_menu_for_follow = context_menu.clone();
    follow_menu_btn.connect_clicked(move |_btn| {
        context_menu_for_follow.popdown();
        let subject = selected_for_follow.borrow().as_ref().map(|n| n.subject());
        match subject {
            Some(subject) if !subject.is_empty() => open_follow_dialog(
                &window_for_follow,
                &state_for_follow,
                &sbmodel_for_follow_btn,
                &defprofile_for_follow,
                &spinner_for_follow,
                &status_for_follow,
                &subject,
            ),
            _ => status_for_follow.set_text("Select a thread to follow its series"),
        }
    });

    // "Archive series" → hide the whole series from filtered views (still in
    // All Mail), then refresh the current list.
    let state_for_archive = state.clone();
    let selected_for_archive = selected_node.clone();
    let status_for_archive = status_label.clone();
    let spinner_for_archive = spinner.clone();
    let context_menu_for_archive = context_menu.clone();
    archive_menu_btn.connect_clicked(move |_btn| {
        context_menu_for_archive.popdown();
        let subject = selected_for_archive.borrow().as_ref().map(|n| n.subject());
        let Some(subject) = subject.filter(|s| !s.is_empty()) else {
            status_for_archive.set_text("Select a thread to archive its series");
            return;
        };
        let s = state_for_archive.borrow();
        match s.archive_series(&subject) {
            Ok(n) => {
                status_for_archive.set_text(&format!("Archived {} message(s)", n));
                spinner_for_archive.set_spinning(true);
                if let Err(e) = s.rerun_active_view() {
                    spinner_for_archive.set_spinning(false);
                    status_for_archive.set_text(&format!("Archive refresh failed: {}", e));
                }
            }
            Err(e) => status_for_archive.set_text(&format!("Archive failed: {}", e)),
        }
    });

    // "Unarchive series" → bring the series back into filtered views.
    let state_for_unarchive = state.clone();
    let selected_for_unarchive = selected_node.clone();
    let status_for_unarchive = status_label.clone();
    let spinner_for_unarchive = spinner.clone();
    let context_menu_for_unarchive = context_menu.clone();
    unarchive_menu_btn.connect_clicked(move |_btn| {
        context_menu_for_unarchive.popdown();
        let subject = selected_for_unarchive.borrow().as_ref().map(|n| n.subject());
        let Some(subject) = subject.filter(|s| !s.is_empty()) else {
            status_for_unarchive.set_text("Select a thread to unarchive its series");
            return;
        };
        let s = state_for_unarchive.borrow();
        match s.unarchive_series(&subject) {
            Ok(n) => {
                status_for_unarchive.set_text(&format!("Unarchived {} message(s)", n));
                spinner_for_unarchive.set_spinning(true);
                if let Err(e) = s.rerun_active_view() {
                    spinner_for_unarchive.set_spinning(false);
                    status_for_unarchive.set_text(&format!("Unarchive refresh failed: {}", e));
                }
            }
            Err(e) => status_for_unarchive.set_text(&format!("Unarchive failed: {}", e)),
        }
    });

    // Right-click gesture on the column view
    let ctx_menu_ref = context_menu.clone();
    let column_view_for_gesture = column_view.clone();
    let reply_menu_btn_gesture = reply_menu_btn.clone();
    let edit_draft_btn_gesture = edit_draft_btn.clone();
    let delete_draft_btn_gesture = delete_draft_btn.clone();
    let follow_menu_btn_gesture = follow_menu_btn.clone();
    let archive_menu_btn_gesture = archive_menu_btn.clone();
    let unarchive_menu_btn_gesture = unarchive_menu_btn.clone();
    let archive_sep_gesture = archive_separator.clone();
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
    let selected_for_gesture = selected_node.clone();
    let kind_for_gesture = active_folder_kind.clone();
    gesture.connect_pressed(move |_gesture, _n, x, y| {
        // Don't show context menu if no row is selected.
        if selected_for_gesture.borrow().is_none() {
            return;
        }

        // Show only applicable entries based on folder kind.  Drafts get
        // edit/delete; mail folders get reply plus the follow/archive series
        // actions.
        match *kind_for_gesture.borrow() {
            FolderKind::Drafts => {
                reply_menu_btn_gesture.set_visible(false);
                edit_draft_btn_gesture.set_visible(true);
                delete_draft_btn_gesture.set_visible(true);
                follow_menu_btn_gesture.set_visible(false);
                archive_sep_gesture.set_visible(false);
                archive_menu_btn_gesture.set_visible(false);
                unarchive_menu_btn_gesture.set_visible(false);
            }
            _ => {
                reply_menu_btn_gesture.set_visible(true);
                edit_draft_btn_gesture.set_visible(false);
                delete_draft_btn_gesture.set_visible(false);
                follow_menu_btn_gesture.set_visible(true);
                archive_sep_gesture.set_visible(true);
                archive_menu_btn_gesture.set_visible(true);
                unarchive_menu_btn_gesture.set_visible(true);
            }
        }

        // Re-parent the popover if it was unparented after a draft deletion.
        if ctx_menu_ref.parent().is_none() {
            ctx_menu_ref.set_parent(&column_view_for_gesture);
        }
        let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        ctx_menu_ref.set_pointing_to(Some(&rect));
        ctx_menu_ref.set_has_arrow(false);
        ctx_menu_ref.popup();
    });
    column_view.add_controller(gesture);

    // ── Reply button in header bar ─────────────────────────────────
    let state_for_reply_btn = state.clone();
    let selected_for_reply_btn = selected_node.clone();
    let kind_for_reply_btn = active_folder_kind.clone();
    let status_for_reply_btn = status_label.clone();
    let app_for_reply_btn = app.clone();
    reply_btn.connect_clicked(move |_| {
        trigger_reply(
            &state_for_reply_btn,
            &selected_for_reply_btn,
            &kind_for_reply_btn,
            &app_for_reply_btn,
            &status_for_reply_btn,
            is_dark,
        );
    });

    // ── Unfollow (right-click on a followed sidebar row) ──────────
    let unfollow_popover = gtk4::Popover::new();
    let unfollow_btn = gtk4::Button::with_label("Unfollow");
    unfollow_btn.add_css_class("flat");
    unfollow_btn.set_margin_top(4);
    unfollow_btn.set_margin_bottom(4);
    unfollow_btn.set_margin_start(8);
    unfollow_btn.set_margin_end(8);
    unfollow_popover.set_child(Some(&unfollow_btn));
    unfollow_popover.set_has_arrow(false);
    unfollow_popover.set_parent(&sidebar_lb);

    // The query of the row the popover currently targets.
    let unfollow_target: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let state_for_unfollow = state.clone();
    let target_for_btn = unfollow_target.clone();
    let popover_for_btn = unfollow_popover.clone();
    let sbmodel_for_unfollow_btn = sidebar_model_for_unfollow.clone();
    let defprofile_for_unfollow = default_profile.clone();
    unfollow_btn.connect_clicked(move |_btn| {
        popover_for_btn.popdown();
        if let Some(query) = target_for_btn.borrow_mut().take() {
            let s = state_for_unfollow.borrow();
            s.remove_follow(&query);
            refresh_follow_rows(&sbmodel_for_unfollow_btn, &defprofile_for_unfollow, &s.follows.borrow());
        }
    });

    let sidebar_lb_for_gesture = sidebar_lb.clone();
    let model_for_unfollow = sidebar_model_for_unfollow.clone();
    let target_for_gesture = unfollow_target.clone();
    let popover_for_gesture = unfollow_popover.clone();
    let unfollow_gesture = gtk4::GestureClick::new();
    unfollow_gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
    unfollow_gesture.connect_pressed(move |_g, _n, x, y| {
        let Some(row) = sidebar_lb_for_gesture.row_at_y(y as i32) else { return };
        let idx = row.index() as u32;
        let Some(item) = model_for_unfollow.item(idx).and_downcast::<FolderItem>() else { return };
        if FolderKind::from_str(&item.row_kind()) != Some(FolderKind::Follow) {
            return;
        }
        *target_for_gesture.borrow_mut() = Some(item.query());
        let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover_for_gesture.set_pointing_to(Some(&rect));
        popover_for_gesture.popup();
    });
    sidebar_lb.add_controller(unfollow_gesture);

    // ── Ctrl+R keybind for Reply ───────────────────────────────────
    let state_for_reply = state.clone();
    let selected_for_reply = selected_node.clone();
    let kind_for_reply = active_folder_kind.clone();
    let status_for_reply = status_label.clone();
    let app_for_reply = app.clone();
    let reply_action = gtk4::gio::SimpleAction::new("reply", None);
    reply_action.connect_activate(move |_action, _param| {
        trigger_reply(
            &state_for_reply,
            &selected_for_reply,
            &kind_for_reply,
            &app_for_reply,
            &status_for_reply,
            is_dark,
        );
    });
    app.add_action(&reply_action);
    app.set_accels_for_action("app.reply", &["<Ctrl>R"]);

    // ── Ctrl+S: focus search field ─────────────────────────────────
    let search_ref = search_entry.clone();
    let focus_search = gtk4::gio::SimpleAction::new("focus-search", None);
    focus_search.connect_activate(move |_action, _param| {
        search_ref.grab_focus();
    });
    app.add_action(&focus_search);
    app.set_accels_for_action("app.focus-search", &["<Ctrl>s"]);

    // ── Ctrl+T: focus thread list ──────────────────────────────────
    let cv_ref = column_view.clone();
    let focus_threads = gtk4::gio::SimpleAction::new("focus-threads", None);
    focus_threads.connect_activate(move |_action, _param| {
        cv_ref.grab_focus();
    });
    app.add_action(&focus_threads);
    app.set_accels_for_action("app.focus-threads", &["<Ctrl>t"]);

    // ── Left/Right arrows: expand/collapse thread nodes ────────────
    let sel_for_keys = selection.clone();
    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
    key_ctrl.connect_key_pressed(move |_, keyval, _, _| {
        let pos = sel_for_keys.selected();
        let item = sel_for_keys.model().and_then(|m| m.item(pos));
        let Some(row) = item.and_downcast_ref::<TreeListRow>() else {
            return glib::Propagation::Proceed;
        };
        if !row.is_expandable() {
            return glib::Propagation::Proceed;
        }
        match keyval {
            gtk4::gdk::Key::Right => {
                row.set_expanded(true);
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Left => {
                row.set_expanded(false);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    column_view.add_controller(key_ctrl);

    window.present();
}

// ── Reply action ────────────────────────────────────────────────────

/// Triggered by Ctrl+R or the context menu Reply button.
///
/// Builds a pre-filled `Mail` from the selected message, calls
/// `on_reply` if the hook exists, and opens the compose window.
fn trigger_reply(
    state: &Rc<RefCell<AppState>>,
    selected_node: &Rc<RefCell<Option<ThreadNode>>>,
    active_folder_kind: &Rc<RefCell<FolderKind>>,
    app: &gtk4::Application,
    status_label: &Label,
    is_dark: bool,
) {
    if matches!(*active_folder_kind.borrow(), FolderKind::Drafts) {
        status_label.set_text("Reply is not available in the Drafts folder");
        return;
    }
    let node = match selected_node.borrow().as_ref() {
        Some(n) => n.clone(),
        None => {
            status_label.set_text("No message selected — select a message first");
            return;
        }
    };

    let s = state.borrow();
    let profile_label = s.active_profile.borrow().clone();
    if profile_label.is_empty() {
        status_label.set_text("No profile selected — select a profile first");
        return;
    }
    let profile = match s.profiles.get(&profile_label) {
        Some(p) => p.clone(),
        None => {
            status_label.set_text(&format!("Profile '{}' not found", profile_label));
            return;
        }
    };

    // Build Mail from the selected node, including ALL original
    // headers read from disk so the on_reply hook can inspect any header.
    let filename = node.filename();
    let maildir = s.active_maildir.borrow().clone();
    let headers = if !filename.is_empty() {
        lorebird_core::store::read_raw_headers(&maildir, &filename).unwrap_or_default()
    } else {
        HashMap::new()
    };

    let parent = lorebird_core::compose::Mail {
        from: node.sender(),
        to: node.to_addrs(),
        cc: node.cc_addrs(),
        bcc: String::new(),
        subject: node.subject(),
        date: {
            let d = node.date_str();
            if d.is_empty() { None } else { Some(d) }
        },
        message_id: {
            let mid = node.message_id();
            if mid.is_empty() { None } else { Some(mid) }
        },
        in_reply_to: {
            let irt = node.in_reply_to();
            if irt.is_empty() { None } else { Some(irt) }
        },
        references: {
            let r = node.references_str();
            if r.is_empty() { None } else { Some(r) }
        },
        body_text: node.body_preview(),
        headers,
    };

    // Build pre-filled reply
    let mail = Mail::new_reply(&parent, &profile.name, &profile.email);

    // If on_reply hook exists, dispatch to the Lua thread and poll for result
    if s.has_on_reply {
        status_label.set_text("Calling on_reply hook…");
        match s.lua_thread.send(LuaCommand::Reply {
            profile_label: profile_label.clone(),
            parent: parent.clone(),
            mail: mail.clone(),
        }) {
            Ok(()) => {}
            Err(e) => {
                status_label.set_text(&format!("Reply error: {}", e));
                return;
            }
        }
        drop(s); // release borrow before polling

        // Poll for the reply result (blocking with timeout)
        let state_poll = state.clone();
        let status_poll = status_label.clone();
        let _selected_poll = selected_node.clone();
        let app_poll = app.clone();
        let profile_label_poll = profile_label.clone();
        let _profile_poll = profile.clone();
        let mail_poll = mail.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let s = state_poll.borrow();
            match s.poll_fetch_result() {
                Some(crate::lua_thread::LuaResult::ReplyDone {
                    mail: modified,
                    error,
                }) => {
                    if let Some(e) = error {
                        status_poll.set_text(&format!("on_reply error: {}", e));
                        // Still open compose with default mail
                        let ctx = ComposeContext {
                            profile_label: profile_label_poll.clone(),
                            mail: mail_poll.clone(),
                            is_dark,
                        };
                        compose::open_compose_window(&app_poll, &state_poll, ctx);
                    } else {
                        let final_mail = modified.unwrap_or_else(|| mail_poll.clone());
                        let ctx = ComposeContext {
                            profile_label: profile_label_poll.clone(),
                            mail: final_mail,
                            is_dark,
                        };
                        compose::open_compose_window(&app_poll, &state_poll, ctx);
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
    } else {
        // No on_reply hook — open compose with default pre-filled mail
        let ctx = ComposeContext {
            profile_label: profile_label.clone(),
            mail,
            is_dark,
        };
        compose::open_compose_window(app, state, ctx);
    }
}

// ── Edit Draft action ───────────────────────────────────────────────

/// Reopen the selected draft in a compose window.
fn trigger_edit_draft(
    state: &Rc<RefCell<AppState>>,
    selected_node: &Rc<RefCell<Option<ThreadNode>>>,
    active_folder_kind: &Rc<RefCell<FolderKind>>,
    app: &gtk4::Application,
    status_label: &Label,
    is_dark: bool,
) {
    if !matches!(*active_folder_kind.borrow(), FolderKind::Drafts) {
        status_label.set_text("Select a draft in the Drafts folder first");
        return;
    }

    let node = match selected_node.borrow().as_ref() {
        Some(n) => n.clone(),
        None => {
            status_label.set_text("No draft selected — select a draft first");
            return;
        }
    };

    let s = state.borrow();
    let profile_label = s.active_profile.borrow().clone();
    if profile_label.is_empty() {
        status_label.set_text("No profile selected — select a profile first");
        return;
    }
    let profile = match s.profiles.get(&profile_label) {
        Some(p) => p.clone(),
        None => {
            status_label.set_text(&format!("Profile '{}' not found", profile_label));
            return;
        }
    };

    let filename = node.filename();
    if filename.is_empty() {
        status_label.set_text("Draft has no file on disk");
        return;
    }
    let raw = match std::fs::read(profile.maildir.join(&filename)) {
        Ok(r) => r,
        Err(e) => {
            status_label.set_text(&format!("Cannot read draft: {}", e));
            return;
        }
    };
    let mail = match lorebird_core::compose::Mail::from_raw(&raw) {
        Some(m) => m,
        None => {
            status_label.set_text("Cannot parse draft");
            return;
        }
    };

    let ctx = ComposeContext {
        profile_label,
        mail,
        is_dark,
    };
    compose::open_compose_window(app, state, ctx);
}

/// Delete the currently selected draft from disk.
/// Only works when viewing the Drafts folder.
fn trigger_delete_draft(
    state: &Rc<RefCell<AppState>>,
    selected_node: &Rc<RefCell<Option<ThreadNode>>>,
    active_folder_kind: &Rc<RefCell<FolderKind>>,
    status_label: &Label,
) {
    if !matches!(*active_folder_kind.borrow(), FolderKind::Drafts) {
        status_label.set_text("Select a draft in the Drafts folder first");
        return;
    }

    let node = match selected_node.borrow().as_ref() {
        Some(n) => n.clone(),
        None => {
            status_label.set_text("No draft selected — select a draft first");
            return;
        }
    };

    let s = state.borrow();
    let profile_label = s.active_profile.borrow().clone();
    if profile_label.is_empty() {
        status_label.set_text("No profile selected — select a profile first");
        return;
    }
    let profile = match s.profiles.get(&profile_label) {
        Some(p) => p.clone(),
        None => {
            status_label.set_text(&format!("Profile '{}' not found", profile_label));
            return;
        }
    };

    let filename = node.filename();
    if filename.is_empty() {
        status_label.set_text("Draft has no file on disk");
        return;
    }

    let raw = match std::fs::read(profile.maildir.join(&filename)) {
        Ok(r) => r,
        Err(e) => {
            status_label.set_text(&format!("Cannot read draft: {}", e));
            return;
        }
    };
    let mail = match lorebird_core::compose::Mail::from_raw(&raw) {
        Some(m) => m,
        None => {
            status_label.set_text("Cannot parse draft");
            return;
        }
    };

    let draft_id = mail.draft_id();
    let drafts_dir = profile.maildir.join("Drafts");
    match lorebird_core::maildir::delete_draft(&drafts_dir, &draft_id) {
        Ok(()) => {
            status_label.set_text(&format!("Deleted draft: {}", &filename));
            // Remove just the deleted node from the model — no full rebuild.
            let model = &s.root_model;
            for i in (0..model.n_items()).rev() {
                if let Some(item) = model.item(i).and_downcast::<ThreadNode>() {
                    if item.filename() == node.filename() {
                        model.remove(i);
                        break;
                    }
                }
            }
            // Clear the stale selection.
            *selected_node.borrow_mut() = None;
        }
        Err(e) => status_label.set_text(&format!("Failed to delete draft: {}", e)),
    }
}

// ── Sidebar ───────────────────────────────────────────────────────

/// Build the sidebar from the loaded config.
fn build_sidebar(state: &AppState) -> (ScrolledWindow, ListStore, gtk4::ListBox) {
    let scrolled = ScrolledWindow::new();
    scrolled.set_policy(PolicyType::Never, PolicyType::Automatic);
    // Low floor so the pane can be dragged narrow (names ellipsise); the
    // header toggle hides it outright.
    scrolled.set_min_content_width(90);

    // Build the model of FolderItems
    let sidebar_model = ListStore::new::<FolderItem>();

    // Sort profiles alphabetically
    let mut profile_labels: Vec<String> = state.profiles.keys().cloned().collect();
    profile_labels.sort();

    for label in &profile_labels {
        let profile = &state.profiles[label];

        // Profile header
        sidebar_model.append(&FolderItem::profile_header(label));
        // All Mail
        sidebar_model.append(&FolderItem::all_mail(label));
        // Drafts
        sidebar_model.append(&FolderItem::drafts(label));
        // Views
        for view in &profile.views {
            sidebar_model.append(&FolderItem::view(label, &view.label, &view.query, view.inbox));
        }
        // Separator (modelled as a disabled item)
        sidebar_model.append(&FolderItem::separator());
    }

    // Followed series section (global; rows run against the first profile).
    if let Some(def) = profile_labels.first() {
        refresh_follow_rows(&sidebar_model, def, &state.follows.borrow());
    }

    // If no profiles, show a helpful placeholder
    if profile_labels.is_empty() {
        sidebar_model.append(&FolderItem::placeholder(&format!(
            "No profiles configured.\n\n\
                 Create {}\n\
                 or start with --config <path>",
            lorebird_core::config_dir::lorebird_conf_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<config dir>/lorebird/config.lua".to_string()),
        )));
    }

    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::Single);
    list_box.add_css_class("navigation-sidebar");

    // Bind model → ListBox rows
    list_box.bind_model(Some(&sidebar_model), |item: &Object| -> gtk4::Widget {
        let folder_item = item.downcast_ref::<FolderItem>().unwrap();
        let row = make_sidebar_row(folder_item);
        row.upcast::<gtk4::Widget>()
    });

    scrolled.set_child(Some(&list_box));
    (scrolled, sidebar_model, list_box)
}

/// Rebuild the "Followed" section of the sidebar model in place: remove any
/// existing follow / follow-header rows (always at the tail) and re-append
/// from `follows`. Safe to call at build time and after follow/unfollow.
fn refresh_follow_rows(model: &ListStore, profile: &str, follows: &[Follow]) {
    let mut i = model.n_items();
    while i > 0 {
        i -= 1;
        if let Some(item) = model.item(i).and_downcast::<FolderItem>() {
            let kind = item.row_kind();
            if kind == "follow" || kind == "follow-header" {
                model.remove(i);
            }
        }
    }
    if follows.is_empty() {
        return;
    }
    model.append(&FolderItem::follow_header());
    for f in follows {
        model.append(&FolderItem::follow(profile, &f.label, &f.query));
    }
}

/// Confirm dialog for following a series. Prefills the label and match phrase
/// from the normalised subject and lets the user tweak them before saving.
fn open_follow_dialog(
    parent: &ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    sidebar_model: &ListStore,
    default_profile: &str,
    spinner: &Spinner,
    status: &Label,
    subject: &str,
) {
    let key = lorebird_core::series::series_key(subject);

    let dialog = gtk4::Window::builder()
        .title("Follow series")
        .transient_for(parent)
        .modal(true)
        .default_width(460)
        .build();

    let vbox = Box::new(Orientation::Vertical, 10);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);

    let intro = Label::new(Some(
        "Follow this series — matching threads stay one click away, across every version (past and future).",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro.add_css_class("dim-label");
    vbox.append(&intro);

    let label_lbl = Label::new(Some("Label"));
    label_lbl.set_xalign(0.0);
    let label_entry = gtk4::Entry::new();
    label_entry.set_text(&key);
    vbox.append(&label_lbl);
    vbox.append(&label_entry);

    let match_lbl = Label::new(Some("Match subject (phrase)"));
    match_lbl.set_xalign(0.0);
    let match_entry = gtk4::Entry::new();
    match_entry.set_text(&key);
    vbox.append(&match_lbl);
    vbox.append(&match_entry);

    let inbox_check = gtk4::CheckButton::with_label("Add to inbox");
    inbox_check.set_active(true);
    vbox.append(&inbox_check);

    let btn_box = Box::new(Orientation::Horizontal, 8);
    btn_box.set_halign(gtk4::Align::End);
    btn_box.set_margin_top(8);
    let cancel_btn = gtk4::Button::with_label("Cancel");
    let follow_btn = gtk4::Button::with_label("Follow");
    follow_btn.add_css_class("suggested-action");
    btn_box.append(&cancel_btn);
    btn_box.append(&follow_btn);
    vbox.append(&btn_box);

    dialog.set_child(Some(&vbox));

    let dialog_for_cancel = dialog.clone();
    cancel_btn.connect_clicked(move |_| dialog_for_cancel.close());

    let state_c = state.clone();
    let sbmodel_c = sidebar_model.clone();
    let defprofile_c = default_profile.to_string();
    let spinner_c = spinner.clone();
    let status_c = status.clone();
    let dialog_c = dialog.clone();
    let label_entry_c = label_entry.clone();
    let match_entry_c = match_entry.clone();
    let inbox_check_c = inbox_check.clone();
    follow_btn.connect_clicked(move |_| {
        let m = match_entry_c.text().to_string().trim().to_string();
        if m.is_empty() {
            status_c.set_text("Match phrase is empty — not followed");
            dialog_c.close();
            return;
        }
        let mut label = label_entry_c.text().to_string();
        if label.trim().is_empty() {
            label = m.clone();
        }
        let query = format!("subject:\"{}\"", m.replace('"', ""));

        let s = state_c.borrow();
        if s.is_followed(&query) {
            status_c.set_text(&format!("Already following: {}", label));
        } else {
            s.add_follow(Follow {
                label: label.clone(),
                query: query.clone(),
                in_inbox: inbox_check_c.is_active(),
            });
            status_c.set_text(&format!("Following: {}", label));
        }
        refresh_follow_rows(&sbmodel_c, &defprofile_c, &s.follows.borrow());

        // If the inbox is on screen and this series joins it, re-run the query.
        if inbox_check_c.is_active() && s.active_is_inbox() {
            if let Some(base) = s.inbox_base_query() {
                let q = s.augment_inbox_query(&base);
                spinner_c.set_spinning(true);
                let profile = s.active_profile.borrow().clone();
                if let Err(e) =
                    s.request_search(q, PendingDesc::View { name: "inbox".to_string(), profile })
                {
                    spinner_c.set_spinning(false);
                    status_c.set_text(&format!("Inbox refresh failed: {}", e));
                }
            }
        }
        drop(s);
        dialog_c.close();
    });

    dialog.present();
}

/// Build a `ListBoxRow` widget for a `FolderItem`.
fn make_sidebar_row(item: &FolderItem) -> ListBoxRow {
    let hbox = Box::new(Orientation::Horizontal, 6);
    hbox.set_margin_top(4);
    hbox.set_margin_bottom(4);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);

    // Separators and placeholders are non-selectable
    let kind = FolderKind::from_str(&item.row_kind()).unwrap_or(FolderKind::Placeholder);
    if matches!(kind, FolderKind::Separator) {
        let sep = gtk4::Separator::new(Orientation::Horizontal);
        hbox.append(&sep);
        let row = ListBoxRow::new();
        row.set_child(Some(&hbox));
        row.set_selectable(false);
        row.set_activatable(false);
        row.add_css_class("separator");
        return row;
    }

    if matches!(kind, FolderKind::Placeholder) {
        let label = Label::new(Some(&item.name()));
        label.set_justify(gtk4::Justification::Center);
        label.add_css_class("dim-label");
        label.add_css_class("caption");
        label.set_wrap(true);
        hbox.append(&label);
        let row = ListBoxRow::new();
        row.set_child(Some(&hbox));
        row.set_selectable(false);
        row.set_activatable(false);
        return row;
    }

    // Normal row: icon + name
    let icon_name = item.icon_name();
    if !icon_name.is_empty() {
        let img = Image::from_icon_name(&icon_name);
        img.set_icon_size(IconSize::Normal);
        hbox.append(&img);
    }

    let label = Label::new(Some(&item.name()));
    label.set_hexpand(true);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    if matches!(kind, FolderKind::ProfileHeader | FolderKind::FollowHeader) {
        label.add_css_class("heading");
        label.add_css_class("caption");
    }
    hbox.append(&label);

    let count = item.count();
    if count > 0 {
        let count_lbl = Label::new(Some(&count.to_string()));
        count_lbl.add_css_class("dim-label");
        count_lbl.add_css_class("caption");
        hbox.append(&count_lbl);
    }

    let row = ListBoxRow::new();
    row.set_child(Some(&hbox));

    // Profile headers stay selectable (they set the active profile).
    // The "Followed" header is a label only — not a selectable row.
    if matches!(kind, FolderKind::FollowHeader) {
        row.set_selectable(false);
        row.set_activatable(false);
    }

    row
}

// ── Center pane ───────────────────────────────────────────────────

/// Labels in the preview pane that need to be updated on selection change.
pub(crate) struct PreviewLabels {
    pub from_label: Label,
    pub to_label: Label,
    pub cc_label: Label,
    pub subject_label: Label,
    pub date_label: Label,
    pub body_buffer: sv::Buffer,
    /// Reveals the full From/To/Cc when they are truncated.
    pub expand_toggle: ToggleButton,
}

/// Build the centre pane, returning the root widget, the selection model
/// (for wiring to the preview), and the preview labels.
fn build_center_pane(
    root_model: &ListStore,
    is_dark: bool,
    expand_headers: bool,
) -> (
    Box,
    SingleSelection,
    ColumnView,
    PreviewLabels,
    SearchEntry,
    Rc<Cell<bool>>,
) {
    let vbox = Box::new(Orientation::Vertical, 0);

    // ── Search bar ────────────────────────────────────────────
    let search = SearchEntry::new();
    search.set_hexpand(true);
    search.set_placeholder_text(Some("Search mail\u{2026}"));
    search.set_margin_top(6);
    search.set_margin_bottom(6);
    search.set_margin_start(8);
    search.set_margin_end(8);
    vbox.append(&search);

    // ── Thread list ──────────────────────────────────────────
    let (column_view, selection, expand_guard) = build_thread_list(root_model);

    let scrolled = ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_child(Some(&column_view));
    vbox.append(&scrolled);

    // ── Preview labels (updated on selection) ────────────────
    let from_label = Label::new(Some("no message selected"));
    from_label.set_xalign(0.0);
    from_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let to_label = Label::new(Some(""));
    to_label.set_xalign(0.0);
    to_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let cc_label = Label::new(Some(""));
    cc_label.set_xalign(0.0);
    cc_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let subject_label = Label::new(Some(""));
    subject_label.set_xalign(0.0);
    let date_label = Label::new(Some(""));
    date_label.set_xalign(0.0);
    let body_buffer = sv::Buffer::new(None::<&gtk4::TextTagTable>);
    body_buffer.set_highlight_syntax(true);
    // Sync SourceView style scheme with the app theme
    let style_mgr = sv::StyleSchemeManager::default();
    let scheme_name = if is_dark { "Adwaita-dark" } else { "kate" };
    let fallback_name = if is_dark { "oblivion" } else { "Adwaita" };
    if let Some(scheme) = style_mgr
        .scheme(scheme_name)
        .or_else(|| style_mgr.scheme(fallback_name))
    {
        body_buffer.set_style_scheme(Some(&scheme));
    }
    body_buffer.set_text("Select a profile, then click Refresh to load messages.");
    // Placeholder: no language, no highlighting
    body_buffer.set_language(None);

    let expand_toggle = ToggleButton::new();
    expand_toggle.set_icon_name(header_toggle_icon(expand_headers));
    expand_toggle.set_tooltip_text(Some("Show full From/To/Cc"));
    expand_toggle.add_css_class("flat");
    expand_toggle.set_valign(Align::Start);
    expand_toggle.set_visible(false);
    expand_toggle.set_active(expand_headers);

    let preview_labels = PreviewLabels {
        from_label,
        to_label,
        cc_label,
        subject_label,
        date_label,
        body_buffer,
        expand_toggle,
    };

    (
        vbox,
        selection,
        column_view,
        preview_labels,
        search,
        expand_guard,
    )
}

// ── Thread list (ColumnView + TreeListModel) ──────────────────────

fn build_thread_list(root_model: &ListStore) -> (ColumnView, SingleSelection, Rc<Cell<bool>>) {
    // True during a programmatic recursive expansion, so the per-row
    // `expanded` notify handlers do not launch nested sweeps.
    let expand_guard: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // ── Column view (created first to get its composite sorter) ──
    //
    // GTK ColumnView sorting works like this:
    // 1. Each sortable column has a CustomSorter that compares items
    // 2. ColumnView.get_sorter() returns a composite sorter reflecting
    //    the column the user last clicked and direction
    // 3. That composite sorter drives a SortListModel, which keeps
    //    root-level items in sorted order
    // 4. TreeListModel wraps SortListModel for expansion
    let column_view = ColumnView::new(None::<SingleSelection>);
    column_view.set_vexpand(true);
    column_view.set_hexpand(true);

    // — Column: Subject (with tree expander) ─────────────────
    let subject_factory = SignalListItemFactory::new();
    subject_factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let expander = TreeExpander::new();
        let label = Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        expander.set_child(Some(&label));
        list_item.set_child(Some(&expander));
    });
    let guard_for_bind = expand_guard.clone();
    subject_factory.connect_bind(move |_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let row = list_item.item().and_downcast::<TreeListRow>().unwrap();
        let expander = list_item.child().and_downcast::<TreeExpander>().unwrap();
        expander.set_list_row(Some(&row));
        if let Some(node) = row.item().and_downcast::<ThreadNode>() {
            if let Some(label) = expander.child().and_downcast::<Label>() {
                label.set_label(&node.subject());
            }
            install_tint(list_item, &node, &expander);
        }
        // Make a manual arrow/keyboard expansion go full-depth too.
        install_expand(list_item, &row, &guard_for_bind);
    });
    subject_factory.connect_unbind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        remove_tint(list_item);
        remove_expand(list_item);
    });

    let subject_col = ColumnViewColumn::new(Some("Subject"), Some(subject_factory));
    subject_col.set_expand(true);
    column_view.append_column(&subject_col);

    // — Column: From ──────────────────────────────────────────
    let from_factory = SignalListItemFactory::new();
    from_factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let label = Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        // Keep From tight so the Subject column, which holds the tree indent
        // and truncates first on deep threads, keeps the width.
        label.set_width_chars(12);
        label.add_css_class("dim-label");
        list_item.set_child(Some(&label));
    });
    from_factory.connect_bind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let row = list_item.item().and_downcast::<TreeListRow>().unwrap();
        if let Some(node) = row.item().and_downcast::<ThreadNode>()
            && let Some(label) = list_item.child().and_downcast::<Label>()
        {
            label.set_label(&node.sender());
            install_tint(list_item, &node, &label);
        }
    });
    from_factory.connect_unbind(|_, obj| {
        remove_tint(obj.downcast_ref::<ListItem>().unwrap());
    });

    let from_col = ColumnViewColumn::new(Some("From"), Some(from_factory));
    from_col.set_resizable(true);
    column_view.append_column(&from_col);

    // — Column: Started ──────────────────────────────────────
    let started_factory = SignalListItemFactory::new();
    started_factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let label = Label::new(None);
        label.set_xalign(1.0);
        label.set_width_chars(6);
        label.add_css_class("dim-label");
        label.add_css_class("numeric");
        list_item.set_child(Some(&label));
    });
    started_factory.connect_bind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let row = list_item.item().and_downcast::<TreeListRow>().unwrap();
        if let Some(node) = row.item().and_downcast::<ThreadNode>()
            && let Some(label) = list_item.child().and_downcast::<Label>()
        {
            label.set_label(&node.started());
            install_tint(list_item, &node, &label);
        }
    });
    started_factory.connect_unbind(|_, obj| {
        remove_tint(obj.downcast_ref::<ListItem>().unwrap());
    });

    // Sorters compare ThreadNode items — ColumnView unwraps
    // TreeListRow automatically before passing to sorters.
    let started_sorter = CustomSorter::new(|a, b| {
        let a_node = a.downcast_ref::<ThreadNode>().unwrap();
        let b_node = b.downcast_ref::<ThreadNode>().unwrap();
        match a_node.started_ts().cmp(&b_node.started_ts()) {
            std::cmp::Ordering::Less => Ordering::Smaller,
            std::cmp::Ordering::Equal => Ordering::Equal,
            std::cmp::Ordering::Greater => Ordering::Larger,
        }
    });

    let started_col = ColumnViewColumn::new(Some("Started"), Some(started_factory));
    started_col.set_sorter(Some(&started_sorter));
    started_col.set_resizable(false);
    column_view.append_column(&started_col);

    // — Column: Last Reply ─────────────────────────────────
    let last_reply_factory = SignalListItemFactory::new();
    last_reply_factory.connect_setup(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let label = Label::new(None);
        label.set_xalign(1.0);
        label.set_width_chars(10);
        label.add_css_class("dim-label");
        label.add_css_class("numeric");
        list_item.set_child(Some(&label));
    });
    last_reply_factory.connect_bind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let row = list_item.item().and_downcast::<TreeListRow>().unwrap();
        if let Some(node) = row.item().and_downcast::<ThreadNode>()
            && let Some(label) = list_item.child().and_downcast::<Label>()
        {
            label.set_label(&node.last_reply());
            install_tint(list_item, &node, &label);
        }
    });
    last_reply_factory.connect_unbind(|_, obj| {
        remove_tint(obj.downcast_ref::<ListItem>().unwrap());
    });

    let last_reply_sorter = CustomSorter::new(|a, b| {
        let a_node = a.downcast_ref::<ThreadNode>().unwrap();
        let b_node = b.downcast_ref::<ThreadNode>().unwrap();
        // Natural ascending order; SortType::Descending reverses to newest-first
        match a_node.last_reply_ts().cmp(&b_node.last_reply_ts()) {
            std::cmp::Ordering::Less => Ordering::Smaller,
            std::cmp::Ordering::Equal => Ordering::Equal,
            std::cmp::Ordering::Greater => Ordering::Larger,
        }
    });

    let last_reply_col = ColumnViewColumn::new(Some("Last Reply"), Some(last_reply_factory));
    last_reply_col.set_sorter(Some(&last_reply_sorter));
    last_reply_col.set_resizable(false);
    column_view.append_column(&last_reply_col);

    // ── Model pipeline: SortListModel → TreeListModel → Selection ──
    //
    // ColumnView.get_sorter() returns a composite sorter that tracks
    // which column the user clicked and in which direction.  We plug
    // it into a SortListModel so the root-level items stay sorted.
    let view_sorter = column_view.sorter().expect("ColumnView must have a sorter");
    let sorted_model = SortListModel::new(Some(root_model.clone()), Some(view_sorter));

    let tree_model = TreeListModel::new(
        sorted_model.upcast::<gio::ListModel>(),
        false, // passthrough=false → items are TreeListRow
        false, // autoexpand — user must click to expand
        |item: &Object| -> Option<gio::ListModel> {
            let node = item.downcast_ref::<ThreadNode>()?;
            let children = node.children_store();
            if children.n_items() > 0 {
                Some(children.clone().upcast())
            } else {
                None
            }
        },
    );

    let selection = SingleSelection::new(Some(tree_model));
    selection.set_can_unselect(true);
    selection.set_autoselect(false);

    column_view.set_model(Some(&selection));

    // Default sort: Last Reply descending (newest first)
    column_view.sort_by_column(Some(&last_reply_col), SortType::Descending);

    // Double-click toggles a non-root sub-group; top-level rows belong to the
    // accordion, so skipping them avoids collapsing a just-expanded thread.
    column_view.connect_activate(move |cv, pos| {
        if let Some(item) = cv.model().and_then(|m| m.item(pos))
            && let Some(row) = item.downcast_ref::<TreeListRow>()
            && row.is_expandable()
            && row.parent().is_some()
        {
            row.set_expanded(!row.is_expanded());
        }
    });

    (column_view, selection, expand_guard)
}

// ── Whole-thread tint & recursive expansion helpers ───────────────

/// CSS class applied to every cell child of a row in the selected thread.
const THREAD_TINT_CLASS: &str = "thread-active";
/// `list_item` data key holding the tint `notify` handler and its node.
const TINT_HANDLER_KEY: &str = "lb-tint-handler";
/// `list_item` data key holding the `expanded` notify handler and its row.
const EXPAND_HANDLER_KEY: &str = "lb-expand-handler";

/// Top-level row of the thread containing `row`.
fn root_row_of(row: &TreeListRow) -> TreeListRow {
    let mut cur = row.clone();
    while let Some(parent) = cur.parent() {
        cur = parent;
    }
    cur
}

/// Flag `node`'s whole subtree active or not. Runs on the persistent model
/// objects, so it is unaffected by which rows are expanded or realised.
fn set_subtree_active(node: &ThreadNode, active: bool) {
    node.set_in_selected_thread(active);
    if !node.has_children() {
        return;
    }
    let children = node.children_store();
    for i in 0..children.n_items() {
        if let Some(child) = children.item(i).and_downcast::<ThreadNode>() {
            set_subtree_active(&child, active);
        }
    }
}

/// Expand `row` and every descendant. `children()` yields the underlying
/// model items, not rows, so `child_row` is used to reach each descendant
/// `TreeListRow`; children materialise lazily once `row` is expanded.
fn expand_recursive(row: &TreeListRow) {
    if !row.is_expandable() {
        return;
    }
    if !row.is_expanded() {
        row.set_expanded(true);
    }
    let mut i = 0;
    while let Some(child) = row.child_row(i) {
        expand_recursive(&child);
        i += 1;
    }
}

fn set_thread_tint(widget: &impl glib::object::IsA<gtk4::Widget>, active: bool) {
    // Tint the cell (the child's parent) so the whole field colours, not just
    // the text the label paints over its own allocation.
    let target = widget.parent().unwrap_or_else(|| widget.as_ref().clone());
    if active {
        target.add_css_class(THREAD_TINT_CLASS);
    } else {
        target.remove_css_class(THREAD_TINT_CLASS);
    }
}

/// Tint `widget` now and on every change, via a `notify` handler stashed on
/// the list item. Cleanup on unbind matters because recycled widgets would
/// otherwise leak the handler and tint the wrong row.
fn install_tint<W>(list_item: &ListItem, node: &ThreadNode, widget: &W)
where
    W: glib::object::IsA<gtk4::Widget> + Clone + 'static,
{
    remove_tint(list_item);
    set_thread_tint(widget, node.in_selected_thread());
    let w = widget.clone();
    let handler = node.connect_in_selected_thread_notify(move |n| {
        set_thread_tint(&w, n.in_selected_thread());
    });
    // SAFETY: list items are created and accessed only on the GTK main
    // thread, and this key is always paired with this value type.
    unsafe {
        list_item.set_data(TINT_HANDLER_KEY, (node.clone(), handler));
    }
}

fn remove_tint(list_item: &ListItem) {
    // SAFETY: see install_tint; same key and value type.
    unsafe {
        if let Some((node, handler)) =
            list_item.steal_data::<(ThreadNode, glib::SignalHandlerId)>(TINT_HANDLER_KEY)
        {
            node.disconnect(handler);
        }
    }
}

/// Connect an `expanded` notify so a manual expansion of `row` cascades
/// full-depth. Removed on unbind to avoid leaking across widget recycling.
fn install_expand(list_item: &ListItem, row: &TreeListRow, guard: &Rc<Cell<bool>>) {
    remove_expand(list_item);
    let g = guard.clone();
    let handler = row.connect_expanded_notify(move |row| {
        // Suppress while a programmatic sweep is already running.
        if g.get() {
            return;
        }
        if row.is_expanded() {
            g.set(true);
            expand_recursive(row);
            g.set(false);
        }
    });
    // SAFETY: list items are accessed only on the GTK main thread, and this
    // key is always paired with this value type.
    unsafe {
        list_item.set_data(EXPAND_HANDLER_KEY, (row.clone(), handler));
    }
}

fn remove_expand(list_item: &ListItem) {
    // SAFETY: see install_expand; same key and value type.
    unsafe {
        if let Some((row, handler)) =
            list_item.steal_data::<(TreeListRow, glib::SignalHandlerId)>(EXPAND_HANDLER_KEY)
        {
            row.disconnect(handler);
        }
    }
}

fn build_preview_pane(labels: &PreviewLabels) -> Box {
    let vbox = Box::new(Orientation::Vertical, 0);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(8);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    // ── Headers ──────────────────────────────────────────────
    let headers = Grid::new();
    headers.set_column_spacing(12);
    headers.set_row_spacing(4);
    headers.set_margin_bottom(8);

    let add_row = |grid: &Grid, row: i32, key: &str, value: &Label| {
        let k = make_header_key(key);
        grid.attach(&k, 0, row, 1, 1);
        let v = value.clone();
        v.set_hexpand(true);
        v.set_xalign(0.0);
        v.set_selectable(true);
        grid.attach(&v, 1, row, 1, 1);
    };

    add_row(&headers, 0, "From", &labels.from_label);
    add_row(&headers, 1, "To", &labels.to_label);
    add_row(&headers, 2, "Cc", &labels.cc_label);
    add_row(&headers, 3, "Subject", &labels.subject_label);
    add_row(&headers, 4, "Date", &labels.date_label);

    // Right of the From/To/Cc rows it controls (col 2, spanning rows 0..3).
    headers.attach(&labels.expand_toggle, 2, 0, 1, 3);

    vbox.append(&headers);

    // ── Separator ────────────────────────────────────────────
    let sep = gtk4::Separator::new(Orientation::Horizontal);
    vbox.append(&sep);

    // ── Body ─────────────────────────────────────────────────
    let scrolled = ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_policy(PolicyType::Automatic, PolicyType::Automatic);

    let body_view = sv::View::with_buffer(&labels.body_buffer);
    body_view.set_editable(false);
    body_view.set_cursor_visible(false);
    body_view.set_wrap_mode(WrapMode::WordChar);
    body_view.set_left_margin(4);
    body_view.set_right_margin(4);
    body_view.set_top_margin(4);
    body_view.set_bottom_margin(4);
    body_view.set_show_line_numbers(true);
    body_view.set_monospace(true);

    scrolled.set_child(Some(&body_view));
    vbox.append(&scrolled);

    vbox
}

/// Render message body with diff highlighting and prose tagging.
///
/// Always sets the SourceView language to "diff" so that diff syntax
/// (coloured `+`/`-` lines, `@@` headers, etc.) is highlighted.
/// Prose sections (cover letters, commentary, signatures) get a
/// TextTag with a subtle background tint so they stand out from the
/// diff regions.
fn set_body_with_highlight(buffer: &sv::Buffer, text: &str) {
    let lm = sv::LanguageManager::default();
    let lang = lm.language("diff");
    buffer.set_language(lang.as_ref());
    buffer.set_text(text);
}

fn make_header_key(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.set_xalign(1.0);
    label.add_css_class("dim-label");
    label
}

/// Length past which a From/To/Cc value is truncated in the collapsed view.
const HEADER_TRUNCATE_MAX: usize = 120;

/// Default width of the folder sidebar, in pixels.
const SIDEBAR_WIDTH: i32 = 168;

/// Narrowest the reading pane can be dragged, in monospace columns.
const READING_PANE_MIN_COLUMNS: i32 = 60;

/// Minimum width of the thread list, in pixels, so it cannot be squeezed away.
const THREAD_LIST_MIN_WIDTH: i32 = 320;
/// Default width of the thread list when the window first opens, in pixels.
const THREAD_LIST_DEFAULT_WIDTH: i32 = 520;

/// Pixel width that fits `columns` monospace characters in the reading pane,
/// with an allowance for the body's line-number gutter, margins and scrollbar.
fn reading_pane_width_px(widget: &impl glib::object::IsA<gtk4::Widget>, columns: i32) -> i32 {
    let ctx = widget.pango_context();
    let mut desc = ctx
        .font_description()
        .unwrap_or_else(|| gtk4::pango::FontDescription::from_string("Monospace 11"));
    desc.set_family("Monospace");
    let metrics = ctx.metrics(Some(&desc), None);
    let char_px = (metrics.approximate_char_width() / gtk4::pango::SCALE).max(7);
    char_px * columns + 96
}

fn truncate_addr(s: &str) -> String {
    if s.len() <= HEADER_TRUNCATE_MAX {
        s.to_string()
    } else {
        // Find the last separator before the limit to avoid cutting mid-address
        let cut = s[..HEADER_TRUNCATE_MAX]
            .rfind(',')
            .map(|i| i + 1)
            .unwrap_or(HEADER_TRUNCATE_MAX);
        format!("{}…", &s[..cut])
    }
}

fn header_toggle_icon(expanded: bool) -> &'static str {
    if expanded {
        "pan-up-symbolic"
    } else {
        "pan-down-symbolic"
    }
}

/// Fully wrapped when `expanded`, otherwise a single ellipsised line capped
/// at `HEADER_TRUNCATE_MAX` with the full text in a tooltip.
fn apply_header_field(label: &Label, full: &str, expanded: bool) {
    if expanded {
        label.set_ellipsize(gtk4::pango::EllipsizeMode::None);
        label.set_wrap(true);
        label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        label.set_text(full);
        label.set_tooltip_text(None);
    } else {
        label.set_wrap(false);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_text(&truncate_addr(full));
        if full.len() > HEADER_TRUNCATE_MAX {
            label.set_tooltip_text(Some(full));
        } else {
            label.set_tooltip_text(None);
        }
    }
}

