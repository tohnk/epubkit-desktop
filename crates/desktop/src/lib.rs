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
    // A copy of this binary launched as a decode worker serves requests and
    // never returns here; it must not reach the window. The parent then runs
    // every image decode in such a worker, held to a memory cap.
    epubkit_core::worker::serve_if_asked();
    if let Ok(exe) = std::env::current_exe() {
        epubkit_core::worker::enable(exe);
    }

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
            commands::check_filename_template,
            commands::find_books,
            commands::inspect_books,
            commands::optimize_books,
            commands::stop_run,
        ])
        // Closed or quit during a run, the window stops the run and goes once
        // it has, so the run takes what it was making with it.
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if commands::stop_before_closing() {
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("epubkit failed to start")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                if commands::stop_before_closing() {
                    api.prevent_exit();
                }
            }
        });
}
