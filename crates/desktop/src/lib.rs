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

#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::Manager;

/// The window's own Quit, in the menu.
const QUIT: &str = "quit";

pub fn run() {
    // A copy of this binary launched as a decode worker serves requests and
    // never returns here; it must not reach the window. The parent then runs
    // every image decode in such a worker, held to a memory cap.
    epubkit_core::worker::serve_if_asked();
    if let Ok(exe) = std::env::current_exe() {
        epubkit_core::worker::enable(exe);
    }

    let builder = tauri::Builder::default();
    // Cmd+Q quits through the menu's Quit, which ended the app there and
    // then: tao handles only applicationWillTerminate, so there was no exit
    // request to put off, and a run under way left its files. The menu's Quit
    // is the window's own instead, and asks to exit, as closing the window
    // does.
    #[cfg(target_os = "macos")]
    let builder = builder.menu(menu_with_own_quit);

    builder
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
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if commands::stop_before_closing(window.app_handle()) {
                    api.prevent_close();
                }
            }
        })
        .on_menu_event(|app, event| {
            if event.id().as_ref() == QUIT {
                app.exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("epubkit failed to start")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { api, .. } => {
                if commands::stop_before_closing(app) {
                    api.prevent_exit();
                }
            }
            // Ended with no request to put off, quit from the Dock say, or as
            // the session ends, the app still waits for a run to stop.
            tauri::RunEvent::Exit => commands::stop_before_exiting(),
            _ => {}
        });
}

/// Tauri's default menu, but for its app menu, made again with the window's
/// own Quit where the system's was.
#[cfg(target_os = "macos")]
fn menu_with_own_quit<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<Menu<R>> {
    let name = app.package_info().name.clone();
    let about = AboutMetadata {
        name: Some(name.clone()),
        version: Some(app.package_info().version.to_string()),
        ..Default::default()
    };
    let app_menu = Submenu::with_items(
        app,
        &name,
        true,
        &[
            &PredefinedMenuItem::about(app, None, Some(about))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, QUIT, format!("Quit {name}"), true, Some("CmdOrCtrl+Q"))?,
        ],
    )?;

    let menu = Menu::default(app)?;
    menu.remove_at(0)?;
    menu.insert(&app_menu, 0)?;
    Ok(menu)
}
