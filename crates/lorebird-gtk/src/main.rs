#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod app_state;
mod compose;
mod folder_item;
mod lua_thread;
mod platform;
mod query_thread;
mod thread_node;
mod window;

use gtk4::prelude::*;
use gtk4::Application;

use std::cell::RefCell;
use std::rc::Rc;

use app_state::AppState;

fn main() {
    // Load compiled-in resources (icons)
    gio::resources_register_include!("org.lorebird.app.gresource")
        .expect("Failed to register GResource bundle");

    let app = Application::builder()
        .application_id("org.lorebird.app")
        .build();

    app.connect_activate(|app| {
        // Register our resource path so GTK's icon theme can find our icon
        gtk4::IconTheme::default()
            .add_resource_path("/org/lorebird/app/icons");

        // On macOS, set the Dock / ⌘-Tab icon (no .app bundle when run via cargo).
        platform::set_app_icon();

        // Look for --config <path> on the command line
        let config_path = std::env::args()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[0] == "--config")
            .map(|w| std::path::PathBuf::from(&w[1]));

        let state = Rc::new(RefCell::new(AppState::new(
            config_path.as_deref(),
        )));
        window::build_window(app, &state);
    });

    app.run();
}