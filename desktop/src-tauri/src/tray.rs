//! The menu-bar icon: how many agents run, and the way back to the window.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};

const TRAY_ID: &str = "agent-ws";

/// The menu line that counts running agents, kept to change its text.
struct StatusLine(MenuItem<Wry>);

/// Builds the icon. Call once from `setup`.
pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "No agent running", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open agent-ws", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit agent-ws", true, Some("CmdOrCtrl+Q"))?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&status, &separator, &open, &quit])?;
    app.manage(StatusLine(status));
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon())
        .icon_as_template(true)
        .tooltip("agent-ws")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => crate::show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Show how many agents run: next to the icon (macOS) and in the menu.
pub fn update(app: &AppHandle, running: usize) {
    let line = match running {
        0 => "No agent running".to_string(),
        1 => "1 agent running".to_string(),
        n => format!("{n} agents running"),
    };
    if let Some(status) = app.try_state::<StatusLine>() {
        let _ = status.0.set_text(&line);
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_title((running > 0).then(|| running.to_string()));
        let _ = tray.set_tooltip(Some(format!("agent-ws: {line}")));
    }
}

/// A 44×44 template icon (22 pt at 2x): a square with its clone behind it.
fn icon() -> Image<'static> {
    const N: u32 = 44;
    // Inside a rounded rectangle (x0, y0, x1, y1) with corner radius `r`.
    let rounded = |px: f32, py: f32, (x0, y0, x1, y1): (f32, f32, f32, f32), r: f32| {
        let (cx, cy) = (px.clamp(x0 + r, x1 - r), py.clamp(y0 + r, y1 - r));
        (px - cx).powi(2) + (py - cy).powi(2) <= r * r
    };
    let front = (6.0, 15.0, 29.0, 38.0);
    let back = (15.0, 6.0, 38.0, 29.0);
    let covered = |px: f32, py: f32| {
        let back_ring = rounded(px, py, back, 5.0)
            && !rounded(
                px,
                py,
                (back.0 + 3.5, back.1 + 3.5, back.2 - 3.5, back.3 - 3.5),
                2.0,
            );
        // A clear gap around the front square sets it apart from the back.
        let gap = rounded(
            px,
            py,
            (front.0 - 2.5, front.1 - 2.5, front.2 + 2.5, front.3 + 2.5),
            7.0,
        );
        rounded(px, py, front, 5.0) || (back_ring && !gap)
    };
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            // 4×4 samples per pixel for smooth edges.
            let hits = (0..16)
                .filter(|i| {
                    let px = x as f32 + (i % 4) as f32 / 4.0 + 0.125;
                    let py = y as f32 + (i / 4) as f32 / 4.0 + 0.125;
                    covered(px, py)
                })
                .count() as u32;
            rgba.extend_from_slice(&[0, 0, 0, (hits * 255 / 16) as u8]);
        }
    }
    Image::new_owned(rgba, N, N)
}
