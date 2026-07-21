//! Main application window — Thunderbird-style tri-pane layout.
//!
//! The sidebar is built from the loaded config: each profile appears
//! as a header with "All Mail" and its views underneath. Clicking a
//! row sets the active profile (and optionally the view query).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gio::ListStore;
use glib::Object;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box, CustomSorter, FlowBox, Grid, HeaderBar, IconSize,
    Image, Label, ListBoxRow, ListItem, ListView, Ordering, Orientation, Paned, PolicyType,
    ProgressBar, ScrolledWindow, SearchEntry, SignalListItemFactory, SingleSelection, SortListModel,
    ToggleButton, TreeExpander, TreeListModel, TreeListRow, WrapMode,
};
use lorebird_lua::ContactGroup;
use sourceview5 as sv;
use sourceview5::prelude::*;

use crate::app_state::{AppState, PendingDesc};
use crate::compose::{self, ComposeContext};
use crate::folder_item::{FolderItem, FolderKind};
use crate::lua_thread::{LuaCommand, LuaResult};
use crate::thread_node::ThreadNode;
use lorebird_core::compose::Mail;
use lorebird_core::follows::Follow;

// ── Multi-select (bulk archive) state ──────────────────────────────

/// A thread ticked in select mode: its subject (for the series key) and the
/// precomputed ids of the whole thread, so the bulk action needs no live node.
struct PickedThread {
    subject: String,
    ids: Vec<String>,
}

/// Orthogonal "select mode" for bulk actions. Keeps its own set keyed on the
/// root message-id, stable across the model rebuild every view run performs.
/// Independent of the `SingleSelection` that drives preview, tint and reply.
struct SelectMode {
    on: Cell<bool>,
    picked: RefCell<HashMap<String, PickedThread>>,
    /// Tree-model position of the last plainly-checked top-level row, the anchor
    /// for a subsequent Shift+click range. Captured at click time and consumed
    /// synchronously, so a later accordion reshuffle cannot stale it.
    anchor: Cell<Option<u32>>,
    /// Raised while a range fill drives node `checked` flags programmatically.
    /// Each checkbox mirrors its node's `checked` through a property binding, so
    /// those writes would re-enter the `toggled` handler; the flag makes it a
    /// no-op there and leaves the picked set and anchor authoritative.
    in_bulk_update: Cell<bool>,
    /// Whether the primary press that is about to fire a `toggled` held Shift.
    /// A capture-phase gesture records it before the built-in toggle runs; the
    /// `toggled` handler then reconciles with the toggle instead of fighting it,
    /// filling a range rather than flipping this one box.
    shift_pending: Cell<bool>,
}

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
    // Whole-thread tint plus the recipient-pill rules. The pill rules are built
    // once from the distinct colours the configured contact groups actually use:
    // one `.pill-c<hex>` class per colour, a neutral `.pill` base, and a `.pill-dim`
    // for unmatched recipients. `@theme_fg_color` keeps the neutral chip theme-adaptive.
    let mut css_data = format!(".thread-active {{ background-color: {tint}; }}\n");
    css_data.push_str(PILL_BASE_CSS);
    let mut seen_pill_colors: HashSet<String> = HashSet::new();
    for group in &state_ref.contact_groups {
        if let Some(hex) = resolve_pill_color(&group.color)
            && seen_pill_colors.insert(hex.clone())
        {
            let class = pill_class_for_hex(&hex);
            css_data.push_str(&format!(
                ".{class} {{ background-color: alpha({hex}, 0.15); color: {hex}; }}\n"
            ));
        }
    }
    let css = gtk4::CssProvider::new();
    css.load_from_data(&css_data);
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

    // ── Progress bar (unified progress affordance) ─────────────
    // Lives in the bottom status row (see below). Drives Refresh (determinate
    // k/N fetch + index) and every quick query (search, view switch, All Mail)
    // as an indeterminate pulse. Hidden when idle. It carries only the fraction
    // (shown as a percentage); the descriptive text lives in `status_label`, so
    // the bar keeps a fixed width and never reflows as the query text changes.
    // `progress_indeterminate` tells the query poller to pulse it on each tick
    // while a non-determinate operation is in flight.
    let progress = ProgressBar::new();
    progress.set_show_text(true);
    progress.set_visible(false);
    progress.set_valign(Align::Center);
    progress.set_size_request(STATUS_PROGRESS_WIDTH, -1);
    let progress_indeterminate = Rc::new(Cell::new(false));

    // ── Status bar (created early so callbacks can clone it) ──
    // Fixed-width, ellipsizing column so a long query label truncates in place
    // instead of resizing the row.
    let status_label = Label::new(Some("Ready \u{2014} select a profile, then Refresh"));
    status_label.set_xalign(0.0);
    status_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    status_label.set_width_chars(STATUS_TEXT_CHARS);
    status_label.set_max_width_chars(STATUS_TEXT_CHARS);
    status_label.set_margin_top(4);
    status_label.set_margin_bottom(4);
    status_label.add_css_class("dim-label");
    status_label.add_css_class("caption");

    header.pack_end(&refresh_btn);
    header.pack_end(&reply_btn);
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

    // Toggle the per-row metadata line (sender / started / last reply). A
    // GObject property binding set up in the row factory mirrors this button's
    // `active` onto every metadata label's `visible`, so realized rows update
    // live without re-running the factory. Default on.
    let meta_toggle = ToggleButton::new();
    meta_toggle.set_icon_name("lorebird-details-symbolic");
    meta_toggle.set_tooltip_text(Some("Show or hide message details"));
    meta_toggle.add_css_class("flat");
    meta_toggle.set_active(true);
    header.pack_start(&meta_toggle);

    // ── Multi-select mode ─────────────────────────────────────
    // A header toggle turns on per-row checkboxes and reveals a bulk-action
    // bar. The authoritative set lives in SelectMode, keyed on root
    // message-id so it survives the model rebuild every view run performs.
    let select_mode = Rc::new(SelectMode {
        on: Cell::new(false),
        picked: RefCell::new(HashMap::new()),
        anchor: Cell::new(None),
        in_bulk_update: Cell::new(false),
        shift_pending: Cell::new(false),
    });
    // A text label rather than an icon: this theme only carries the bundled
    // custom symbolics, so a stock checkbox icon renders blank.
    let select_toggle = ToggleButton::with_label("Select");
    select_toggle.set_tooltip_text(Some("Select multiple threads for bulk actions"));
    select_toggle.add_css_class("flat");
    header.pack_start(&select_toggle);

    // Bulk-action bar, revealed only in select mode. The archive/unarchive
    // labels carry the live selection count; a closure recomputes them.
    let bulk_archive_btn = gtk4::Button::with_label("Archive selected (0)");
    bulk_archive_btn.add_css_class("flat");
    let bulk_unarchive_btn = gtk4::Button::with_label("Unarchive selected (0)");
    bulk_unarchive_btn.add_css_class("flat");
    let bulk_follow_btn = gtk4::Button::with_label("Follow selected (0)");
    bulk_follow_btn.add_css_class("flat");
    let bulk_clear_btn = gtk4::Button::with_label("Clear");
    bulk_clear_btn.add_css_class("flat");
    let bulk_bar = Box::new(Orientation::Horizontal, 6);
    bulk_bar.set_margin_top(4);
    bulk_bar.set_margin_bottom(4);
    bulk_bar.set_margin_start(8);
    bulk_bar.set_margin_end(8);
    bulk_bar.append(&bulk_archive_btn);
    bulk_bar.append(&bulk_unarchive_btn);
    bulk_bar.append(&bulk_follow_btn);
    bulk_bar.append(&bulk_clear_btn);
    let bulk_revealer = gtk4::Revealer::new();
    bulk_revealer.set_child(Some(&bulk_bar));
    bulk_revealer.set_reveal_child(false);

    // Recompute the bulk-action button labels/sensitivity from the picked set.
    let refresh_bulk_ui: Rc<dyn Fn()> = {
        let select_mode = select_mode.clone();
        let archive_btn = bulk_archive_btn.clone();
        let unarchive_btn = bulk_unarchive_btn.clone();
        let follow_btn = bulk_follow_btn.clone();
        Rc::new(move || {
            let n = select_mode.picked.borrow().len();
            archive_btn.set_label(&format!("Archive selected ({n})"));
            unarchive_btn.set_label(&format!("Unarchive selected ({n})"));
            follow_btn.set_label(&format!("Follow selected ({n})"));
            let any = n > 0;
            archive_btn.set_sensitive(any);
            unarchive_btn.set_sensitive(any);
            follow_btn.set_sensitive(any);
        })
    };
    refresh_bulk_ui();

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
    let (center, selection, thread_view, preview_labels, search_entry, expand_guard) =
        build_center_pane(
            &state_ref.root_model,
            is_dark,
            state_ref.expand_headers,
            &meta_toggle,
            state_ref.compact_list,
            &select_toggle,
            &select_mode,
            &refresh_bulk_ui,
        );
    let lore_btn = preview_labels.lore_btn.clone();
    // Bulk-action bar sits above the search/thread list, revealed in select mode.
    center.prepend(&bulk_revealer);
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
    // Bottom status row: a fixed-width text column and a fixed-width progress
    // bar, clustered at the right edge so both keep a constant size and place.
    let status_row = Box::new(Orientation::Horizontal, 12);
    status_row.set_halign(Align::End);
    status_row.set_margin_start(8);
    status_row.set_margin_end(8);
    status_row.append(&status_label);
    status_row.append(&progress);
    main_vbox.append(&status_row);
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
    let progress_for_refresh = progress.clone();
    let indeterminate_for_refresh = progress_indeterminate.clone();
    let refresh_btn_ref = refresh_btn.clone();
    refresh_btn.connect_clicked(move |_btn| {
        refresh_btn_ref.set_sensitive(false);
        let s = state_for_refresh.borrow();
        match s.request_fetch() {
            Ok(()) => {
                // Determinate segments (fetch/index) own the bar; the poller
                // pulses only while a segment reports an unknown total.
                indeterminate_for_refresh.set(false);
                progress_for_refresh.set_visible(true);
                progress_for_refresh.set_fraction(0.0);
                progress_for_refresh.set_text(Some("Refreshing\u{2026}"));
                status_for_refresh.set_text("Refreshing\u{2026}");
                let state_poll = state_for_refresh.clone();
                let status_poll = status_for_refresh.clone();
                let progress_poll = progress_for_refresh.clone();
                let indeterminate_poll = indeterminate_for_refresh.clone();
                let btn_poll = refresh_btn_ref.clone();
                glib::timeout_add_local(Duration::from_millis(100), move || {
                    let s = state_poll.borrow();
                    // Drain every available result this tick, updating the bar
                    // for each non-terminal FetchProgress and breaking only on
                    // the terminal FetchDone (or an error).
                    loop {
                        let Some(result) = s.poll_fetch_result() else {
                            // Nothing terminal yet. If the current segment has
                            // no known total we pulse; otherwise the last
                            // fraction stands.
                            if indeterminate_poll.get() {
                                progress_poll.pulse();
                            }
                            return glib::ControlFlow::Continue;
                        };
                        if let LuaResult::FetchProgress { phase, step, total, label, .. } = &result {
                            let display = crate::app_state::describe_fetch_progress(
                                *phase, *step, *total, label,
                            );
                            match crate::app_state::fetch_progress_fraction(*phase, *step, *total) {
                                Some(f) => {
                                    indeterminate_poll.set(false);
                                    progress_poll.set_fraction(f);
                                    // Text None lets the bar show its percentage.
                                    progress_poll.set_text(None);
                                }
                                None => {
                                    indeterminate_poll.set(true);
                                    progress_poll.pulse();
                                    // Blank the bar text: a percentage is
                                    // meaningless while pulsing.
                                    progress_poll.set_text(Some(""));
                                }
                            }
                            status_poll.set_text(&display);
                            continue;
                        }

                        // Terminal result: hand off to the query rebuild phase.
                        btn_poll.set_sensitive(true);
                        match s.handle_fetch_result(&result) {
                            Ok(()) => {
                                // The list rebuild was dispatched to the query
                                // worker; the persistent query poller finishes
                                // the bar (0.9 → 1.0) and hides it on done.
                                indeterminate_poll.set(false);
                                progress_poll.set_fraction(0.9);
                                progress_poll.set_text(None);
                                status_poll.set_text("Rebuilding view\u{2026}");
                            }
                            Err(e) => {
                                indeterminate_poll.set(false);
                                progress_poll.set_visible(false);
                                progress_poll.set_text(None);
                                status_poll.set_text(&format!("Refresh error: {}", e));
                            }
                        }
                        return glib::ControlFlow::Break;
                    }
                });
            }
            Err(e) => {
                refresh_btn_ref.set_sensitive(true);
                progress_for_refresh.set_visible(false);
                status_for_refresh.set_text(&format!("Refresh error: {}", e));
            }
        }
    });

    // ── Persistent query poller ──────────────────────────────────
    // The background query worker delivers `PlainNode` trees here in
    // batches (newest-first). This poller applies current batches, discards
    // stale ones, and updates the status bar. On the first batch it scrolls
    // to the top so the newest mail is visible immediately; the progress bar
    // keeps pulsing (or holding its fetch fraction) until the final batch.
    let state_for_qpoll = state.clone();
    let status_for_qpoll = status_label.clone();
    let progress_for_qpoll = progress.clone();
    let indeterminate_for_qpoll = progress_indeterminate.clone();
    let column_view_for_qpoll = thread_view.clone();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let s = state_for_qpoll.borrow();
        let mut scroll_to_top = false;
        let mut finished = false;
        while let Some(result) = s.poll_query_result() {
            if let Some(outcome) = s.apply_query_result(&result) {
                status_for_qpoll.set_text(&outcome.status);
                if outcome.first {
                    scroll_to_top = true;
                }
                if outcome.done {
                    finished = true;
                }
            }
        }
        if finished {
            // Complete and hide the bar. This is the terminal step for both the
            // post-fetch rebuild (bar arrives at 0.9) and any quick indeterminate
            // query (search / view switch / All Mail). A short delay lets the
            // full bar register before it disappears.
            indeterminate_for_qpoll.set(false);
            if progress_for_qpoll.is_visible() {
                progress_for_qpoll.set_fraction(1.0);
                let p = progress_for_qpoll.clone();
                glib::timeout_add_local_once(Duration::from_millis(1200), move || {
                    p.set_visible(false);
                    p.set_text(None);
                });
            }
        } else if indeterminate_for_qpoll.get() && progress_for_qpoll.is_visible() {
            // A quick query is streaming with no natural step count: keep the
            // bar pulsing until its final batch arrives.
            progress_for_qpoll.pulse();
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
    let select_toggle_sidebar = select_toggle.clone();
    let column_view_for_sidebar = thread_view.clone();
    let progress_for_sidebar = progress.clone();
    let indeterminate_for_sidebar = progress_indeterminate.clone();
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
        let on_mail = !matches!(kind, FolderKind::Drafts);
        reply_btn_sidebar.set_sensitive(on_mail);
        // Bulk select mode is meaningless in Drafts: turn it off and disable it.
        if !on_mail {
            select_toggle_sidebar.set_active(false);
        }
        select_toggle_sidebar.set_sensitive(on_mail);

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
                    progress_pulse_start(&progress_for_sidebar, &indeterminate_for_sidebar);
                    status_for_sidebar.set_text("Loading\u{2026}");
                    if let Err(e) = s.request_load_all(PendingDesc::AllMail {
                        profile: profile.to_string(),
                    }) {
                        progress_hide(&progress_for_sidebar, &indeterminate_for_sidebar);
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
                progress_pulse_start(&progress_for_sidebar, &indeterminate_for_sidebar);
                status_for_sidebar.set_text("Searching\u{2026}");
                if let Err(e) = s.request_search(
                    effective,
                    PendingDesc::View {
                        name: item.name().to_string(),
                        profile: profile.to_string(),
                    },
                ) {
                    progress_hide(&progress_for_sidebar, &indeterminate_for_sidebar);
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
    // Contact groups drive the per-recipient pill colour. Shared with the
    // selection handler that rebuilds the chips on every selection.
    let contact_groups: Rc<Vec<ContactGroup>> = Rc::new(state_ref.contact_groups.clone());
    let contact_groups_sel = contact_groups.clone();
    // The expand toggle only flips each field's wrap-vs-single-row policy, so it
    // needs the scrolled wrappers, not the chip containers.
    let toggle_from = pl.from.scroll.clone();
    let toggle_to = pl.to.scroll.clone();
    let toggle_cc = pl.cc.scroll.clone();
    let toggle_btn = pl.expand_toggle.clone();
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
            render_recipient_chips(&pl.from.flow, &from_full, &contact_groups_sel);
            render_recipient_chips(&pl.to.flow, &to_full, &contact_groups_sel);
            render_recipient_chips(&pl.cc.flow, &cc_full, &contact_groups_sel);
            apply_chip_expand(&pl.from.scroll, expanded);
            apply_chip_expand(&pl.to.scroll, expanded);
            apply_chip_expand(&pl.cc.scroll, expanded);
            // Offer the toggle only when something is actually clipped.
            let has_long = from_full.len() > HEADER_TRUNCATE_MAX
                || to_full.len() > HEADER_TRUNCATE_MAX
                || cc_full.len() > HEADER_TRUNCATE_MAX;
            pl.expand_toggle.set_visible(has_long);
            pl.subject_label.set_text(&node.subject());
            pl.date_label.set_text(&node.last_reply());
            let mid = node.message_id();
            pl.message_id_label
                .set_text(mid.trim_start_matches('<').trim_end_matches('>'));

            let body = node.body_preview();
            if body.is_empty() {
                set_body_with_highlight(&pl.body_buffer, "(no preview available)");
            } else {
                set_body_with_highlight(&pl.body_buffer, &body);
            }
            return;
        }
        clear_chips(&pl.from.flow);
        clear_chips(&pl.to.flow);
        clear_chips(&pl.cc.flow);
        pl.expand_toggle.set_visible(false);
        pl.subject_label.set_text("");
        pl.date_label.set_text("");
        pl.message_id_label.set_text("");
        set_body_with_highlight(&pl.body_buffer, "");
    });

    toggle_btn.connect_toggled(move |btn| {
        let expanded = btn.is_active();
        btn.set_icon_name(header_toggle_icon(expanded));
        apply_chip_expand(&toggle_from, expanded);
        apply_chip_expand(&toggle_to, expanded);
        apply_chip_expand(&toggle_cc, expanded);
    });

    // ── Track the currently selected node for Reply ────────────
    let selected_node_clone = selected_node.clone();
    let reply_btn_ref = reply_btn.clone();
    let lore_btn_ref = lore_btn.clone();
    let kind_ref = active_folder_kind.clone();
    selection.connect_selection_changed(move |sel, _pos, _n| {
        if let Some(obj) = sel.selected_item()
            && let Some(row) = obj.downcast_ref::<TreeListRow>()
            && let Some(node) = row.item().and_downcast::<ThreadNode>()
        {
            // Reply and Lore are only sensitive when viewing mail, not drafts.
            let on_mail = !matches!(*kind_ref.borrow(), FolderKind::Drafts);
            lore_btn_ref.set_sensitive(on_mail && !node.message_id().is_empty());
            *selected_node_clone.borrow_mut() = Some(node);
            reply_btn_ref.set_sensitive(on_mail);
        } else {
            *selected_node_clone.borrow_mut() = None;
            reply_btn_ref.set_sensitive(false);
            lore_btn_ref.set_sensitive(false);
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
    let progress_for_search = progress.clone();
    let indeterminate_for_search = progress_indeterminate.clone();
    search_entry.connect_activate(move |entry| {
        let query = entry.text().to_string();
        let s = state_for_search.borrow();
        progress_pulse_start(&progress_for_search, &indeterminate_for_search);
        let dispatch = if query.is_empty() {
            status_for_search.set_text("Loading\u{2026}");
            s.request_load_all(PendingDesc::ShowAll)
        } else {
            status_for_search.set_text("Searching\u{2026}");
            s.request_search(query, PendingDesc::Search)
        };
        if let Err(e) = dispatch {
            progress_hide(&progress_for_search, &indeterminate_for_search);
            status_for_search.set_text(&format!("Search error: {}", e));
        }
    });

    // Escape / stop-search → clear search, show all
    let state_for_clear = state.clone();
    let status_for_clear = status_label.clone();
    let progress_for_clear = progress.clone();
    let indeterminate_for_clear = progress_indeterminate.clone();
    search_entry.connect_stop_search(move |entry| {
        entry.set_text("");
        let s = state_for_clear.borrow();
        progress_pulse_start(&progress_for_clear, &indeterminate_for_clear);
        status_for_clear.set_text("Loading\u{2026}");
        if let Err(e) = s.request_load_all(PendingDesc::ShowAll) {
            progress_hide(&progress_for_clear, &indeterminate_for_clear);
            status_for_clear.set_text(&format!("Error: {}", e));
        }
    });

    // ── Context menu (right-click on thread list) ─────────────────
    let context_menu = gtk4::Popover::new();
    let menu_box = Box::new(Orientation::Vertical, 0);
    context_menu.set_parent(&thread_view);
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
    let archive_selected_btn = gtk4::Button::with_label("Archive selected series");
    archive_selected_btn.add_css_class("flat");
    archive_selected_btn.set_margin_top(4);
    archive_selected_btn.set_margin_bottom(4);
    archive_selected_btn.set_margin_start(8);
    archive_selected_btn.set_margin_end(8);
    let unarchive_selected_btn = gtk4::Button::with_label("Unarchive selected series");
    unarchive_selected_btn.add_css_class("flat");
    unarchive_selected_btn.set_margin_top(4);
    unarchive_selected_btn.set_margin_bottom(4);
    unarchive_selected_btn.set_margin_start(8);
    unarchive_selected_btn.set_margin_end(8);
    let follow_selected_btn = gtk4::Button::with_label("Follow selected series");
    follow_selected_btn.add_css_class("flat");
    follow_selected_btn.set_margin_top(4);
    follow_selected_btn.set_margin_bottom(4);
    follow_selected_btn.set_margin_start(8);
    follow_selected_btn.set_margin_end(8);
    let archive_separator = gtk4::Separator::new(Orientation::Horizontal);
    menu_box.append(&reply_menu_btn);
    menu_box.append(&edit_draft_btn);
    menu_box.append(&delete_draft_btn);
    menu_box.append(&follow_menu_btn);
    menu_box.append(&archive_separator);
    menu_box.append(&archive_menu_btn);
    menu_box.append(&unarchive_menu_btn);
    menu_box.append(&archive_selected_btn);
    menu_box.append(&unarchive_selected_btn);
    menu_box.append(&follow_selected_btn);
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
    let progress_ui_for_follow = ProgressUi {
        bar: progress.clone(),
        indeterminate: progress_indeterminate.clone(),
    };
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
                &progress_ui_for_follow,
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
    let progress_for_archive = progress.clone();
    let indeterminate_for_archive = progress_indeterminate.clone();
    let context_menu_for_archive = context_menu.clone();
    archive_menu_btn.connect_clicked(move |_btn| {
        context_menu_for_archive.popdown();
        let sel = selected_for_archive.borrow();
        let Some(node) = sel.as_ref() else {
            status_for_archive.set_text("Select a thread to archive its series");
            return;
        };
        let subject = node.subject();
        let mut ids = Vec::new();
        collect_thread_message_ids(node, &mut ids);
        drop(sel);
        let s = state_for_archive.borrow();
        match s.archive_series(&subject, &ids) {
            Ok(n) => {
                status_for_archive.set_text(&format!("Archived {} message(s)", n));
                progress_pulse_start(&progress_for_archive, &indeterminate_for_archive);
                if let Err(e) = s.rerun_active_view() {
                    progress_hide(&progress_for_archive, &indeterminate_for_archive);
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
    let progress_for_unarchive = progress.clone();
    let indeterminate_for_unarchive = progress_indeterminate.clone();
    let context_menu_for_unarchive = context_menu.clone();
    unarchive_menu_btn.connect_clicked(move |_btn| {
        context_menu_for_unarchive.popdown();
        let sel = selected_for_unarchive.borrow();
        let Some(node) = sel.as_ref() else {
            status_for_unarchive.set_text("Select a thread to unarchive its series");
            return;
        };
        let subject = node.subject();
        let mut ids = Vec::new();
        collect_thread_message_ids(node, &mut ids);
        drop(sel);
        let s = state_for_unarchive.borrow();
        match s.unarchive_series(&subject, &ids) {
            Ok(n) => {
                status_for_unarchive.set_text(&format!("Unarchived {} message(s)", n));
                progress_pulse_start(&progress_for_unarchive, &indeterminate_for_unarchive);
                if let Err(e) = s.rerun_active_view() {
                    progress_hide(&progress_for_unarchive, &indeterminate_for_unarchive);
                    status_for_unarchive.set_text(&format!("Unarchive refresh failed: {}", e));
                }
            }
            Err(e) => status_for_unarchive.set_text(&format!("Unarchive failed: {}", e)),
        }
    });

    // ── Bulk archive / unarchive of the multi-selected threads ─────
    // One closure serves the action-bar buttons and the context-menu entries.
    // `archive == false` runs the unarchive path. It gathers (subject, ids)
    // from the picked set, guards an empty selection, applies the bulk change
    // in a single transaction, clears the set, exits select mode, and runs one
    // rerun_active_view so the refreshed list comes up clean.
    let run_bulk: Rc<dyn Fn(bool)> = {
        let state = state.clone();
        let select_mode = select_mode.clone();
        let select_toggle = select_toggle.clone();
        let status = status_label.clone();
        let progress = progress.clone();
        let indeterminate = progress_indeterminate.clone();
        Rc::new(move |archive: bool| {
            let items: Vec<(String, Vec<String>)> = select_mode
                .picked
                .borrow()
                .values()
                .map(|p| (p.subject.clone(), p.ids.clone()))
                .collect();
            if items.is_empty() {
                status.set_text("Select one or more threads first");
                return;
            }
            let n_threads = items.len();
            let s = state.borrow();
            let res = if archive {
                s.archive_series_bulk(&items)
            } else {
                s.unarchive_series_bulk(&items)
            };
            match res {
                Ok(n) => {
                    let verb = if archive { "Archived" } else { "Unarchived" };
                    status.set_text(&format!("{verb} {n_threads} thread(s), {n} message(s)"));
                    select_mode.picked.borrow_mut().clear();
                    select_mode.anchor.set(None);
                    select_mode.shift_pending.set(false);
                    // Exit select mode so the rebuilt rows come up unchecked;
                    // this also hides the checkboxes and the action bar.
                    select_toggle.set_active(false);
                    progress_pulse_start(&progress, &indeterminate);
                    if let Err(e) = s.rerun_active_view() {
                        progress_hide(&progress, &indeterminate);
                        let noun = if archive { "Archive" } else { "Unarchive" };
                        status.set_text(&format!("{noun} refresh failed: {e}"));
                    }
                }
                Err(e) => {
                    let noun = if archive { "Archive" } else { "Unarchive" };
                    status.set_text(&format!("{noun} failed: {e}"));
                }
            }
        })
    };

    // ── Bulk follow of the multi-selected threads' series ──────────
    // Follows each picked thread's series with default settings (no per-thread
    // dialog, not added to any inbox), deriving the series key from the subject.
    // Empty keys and already-followed queries are skipped; the sidebar is
    // refreshed once afterwards. Like bulk archive, it clears the set and exits
    // select mode.
    let run_bulk_follow: Rc<dyn Fn()> = {
        let state = state.clone();
        let select_mode = select_mode.clone();
        let select_toggle = select_toggle.clone();
        let status = status_label.clone();
        let sidebar_model = sidebar_model_for_follow.clone();
        let default_profile = default_profile.clone();
        Rc::new(move || {
            let subjects: Vec<String> = select_mode
                .picked
                .borrow()
                .values()
                .map(|p| p.subject.clone())
                .collect();
            if subjects.is_empty() {
                status.set_text("Select one or more threads first");
                return;
            }
            let n_threads = subjects.len();
            let s = state.borrow();
            let mut new_count = 0usize;
            for subject in &subjects {
                let key = lorebird_core::series::series_key(subject);
                if key.is_empty() {
                    continue;
                }
                let query = format!("subject:\"{}\"", key.replace('"', ""));
                if s.is_followed(&query) {
                    continue;
                }
                s.add_follow(Follow {
                    label: key,
                    query,
                    in_inbox: false,
                });
                new_count += 1;
            }
            refresh_follow_rows(&sidebar_model, &default_profile, &s.follows.borrow());
            drop(s);
            status.set_text(&format!("Following {n_threads} series ({new_count} new)"));
            select_mode.picked.borrow_mut().clear();
            select_mode.anchor.set(None);
            select_mode.shift_pending.set(false);
            select_toggle.set_active(false);
        })
    };

    // Select-mode toggle: reveal the action bar, or on turning off clear the
    // picked set and untick every live top-level node so checkboxes reset.
    let select_mode_toggle = select_mode.clone();
    let bulk_revealer_toggle = bulk_revealer.clone();
    let root_model_for_toggle = state_ref.root_model.clone();
    let refresh_bulk_toggle = refresh_bulk_ui.clone();
    select_toggle.connect_toggled(move |b| {
        let on = b.is_active();
        select_mode_toggle.on.set(on);
        bulk_revealer_toggle.set_reveal_child(on);
        if !on {
            select_mode_toggle.picked.borrow_mut().clear();
            select_mode_toggle.anchor.set(None);
            select_mode_toggle.shift_pending.set(false);
            for i in 0..root_model_for_toggle.n_items() {
                if let Some(node) = root_model_for_toggle.item(i).and_downcast::<ThreadNode>() {
                    node.set_checked(false);
                }
            }
            refresh_bulk_toggle();
        }
    });

    // Action-bar and context-menu bulk buttons.
    let run_bulk_archive = run_bulk.clone();
    bulk_archive_btn.connect_clicked(move |_| run_bulk_archive(true));
    let run_bulk_unarchive = run_bulk.clone();
    bulk_unarchive_btn.connect_clicked(move |_| run_bulk_unarchive(false));
    let run_bulk_follow_btn = run_bulk_follow.clone();
    bulk_follow_btn.connect_clicked(move |_| run_bulk_follow_btn());

    let select_mode_clear = select_mode.clone();
    let root_model_for_clear = state_ref.root_model.clone();
    let refresh_bulk_clear = refresh_bulk_ui.clone();
    bulk_clear_btn.connect_clicked(move |_| {
        select_mode_clear.picked.borrow_mut().clear();
        select_mode_clear.anchor.set(None);
        select_mode_clear.shift_pending.set(false);
        for i in 0..root_model_for_clear.n_items() {
            if let Some(node) = root_model_for_clear.item(i).and_downcast::<ThreadNode>() {
                node.set_checked(false);
            }
        }
        refresh_bulk_clear();
    });

    let run_bulk_archive_menu = run_bulk.clone();
    let ctx_menu_archive_sel = context_menu.clone();
    archive_selected_btn.connect_clicked(move |_| {
        ctx_menu_archive_sel.popdown();
        run_bulk_archive_menu(true);
    });
    let run_bulk_unarchive_menu = run_bulk.clone();
    let ctx_menu_unarchive_sel = context_menu.clone();
    unarchive_selected_btn.connect_clicked(move |_| {
        ctx_menu_unarchive_sel.popdown();
        run_bulk_unarchive_menu(false);
    });
    let run_bulk_follow_menu = run_bulk_follow.clone();
    let ctx_menu_follow_sel = context_menu.clone();
    follow_selected_btn.connect_clicked(move |_| {
        ctx_menu_follow_sel.popdown();
        run_bulk_follow_menu();
    });

    // Right-click gesture on the column view
    let ctx_menu_ref = context_menu.clone();
    let column_view_for_gesture = thread_view.clone();
    let reply_menu_btn_gesture = reply_menu_btn.clone();
    let edit_draft_btn_gesture = edit_draft_btn.clone();
    let delete_draft_btn_gesture = delete_draft_btn.clone();
    let follow_menu_btn_gesture = follow_menu_btn.clone();
    let archive_menu_btn_gesture = archive_menu_btn.clone();
    let unarchive_menu_btn_gesture = unarchive_menu_btn.clone();
    let archive_selected_gesture = archive_selected_btn.clone();
    let unarchive_selected_gesture = unarchive_selected_btn.clone();
    let follow_selected_gesture = follow_selected_btn.clone();
    let archive_sep_gesture = archive_separator.clone();
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
    let selected_for_gesture = selected_node.clone();
    let kind_for_gesture = active_folder_kind.clone();
    let select_mode_gesture = select_mode.clone();
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
                archive_selected_gesture.set_visible(false);
                unarchive_selected_gesture.set_visible(false);
                follow_selected_gesture.set_visible(false);
            }
            _ => {
                reply_menu_btn_gesture.set_visible(true);
                edit_draft_btn_gesture.set_visible(false);
                delete_draft_btn_gesture.set_visible(false);
                follow_menu_btn_gesture.set_visible(true);
                archive_sep_gesture.set_visible(true);
                archive_menu_btn_gesture.set_visible(true);
                unarchive_menu_btn_gesture.set_visible(true);
                // Bulk entries appear only in select mode and stay insensitive
                // until at least one thread is ticked.
                let on = select_mode_gesture.on.get();
                let any = !select_mode_gesture.picked.borrow().is_empty();
                archive_selected_gesture.set_visible(on);
                unarchive_selected_gesture.set_visible(on);
                follow_selected_gesture.set_visible(on);
                archive_selected_gesture.set_sensitive(any);
                unarchive_selected_gesture.set_sensitive(any);
                follow_selected_gesture.set_sensitive(any);
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
    thread_view.add_controller(gesture);

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

    // ── Lore button: open the message on lore.kernel.org ───────────
    let selected_for_lore = selected_node.clone();
    let window_for_lore = window.clone();
    lore_btn.connect_clicked(move |_| {
        if let Some(node) = selected_for_lore.borrow().as_ref() {
            let mid = node.message_id();
            let mid = mid.trim_start_matches('<').trim_end_matches('>');
            if !mid.is_empty() {
                // /all/ resolves any list; escape the id for the URL path.
                let escaped = glib::Uri::escape_string(mid, Some("@"), false);
                let url = format!("https://lore.kernel.org/all/{}/", escaped);
                gtk4::UriLauncher::new(&url).launch(
                    Some(&window_for_lore),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
        }
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
    let cv_ref = thread_view.clone();
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
    thread_view.add_controller(key_ctrl);

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

/// Collect a thread node's message id and every descendant's, so archiving can
/// cover a whole thread (e.g. a patch series whose siblings differ in subject).
fn collect_thread_message_ids(node: &ThreadNode, out: &mut Vec<String>) {
    let mid = node.message_id();
    if !mid.is_empty() {
        out.push(mid);
    }
    let children = node.children_store();
    for i in 0..children.n_items() {
        if let Some(child) = children.item(i).and_downcast::<ThreadNode>() {
            collect_thread_message_ids(&child, out);
        }
    }
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

/// Start the progress bar in indeterminate (pulsing) mode. The query poller
/// advances the pulse on each tick while `indeterminate` is set, and hides the
/// bar when the operation reports `done`.
fn progress_pulse_start(progress: &ProgressBar, indeterminate: &Rc<Cell<bool>>) {
    indeterminate.set(true);
    progress.set_visible(true);
    progress.set_fraction(0.0);
    // Blank text: a percentage is meaningless while pulsing.
    progress.set_text(Some(""));
    progress.pulse();
}

/// Hide the progress bar and clear indeterminate mode.
fn progress_hide(progress: &ProgressBar, indeterminate: &Rc<Cell<bool>>) {
    indeterminate.set(false);
    progress.set_visible(false);
    progress.set_text(None);
}

/// The unified progress affordance, bundling the bar with its indeterminate
/// flag so the two always travel together. Cloning is cheap (a GObject
/// refcount plus an `Rc`).
#[derive(Clone)]
struct ProgressUi {
    bar: ProgressBar,
    indeterminate: Rc<Cell<bool>>,
}

impl ProgressUi {
    fn pulse_start(&self) {
        progress_pulse_start(&self.bar, &self.indeterminate);
    }
    fn hide(&self) {
        progress_hide(&self.bar, &self.indeterminate);
    }
}

/// Confirm dialog for following a series. Prefills the label and match phrase
/// from the normalised subject and lets the user tweak them before saving.
fn open_follow_dialog(
    parent: &ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    sidebar_model: &ListStore,
    default_profile: &str,
    progress: &ProgressUi,
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
    let progress_c = progress.clone();
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
                progress_c.pulse_start();
                let profile = s.active_profile.borrow().clone();
                if let Err(e) =
                    s.request_search(q, PendingDesc::View { name: "inbox".to_string(), profile })
                {
                    progress_c.hide();
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

/// A recipient field rendered as wrapping pill chips. `flow` holds one chip
/// (a `Label` with the `pill` class) per recipient; `scroll` wraps it so the
/// expand toggle can switch between a single clipped row and a wrapped block.
pub(crate) struct ChipField {
    pub flow: FlowBox,
    pub scroll: ScrolledWindow,
}

/// Labels in the preview pane that need to be updated on selection change.
pub(crate) struct PreviewLabels {
    pub from: ChipField,
    pub to: ChipField,
    pub cc: ChipField,
    pub subject_label: Label,
    pub date_label: Label,
    pub message_id_label: Label,
    pub body_buffer: sv::Buffer,
    /// Reveals the full From/To/Cc when they are truncated.
    pub expand_toggle: ToggleButton,
    /// Opens the selected message on lore.kernel.org; wired in `build_window`.
    pub lore_btn: gtk4::Button,
}

/// Build the centre pane, returning the root widget, the selection model
/// (for wiring to the preview), and the preview labels.
#[allow(clippy::too_many_arguments)]
fn build_center_pane(
    root_model: &ListStore,
    is_dark: bool,
    expand_headers: bool,
    meta_toggle: &ToggleButton,
    compact: bool,
    select_toggle: &ToggleButton,
    select_mode: &Rc<SelectMode>,
    refresh_bulk_ui: &Rc<dyn Fn()>,
) -> (
    Box,
    SingleSelection,
    ListView,
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
    let (thread_view, selection, expand_guard) = build_thread_list(
        root_model,
        meta_toggle,
        compact,
        select_toggle,
        select_mode,
        refresh_bulk_ui,
    );

    let scrolled = ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    // Automatic hscroll (never NEVER) keeps the ListView's content-driven
    // minimum from propagating up: a deep thread's fixed TreeExpander indent
    // scrolls inside the list instead of raising the center pane's minimum and
    // shoving the divider into the reading pane. The subject label wraps within
    // the row width (WordChar), so its minimum stays bounded. Not propagating
    // the natural width keeps the list at its allocated width.
    scrolled.set_policy(PolicyType::Automatic, PolicyType::Automatic);
    scrolled.set_propagate_natural_width(false);
    scrolled.set_child(Some(&thread_view));
    vbox.append(&scrolled);

    // ── Preview recipient chip fields (rebuilt on selection) ──
    let from = make_chip_field(expand_headers);
    let to = make_chip_field(expand_headers);
    let cc = make_chip_field(expand_headers);
    let subject_label = Label::new(Some(""));
    subject_label.set_xalign(0.0);
    // Wrap with a character fallback so the whole subject shows yet its width
    // cannot raise the reading pane's minimum and move the divider on switch.
    subject_label.set_wrap(true);
    subject_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    let date_label = Label::new(Some(""));
    date_label.set_xalign(0.0);
    // Message-ID is shown bare (no angle brackets) so it pastes straight into
    // b4; selectable for manual copy, with a copy button alongside.
    let message_id_label = Label::new(Some(""));
    message_id_label.set_xalign(0.0);
    message_id_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    // Pin the ellipsized minimum to a few characters so a long unbreakable id
    // cannot raise the reading pane's minimum and move the divider.
    message_id_label.set_width_chars(3);
    message_id_label.set_selectable(true);
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

    let lore_btn = gtk4::Button::from_icon_name("lorebird-web-symbolic");
    lore_btn.set_tooltip_text(Some("Open this message on lore.kernel.org"));
    lore_btn.add_css_class("flat");
    lore_btn.set_sensitive(false);

    let preview_labels = PreviewLabels {
        from,
        to,
        cc,
        subject_label,
        date_label,
        message_id_label,
        body_buffer,
        expand_toggle,
        lore_btn,
    };

    (
        vbox,
        selection,
        thread_view,
        preview_labels,
        search,
        expand_guard,
    )
}

// ── Thread list (ListView + TreeListModel) ────────────────────────

/// Build the GitLab-issue-style metadata line: sender, then started and
/// last-reply times. The relative-time strings already carry an " ago"
/// suffix; trim it here so the muted line reads "started 3w  ·  last 1d".
fn meta_line(node: &ThreadNode) -> String {
    let who = sender_display(&node.sender());
    let started = node.started();
    let started = started.strip_suffix(" ago").unwrap_or(&started);
    let last = node.last_reply();
    let last = last.strip_suffix(" ago").unwrap_or(&last);
    format!("{who}  \u{00b7}  started {started}  \u{00b7}  last {last}")
}

fn build_thread_list(
    root_model: &ListStore,
    meta_toggle: &ToggleButton,
    compact: bool,
    select_toggle: &ToggleButton,
    select_mode: &Rc<SelectMode>,
    refresh_bulk_ui: &Rc<dyn Fn()>,
) -> (ListView, SingleSelection, Rc<Cell<bool>>) {
    // True during a programmatic recursive expansion, so the per-row
    // `expanded` notify handlers do not launch nested sweeps.
    let expand_guard: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Row padding: tighter when compact.
    let row_pad = if compact { 0 } else { 2 };

    // ── Single card-style factory ────────────────────────────
    // Each row is a TreeExpander (keeps the tree indent and arrows) wrapping a
    // vertical box: a prominent subject (wrapping to two lines, then ellipsised)
    // over a muted metadata line. No column headers, so a ListView is the
    // cleanest fit: the model pipeline (SortListModel → TreeListModel →
    // SingleSelection) is unchanged, only the sorter now comes from a standalone
    // CustomSorter instead of a column header.
    let factory = SignalListItemFactory::new();
    let toggle_for_setup = meta_toggle.clone();
    factory.connect_setup(move |_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let expander = TreeExpander::new();

        // Row content: an optional select-mode checkbox beside the text column.
        // The checkbox lives inside the row's own content (never the parent
        // GtkListItemWidget), so the tint/manager reentrancy rule is preserved.
        let row_box = Box::new(Orientation::Horizontal, 6);
        row_box.set_hexpand(true);

        // Select-mode checkbox. Only top-level rows show it (bind wires its
        // visibility to the header toggle and its `toggled` handler); child
        // rows keep it hidden, so the binding cannot be set up uniformly here.
        let check = gtk4::CheckButton::new();
        check.set_valign(Align::Center);
        check.set_visible(false);
        row_box.append(&check);

        let vbox = Box::new(Orientation::Vertical, row_pad);
        vbox.set_hexpand(true);
        vbox.set_margin_top(row_pad);
        vbox.set_margin_bottom(row_pad);

        // Subject: full width, wraps to at most two lines, then ellipsises.
        // WordChar wrap keeps the reported minimum width tiny so the row never
        // forces horizontal scrolling or shoves the reading-pane divider.
        let subject = Label::new(None);
        subject.set_xalign(0.0);
        subject.set_yalign(0.0);
        subject.set_hexpand(true);
        subject.set_wrap(true);
        subject.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        subject.set_lines(2);
        subject.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        vbox.append(&subject);

        // Metadata: one muted, secondary line. Visibility is bound to the
        // header toggle's `active` here in setup, so the binding lives with the
        // widget across factory recycling and every realized row updates live.
        let meta = Label::new(None);
        meta.set_xalign(0.0);
        meta.set_yalign(0.0);
        meta.set_hexpand(true);
        meta.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        meta.add_css_class("dim-label");
        meta.add_css_class("caption");
        toggle_for_setup
            .bind_property("active", &meta, "visible")
            .sync_create()
            .build();
        vbox.append(&meta);

        row_box.append(&vbox);
        expander.set_child(Some(&row_box));
        list_item.set_child(Some(&expander));
    });

    let guard_for_bind = expand_guard.clone();
    let select_mode_bind = select_mode.clone();
    let refresh_bulk_bind = refresh_bulk_ui.clone();
    let select_toggle_bind = select_toggle.clone();
    factory.connect_bind(move |_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        let row = list_item.item().and_downcast::<TreeListRow>().unwrap();
        let expander = list_item.child().and_downcast::<TreeExpander>().unwrap();
        expander.set_list_row(Some(&row));
        let row_box = expander.child().and_downcast::<Box>();
        let check = row_box
            .as_ref()
            .and_then(|b| b.first_child())
            .and_downcast::<gtk4::CheckButton>();
        if let Some(node) = row.item().and_downcast::<ThreadNode>() {
            if let Some(vbox) = check
                .as_ref()
                .and_then(|c| c.next_sibling())
                .and_downcast::<Box>()
                && let Some(subject) = vbox.first_child().and_downcast::<Label>()
            {
                subject.set_label(&node.subject());
                if let Some(meta) = subject.next_sibling().and_downcast::<Label>() {
                    meta.set_label(&meta_line(&node));
                }
            }
            // Tint the whole row by colouring the expander, which spans the row
            // (its vbox child hexpands). It must be the expander, never its
            // parent: the parent is the GtkListItemWidget the list-item manager
            // owns, and mutating that from inside bind reenters the manager
            // mid-update and corrupts it.
            install_tint(list_item, &node, &expander);

            // Only top-level rows get an interactive checkbox. Children keep it
            // hidden regardless of the header toggle. The checkbox lives in the
            // row's own box, so it never touches the list-item widget.
            if let Some(check) = check {
                if row.parent().is_none() {
                    install_check(
                        list_item,
                        &check,
                        &node,
                        &select_toggle_bind,
                        &select_mode_bind,
                        &refresh_bulk_bind,
                    );
                } else {
                    // Recycled child row: clear any stale handler and hide it.
                    remove_check(list_item);
                    check.set_active(false);
                    check.set_visible(false);
                }
            }
        }
        // Make a manual arrow/keyboard expansion go full-depth too.
        install_expand(list_item, &row, &guard_for_bind);
    });
    factory.connect_unbind(|_, obj| {
        let list_item = obj.downcast_ref::<ListItem>().unwrap();
        remove_tint(list_item);
        remove_expand(list_item);
        remove_check(list_item);
    });

    // ── Model pipeline: SortListModel → TreeListModel → Selection ──
    //
    // The column headers (and their click-to-sort) are gone, so a standalone
    // sorter keeps the root-level threads ordered by last reply, newest first.
    let last_reply_sorter = CustomSorter::new(|a, b| {
        let a_node = a.downcast_ref::<ThreadNode>().unwrap();
        let b_node = b.downcast_ref::<ThreadNode>().unwrap();
        // Reverse the natural order so the newest last-reply sorts first.
        match b_node.last_reply_ts().cmp(&a_node.last_reply_ts()) {
            std::cmp::Ordering::Less => Ordering::Smaller,
            std::cmp::Ordering::Equal => Ordering::Equal,
            std::cmp::Ordering::Greater => Ordering::Larger,
        }
    });
    let sorted_model = SortListModel::new(Some(root_model.clone()), Some(last_reply_sorter));

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

    let list_view = ListView::new(Some(selection.clone()), Some(factory));
    list_view.set_vexpand(true);
    list_view.set_hexpand(true);

    // Double-click toggles a non-root sub-group; top-level rows belong to the
    // accordion, so skipping them avoids collapsing a just-expanded thread.
    list_view.connect_activate(move |lv, pos| {
        if let Some(item) = lv.model().and_then(|m| m.item(pos))
            && let Some(row) = item.downcast_ref::<TreeListRow>()
            && row.is_expandable()
            && row.parent().is_some()
        {
            row.set_expanded(!row.is_expanded());
        }
    });

    (list_view, selection, expand_guard)
}

// ── Whole-thread tint & recursive expansion helpers ───────────────

/// CSS class applied to every cell child of a row in the selected thread.
const THREAD_TINT_CLASS: &str = "thread-active";
/// `list_item` data key holding the tint `notify` handler and its node.
const TINT_HANDLER_KEY: &str = "lb-tint-handler";
/// `list_item` data key holding the `expanded` notify handler and its row.
const EXPAND_HANDLER_KEY: &str = "lb-expand-handler";
/// `list_item` data key holding the select-mode checkbox `toggled` handler,
/// the checkbox it lives on, and the visibility binding to the header toggle.
const CHECK_HANDLER_KEY: &str = "lb-check-handler";

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
    // Tint the passed widget itself. The caller passes the row's own content
    // widget (the expander), which spans the row. It must not be the
    // GtkListItemWidget the list-item manager owns: mutating that widget's CSS
    // from inside the factory bind reenters `ensure_items` while it is updating
    // the row, which corrupts the manager (GTK_IS_WIDGET assertions, segfault).
    if active {
        widget.add_css_class(THREAD_TINT_CLASS);
    } else {
        widget.remove_css_class(THREAD_TINT_CLASS);
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

/// Tick `node` into the picked set: flag it checked and record its subject and
/// whole-thread ids, keyed on the root message-id. Shared by the checkbox
/// `toggled` handler and the Shift+click range fill.
fn check_node_into_picked(select_mode: &SelectMode, node: &ThreadNode) {
    node.set_checked(true);
    let mut ids = Vec::new();
    collect_thread_message_ids(node, &mut ids);
    select_mode.picked.borrow_mut().insert(
        node.message_id(),
        PickedThread {
            subject: node.subject(),
            ids,
        },
    );
}

/// The list model whose items are the currently-expanded `TreeListRow`s, found
/// by walking up from a row widget to the enclosing `ListView`.
fn thread_list_model(widget: &impl IsA<gtk4::Widget>) -> Option<gio::ListModel> {
    widget
        .ancestor(ListView::static_type())
        .and_downcast::<ListView>()
        .and_then(|lv| lv.model())
        .map(|m| m.upcast::<gio::ListModel>())
}

/// Position of the top-level row backing `node` in `model`, matched by the
/// persistent node identity. Positions index the flattened, expanded rows.
fn top_level_position(model: &gio::ListModel, node: &ThreadNode) -> Option<u32> {
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<TreeListRow>() else {
            continue;
        };
        if row.parent().is_some() {
            continue;
        }
        if let Some(item) = row.item().and_downcast::<ThreadNode>()
            && item.message_id() == node.message_id()
        {
            return Some(i);
        }
    }
    None
}

/// Check every top-level thread between the anchor position and `pos`
/// (inclusive) into the picked set, syncing each realised checkbox. Non-top
/// rows in the flattened range are skipped.
fn fill_range(model: &gio::ListModel, select_mode: &SelectMode, anchor: u32, pos: u32) {
    let (lo, hi) = (anchor.min(pos), anchor.max(pos));
    // The node `checked` writes below drive each realised checkbox through its
    // binding, which re-enters `toggled`; the guard neutralises that there.
    select_mode.in_bulk_update.set(true);
    for i in lo..=hi {
        let Some(row) = model.item(i).and_downcast::<TreeListRow>() else {
            continue;
        };
        if row.parent().is_some() {
            continue;
        }
        if let Some(node) = row.item().and_downcast::<ThreadNode>() {
            check_node_into_picked(select_mode, &node);
        }
    }
    select_mode.in_bulk_update.set(false);
}

/// Wire a top-level row's select-mode checkbox: bind its visibility to the
/// header toggle, seed `active` from the node's `checked`, and connect
/// `toggled` to update the node, the `picked` set, and the action-bar labels.
/// A capture-phase gesture records whether Shift was held on the press; the
/// `toggled` handler then either fills a contiguous range from the standing
/// anchor (Shift) or toggles just this one row and records it as the new anchor
/// (plain). The handler and gesture are stashed like `install_tint` so a
/// recycled row disconnects them (otherwise they would fire on the wrong node).
/// Ids are collected at check time so the bulk action needs no live node
/// reference afterwards.
fn install_check(
    list_item: &ListItem,
    check: &gtk4::CheckButton,
    node: &ThreadNode,
    select_toggle: &ToggleButton,
    select_mode: &Rc<SelectMode>,
    refresh_bulk_ui: &Rc<dyn Fn()>,
) {
    remove_check(list_item);

    let binding = select_toggle
        .bind_property("active", check, "visible")
        .sync_create()
        .build();

    // Seed from the node's checked flag while suppressing the toggled handler
    // (the programmatic set_active below would otherwise miscount the set).
    let syncing = Rc::new(Cell::new(true));
    check.set_active(node.checked());

    // Mirror the node's `checked` onto the box so a range fill (which writes the
    // nodes) updates every realised tick without touching row widgets. One-way:
    // user clicks still flow through `toggled`, which writes the node back.
    let checked_binding = node
        .bind_property("checked", check, "active")
        .sync_create()
        .build();

    let node_c = node.clone();
    let root_mid = node.message_id();
    let sel_mode = select_mode.clone();
    let refresh = refresh_bulk_ui.clone();
    let syncing_h = syncing.clone();
    let check_for_anchor = check.clone();
    let handler = check.connect_toggled(move |c| {
        // Skip the initial seed and any binding-driven update during a range
        // fill: the fill already owns the picked set and the anchor.
        if syncing_h.get() || sel_mode.in_bulk_update.get() {
            return;
        }
        let Some(model) = thread_list_model(&check_for_anchor) else {
            return;
        };
        let Some(pos) = top_level_position(&model, &node_c) else {
            return;
        };
        // A Shift+click extends a range from the standing anchor. The built-in
        // toggle has already flipped this one box either way, so fill_range
        // forces the whole inclusive span (this row included) back to checked,
        // reconciling with the toggle instead of trying to suppress it. The
        // anchor is left unchanged so repeated Shift+clicks keep extending from
        // the same origin.
        if sel_mode.shift_pending.get() {
            sel_mode.shift_pending.set(false);
            let anchor = sel_mode.anchor.get().unwrap_or(pos);
            fill_range(&model, &sel_mode, anchor, pos);
            refresh();
            return;
        }
        let active = c.is_active();
        if active {
            check_node_into_picked(&sel_mode, &node_c);
        } else {
            node_c.set_checked(false);
            sel_mode.picked.borrow_mut().remove(&root_mid);
        }
        // A plain check becomes the anchor for a later Shift+click range; an
        // uncheck clears it so the next Shift+click starts a fresh anchor.
        sel_mode.anchor.set(if active { Some(pos) } else { None });
        refresh();
    });
    syncing.set(false);

    // Capture-phase gesture that only records whether Shift was held on this
    // primary press, before the built-in toggle runs. The `toggled` handler
    // reads it to decide between a range fill and a plain toggle; the gesture
    // does not consume the event, so the CheckButton toggles normally.
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_PRIMARY);
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let sel_mode_g = select_mode.clone();
    gesture.connect_pressed(move |g, _n, _x, _y| {
        if !sel_mode_g.on.get() {
            return;
        }
        let shift = g
            .current_event_state()
            .contains(gtk4::gdk::ModifierType::SHIFT_MASK);
        sel_mode_g.shift_pending.set(shift);
    });
    check.add_controller(gesture.clone());

    // SAFETY: list items are accessed only on the GTK main thread, and this
    // key is always paired with this value type.
    unsafe {
        list_item.set_data(
            CHECK_HANDLER_KEY,
            (check.clone(), handler, binding, checked_binding, gesture),
        );
    }
}

fn remove_check(list_item: &ListItem) {
    // SAFETY: see install_check; same key and value type.
    unsafe {
        if let Some((check, handler, binding, checked_binding, gesture)) = list_item.steal_data::<(
            gtk4::CheckButton,
            glib::SignalHandlerId,
            glib::Binding,
            glib::Binding,
            gtk4::GestureClick,
        )>(CHECK_HANDLER_KEY)
        {
            check.disconnect(handler);
            binding.unbind();
            checked_binding.unbind();
            check.remove_controller(&gesture);
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

    // From/To/Cc are chip rows; the key label aligns to the top of a field that
    // may grow to several wrapped rows.
    let add_chip_row = |grid: &Grid, row: i32, key: &str, field: &ChipField| {
        let k = make_header_key(key);
        k.set_valign(Align::Start);
        grid.attach(&k, 0, row, 1, 1);
        field.scroll.set_hexpand(true);
        grid.attach(&field.scroll, 1, row, 1, 1);
    };

    add_chip_row(&headers, 0, "From", &labels.from);
    add_chip_row(&headers, 1, "To", &labels.to);
    add_chip_row(&headers, 2, "Cc", &labels.cc);
    add_row(&headers, 3, "Subject", &labels.subject_label);
    add_row(&headers, 4, "Date", &labels.date_label);
    add_row(&headers, 5, "Message-ID", &labels.message_id_label);

    // Right of the From/To/Cc rows it controls (col 2, spanning rows 0..3).
    headers.attach(&labels.expand_toggle, 2, 0, 1, 3);

    // Message-ID actions beside its row: copy the bare id, or open on lore.
    let mid_actions = Box::new(Orientation::Horizontal, 4);
    mid_actions.set_valign(Align::Center);
    let copy_mid = gtk4::Button::from_icon_name("edit-copy-symbolic");
    copy_mid.set_tooltip_text(Some("Copy the Message-ID to the clipboard"));
    copy_mid.add_css_class("flat");
    let mid_label = labels.message_id_label.clone();
    copy_mid.connect_clicked(move |b| {
        let mid = mid_label.label();
        if !mid.is_empty() {
            b.clipboard().set_text(&mid);
        }
    });
    mid_actions.append(&copy_mid);
    mid_actions.append(&labels.lore_btn);
    headers.attach(&mid_actions, 2, 5, 1, 1);

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
    body_view.set_monospace(true);

    // Built-in line numbers size the gutter to the buffer's line-count width,
    // so a 5-line and a 5000-line message start their text at different x. Use
    // a custom text renderer with a constant width instead.
    body_view.set_show_line_numbers(false);
    let gutter = sv::prelude::ViewExt::gutter(&body_view, gtk4::TextWindowType::Left);
    let lines = sv::GutterRendererText::new();
    lines.set_xalign(1.0);
    lines.set_xpad(4);
    lines.set_alignment_mode(sv::GutterRendererAlignmentMode::Cell);
    // Fixed budget of 5 digits: patch and email bodies never reach 100k lines.
    let char_px = monospace_char_px(&body_view);
    lines.set_width_request(char_px * LINE_NUMBER_DIGITS + LINE_NUMBER_GUTTER_PAD_PX);
    lines.connect_query_data(move |renderer, _obj, line| {
        renderer.set_text(&(line + 1).to_string());
    });
    gutter.insert(&lines, 0);

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

/// Digits the body line-number gutter reserves, fixing its width so text always
/// starts at the same x. A number wider than this pushes into the body margin.
const LINE_NUMBER_DIGITS: i32 = 5;
/// Padding added to the digit budget for the gutter's xpad on both sides.
const LINE_NUMBER_GUTTER_PAD_PX: i32 = 12;

/// Default width of the folder sidebar, in pixels.
const SIDEBAR_WIDTH: i32 = 168;

/// Narrowest the reading pane can be dragged, in monospace columns.
const READING_PANE_MIN_COLUMNS: i32 = 60;

/// Minimum width of the thread list, in pixels, so it cannot be squeezed away.
const THREAD_LIST_MIN_WIDTH: i32 = 320;
/// Default width of the thread list when the window first opens, in pixels.
const THREAD_LIST_DEFAULT_WIDTH: i32 = 520;

/// Height cap for a recipient chip field, in pixels. Beyond it the field
/// scrolls, so many recipients cannot force the window taller. About five
/// wrapped rows.
const CHIP_FIELD_MAX_HEIGHT: i32 = 140;

/// Fixed width of the bottom status text, in characters. The label ellipsizes
/// so a long query description truncates in place instead of resizing the row.
const STATUS_TEXT_CHARS: i32 = 48;
/// Fixed width of the bottom progress bar, in pixels, so it never reflows.
const STATUS_PROGRESS_WIDTH: i32 = 200;

/// Pixel width that fits `columns` monospace characters in the reading pane,
/// with an allowance for the body's line-number gutter, margins and scrollbar.
fn reading_pane_width_px(widget: &impl glib::object::IsA<gtk4::Widget>, columns: i32) -> i32 {
    monospace_char_px(widget) * columns + 96
}

/// Pixel advance of one monospace character in `widget`'s Pango context.
fn monospace_char_px(widget: &impl glib::object::IsA<gtk4::Widget>) -> i32 {
    let ctx = widget.pango_context();
    let mut desc = ctx
        .font_description()
        .unwrap_or_else(|| gtk4::pango::FontDescription::from_string("Monospace 11"));
    desc.set_family("Monospace");
    let metrics = ctx.metrics(Some(&desc), None);
    (metrics.approximate_char_width() / gtk4::pango::SCALE).max(7)
}

/// Display name of a From header, dropping the `<address>`. With no name, falls
/// back to the address local part, so the From column stays short.
fn sender_display(from: &str) -> String {
    let from = from.trim();
    if let Some(i) = from.find('<') {
        let name = from[..i].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return name.to_string();
        }
        let addr = from[i + 1..].trim_end_matches('>');
        return addr.split('@').next().unwrap_or(addr).to_string();
    }
    from.split('@').next().unwrap_or(from).to_string()
}

fn header_toggle_icon(expanded: bool) -> &'static str {
    if expanded {
        "pan-up-symbolic"
    } else {
        "pan-down-symbolic"
    }
}

// ── Recipient pill chips ──────────────────────────────────────────

/// Base CSS for the recipient pills: a compact chip shape, plus a neutral
/// `.pill-dim` for unmatched recipients. The per-colour `.pill-c<hex>` rules are
/// generated in `build_window` from the configured contact groups. The dim chip
/// uses `@theme_fg_color` so it tracks the light/dark theme.
const PILL_BASE_CSS: &str = "\
.chips > flowboxchild { padding: 0; min-height: 0; min-width: 0; }
.pill { border-radius: 9px; padding: 1px 8px; font-size: 0.9em; }
.pill-dim { background-color: alpha(@theme_fg_color, 0.08); color: alpha(@theme_fg_color, 0.55); }
";

/// The seven palette names accepted in a contact group's `color`, mapped to
/// tasteful Adwaita-ish hexes. A `#rrggbb` value is used verbatim instead.
fn palette_hex(name: &str) -> Option<&'static str> {
    match name {
        "blue" => Some("#3584e4"),
        "green" => Some("#2ec27e"),
        "orange" => Some("#e66100"),
        "red" => Some("#e01b24"),
        "purple" => Some("#9141ac"),
        "teal" => Some("#2190a4"),
        "yellow" => Some("#e5a50a"),
        _ => None,
    }
}

/// Resolve a contact-group colour to a lowercase `#rrggbb` hex. Accepts a
/// palette name or a `#rrggbb` literal; returns `None` for anything else so an
/// unknown colour falls back to the neutral dim chip rather than crashing.
fn resolve_pill_color(color: &str) -> Option<String> {
    let c = color.trim();
    if let Some(hex) = c.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Some(format!("#{}", hex.to_ascii_lowercase()));
        }
        return None;
    }
    palette_hex(&c.to_ascii_lowercase()).map(str::to_string)
}

/// CSS class name for a resolved hex, e.g. `#3584e4` → `pill-c3584e4`. The hex
/// digits form a valid CSS identifier.
fn pill_class_for_hex(hex: &str) -> String {
    format!("pill-c{}", hex.trim_start_matches('#'))
}

/// The first contact group whose patterns match `address` (case-insensitive
/// substring against the bare address). `None` if no group matches.
fn contact_group_for<'a>(address: &str, groups: &'a [ContactGroup]) -> Option<&'a ContactGroup> {
    let addr = address.to_ascii_lowercase();
    groups.iter().find(|g| {
        g.patterns.iter().any(|p| {
            let p = p.trim();
            !p.is_empty() && addr.contains(&p.to_ascii_lowercase())
        })
    })
}

/// Split a recipient list on top-level commas, ignoring commas inside `<...>`.
fn split_addresses(value: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let (mut start, mut depth) = (0usize, 0i32);
    for (i, c) in value.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth <= 0 => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

/// The bare address of a recipient: the part inside `<...>`, or the whole token
/// trimmed when there are no angle brackets.
fn recipient_address(part: &str) -> &str {
    if let Some(i) = part.find('<') {
        let rest = &part[i + 1..];
        return rest.split('>').next().unwrap_or(rest).trim();
    }
    part.trim()
}

/// Pango markup for one recipient: the display name plain, the `<address>`
/// dimmed (alpha is relative to the chip's text colour, so it stays legible in
/// any pill colour). Escapes name and address.
fn recipient_markup(part: &str) -> String {
    let part = part.trim();
    match part.find('<') {
        Some(i) => {
            let name = glib::markup_escape_text(part[..i].trim().trim_matches('"').trim());
            let email = glib::markup_escape_text(part[i..].trim());
            if name.is_empty() {
                format!("<span alpha=\"55%\">{email}</span>")
            } else {
                format!("{name} <span alpha=\"55%\">{email}</span>")
            }
        }
        None => glib::markup_escape_text(part).to_string(),
    }
}

/// Build a recipient chip field: a wrapping `FlowBox` inside a `ScrolledWindow`
/// so the expand toggle can switch between a clipped single row and a wrapped
/// block. Starts collapsed unless `expanded` (the `expand_headers` default).
fn make_chip_field(expanded: bool) -> ChipField {
    let flow = FlowBox::new();
    flow.set_selection_mode(gtk4::SelectionMode::None);
    flow.set_max_children_per_line(1000);
    flow.set_min_children_per_line(1);
    flow.set_row_spacing(2);
    flow.set_column_spacing(2);
    flow.set_homogeneous(false);
    flow.set_halign(Align::Start);
    flow.set_valign(Align::Start);
    flow.add_css_class("chips");

    let scroll = ScrolledWindow::new();
    // Keep the field's natural width from widening the reading pane, but let it
    // grow as tall as the wrapped chips need. A zero minimum content width makes
    // the field width-neutral even in the expanded PolicyType::Never state,
    // where the ScrolledWindow otherwise passes its child minimum straight up.
    scroll.set_propagate_natural_width(false);
    scroll.set_propagate_natural_height(true);
    scroll.set_min_content_width(0);
    // A zero minimum content height keeps an empty (or scrolled) field from
    // reserving the scroller's ~46px default, which three empty recipient rows
    // would otherwise add to the window's minimum height.
    scroll.set_min_content_height(0);
    // Cap the natural height so a message with many recipients does not demand
    // a tall window; beyond this the field scrolls (see `apply_chip_expand`).
    scroll.set_max_content_height(CHIP_FIELD_MAX_HEIGHT);
    scroll.set_child(Some(&flow));
    let field = ChipField { flow, scroll };
    apply_chip_expand(&field.scroll, expanded);
    field
}

/// Expanded wraps the chips to as many rows as needed (no horizontal scroll, so
/// the FlowBox is width-bounded and wraps) and scrolls vertically past
/// `CHIP_FIELD_MAX_HEIGHT`. Vertical `Automatic` (rather than `Never`) is what
/// keeps the field's *minimum* height small: with `Never` the field cannot
/// scroll, so its minimum equals the full wrapped height, and a message with
/// many recipients then forces the whole window taller than its allocation
/// (the "needs at least N" measure warning). Collapsed lays the chips out in
/// one natural-width row, clipped by the scroller (`External` hides the
/// scrollbar while still allowing the row to extend past the viewport).
/// Per-chip tooltips keep the full `Name <addr>` reachable in both modes.
fn apply_chip_expand(scroll: &ScrolledWindow, expanded: bool) {
    if expanded {
        scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
    } else {
        scroll.set_policy(PolicyType::External, PolicyType::Never);
    }
}

/// Remove every chip from a field's FlowBox without leaking child widgets.
fn clear_chips(flow: &FlowBox) {
    while let Some(child) = flow.first_child() {
        flow.remove(&child);
    }
}

/// Rebuild a recipient field's chips from a header value. Each recipient becomes
/// a `pill` chip showing the display name (or address), with the full
/// `Name <addr>` in its tooltip; a contact-group match tints it with that
/// group's colour class, everything else gets the neutral `pill-dim`.
fn render_recipient_chips(flow: &FlowBox, value: &str, groups: &[ContactGroup]) {
    clear_chips(flow);
    for part in split_addresses(value) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Show the full RFC address; the address part is dimmed within the pill.
        let chip = Label::new(None);
        chip.set_markup(&recipient_markup(part));
        chip.add_css_class("pill");
        chip.set_tooltip_text(Some(part));
        chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        // width_chars pins the ellipsized minimum to a few characters. Without
        // it some GTK versions report max_width_chars as the minimum, so a wide
        // recipient would raise the preview minimum past its floor and shove the
        // GtkPaned divider. max_width_chars stays the natural cap; the full
        // `Name <addr>` remains in the tooltip.
        chip.set_width_chars(3);
        chip.set_max_width_chars(48);
        match contact_group_for(recipient_address(part), groups)
            .and_then(|g| resolve_pill_color(&g.color))
        {
            Some(hex) => chip.add_css_class(&pill_class_for_hex(&hex)),
            None => chip.add_css_class("pill-dim"),
        }
        flow.insert(&chip, -1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(color: &str, patterns: &[&str]) -> ContactGroup {
        ContactGroup {
            name: String::new(),
            color: color.to_string(),
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn palette_names_and_hex_resolve() {
        assert_eq!(resolve_pill_color("blue").as_deref(), Some("#3584e4"));
        assert_eq!(resolve_pill_color(" Teal ").as_deref(), Some("#2190a4"));
        assert_eq!(resolve_pill_color("#AABBCC").as_deref(), Some("#aabbcc"));
        // Unknown name and malformed hex fall back to None (dim chip).
        assert_eq!(resolve_pill_color("chartreuse"), None);
        assert_eq!(resolve_pill_color("#xyz"), None);
        assert_eq!(resolve_pill_color("#abcd"), None);
    }

    #[test]
    fn pill_class_is_valid_identifier() {
        assert_eq!(pill_class_for_hex("#3584e4"), "pill-c3584e4");
    }

    #[test]
    fn split_handles_commas_inside_brackets() {
        let parts = split_addresses("Jane <jane@x>, John <john@y>, plain@z");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].trim(), "John <john@y>");
        // A top-level comma inside angle brackets does not split.
        let parts = split_addresses("A <a@x,y>, b@z");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].trim(), "A <a@x,y>");
    }

    #[test]
    fn address_and_markup_extraction() {
        assert_eq!(recipient_address("Jane <jane@kernel.org>"), "jane@kernel.org");
        assert_eq!(recipient_address("bare@kernel.org"), "bare@kernel.org");
        // The chip shows the full RFC address with the address part dimmed.
        assert_eq!(
            recipient_markup("Jane <jane@kernel.org>"),
            "Jane <span alpha=\"55%\">&lt;jane@kernel.org&gt;</span>"
        );
        // No name: just the dimmed address.
        assert_eq!(
            recipient_markup("<jane@kernel.org>"),
            "<span alpha=\"55%\">&lt;jane@kernel.org&gt;</span>"
        );
        assert_eq!(recipient_markup("bare@kernel.org"), "bare@kernel.org");
    }

    #[test]
    fn matching_is_case_insensitive_and_first_wins() {
        let groups = [
            group("blue", &["KERNEL.ORG"]),
            group("green", &["torvalds"]),
        ];
        // Substring match against the address, case-insensitive.
        let g = contact_group_for("Linus@Kernel.Org", &groups).unwrap();
        assert_eq!(g.color, "blue");
        // First matching group wins even when a later one also matches.
        let g = contact_group_for("torvalds@kernel.org", &groups).unwrap();
        assert_eq!(g.color, "blue");
        // No match.
        assert!(contact_group_for("nobody@example.com", &groups).is_none());
        // Empty patterns never match.
        assert!(contact_group_for("a@b.c", &[group("red", &[""])]).is_none());
    }
}

