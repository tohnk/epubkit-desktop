//! Desktop front-end for epubkit.
//!
//! A thin shell around `epubkit-core`: the window reads and writes settings,
//! previews the books that were dropped on it, and runs the pipeline on a
//! worker thread while streaming progress back to the page.
//!
//! Everything the user can see is decided in the core. This crate only moves
//! values across the IPC boundary and off the UI thread — which is also why the
//! commands are ordinary functions, so they can be tested without a window.

pub mod commands;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::devices,
            commands::load_settings,
            commands::save_settings,
            commands::select_preset,
            commands::save_preset,
            commands::delete_preset,
            commands::inspect_books,
            commands::optimize_books,
        ])
        .run(tauri::generate_context!())
        .expect("epubkit failed to start");
}
