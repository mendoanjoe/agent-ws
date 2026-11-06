//! agent-ws desktop: the workspaces of every repository in one window and in
//! the menu bar. Everything goes through the agent-ws library, the same code
//! as the command line; nothing shells out to it.

mod commands;
mod monitor;
mod tray;

use tauri::{Manager, WindowEvent};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            commands::repos,
            commands::workspaces,
            commands::status,
            commands::diff,
            commands::run_log,
            commands::land,
            commands::drop_workspace,
            commands::prune,
            commands::open,
        ])
        .setup(|app| {
            tray::init(app.handle())?;
            monitor::start(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the app in the menu bar.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("start agent-ws")
        .run(|app, event| {
            // Clicking the Dock icon brings the window back.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main(app);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

/// Show the main window and bring it to the front.
pub fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}
