//! The small "Saved" note shown over the game after the save hotkey (ui/toast.html).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "toast";
const WIDTH: f64 = 220.0;
const HEIGHT: f64 = 56.0;

/// Counts toasts shown, so a closing timer leaves a newer toast alone.
static SHOWN: AtomicU64 = AtomicU64::new(0);

/// Shows `text` at the top center of the main display for 1.5 seconds, without taking focus.
pub fn show(app: &AppHandle, text: &str) {
    let id = SHOWN.fetch_add(1, Ordering::SeqCst) + 1;
    let (mut x, mut y) = (0.0, 40.0);
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let scale = monitor.scale_factor();
        x = monitor.position().x as f64 / scale + (monitor.size().width as f64 / scale - WIDTH) / 2.0;
        y += monitor.position().y as f64 / scale;
    }
    let mut url = tauri::Url::parse("http://x/").unwrap();
    url.query_pairs_mut().append_pair("text", text);
    let query = url.query().unwrap_or_default().to_string();
    // Still showing: put the new text in it. A new window with the same label can't be built until
    // the old one is fully gone.
    if let Some(window) = app.get_webview_window(LABEL) {
        let shown = window.url().and_then(|mut url| {
            url.set_query(Some(&query));
            window.navigate(url)
        });
        if shown.is_ok() {
            close_later(app, id);
            return;
        }
        let _ = window.destroy();
    }
    let page = format!("toast.html?{query}");
    let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App(page.into()))
        .title(text)
        .inner_size(WIDTH, HEIGHT)
        .position(x, y)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        // Never take keyboard focus from the game.
        .focused(false)
        .focusable(false)
        .build();
    match built {
        Ok(window) => {
            if let Ok(hwnd) = window.hwnd() {
                crate::win::exclude_from_capture(hwnd.0); // it sits over the game, so keep it out of clips
            }
        }
        Err(e) => {
            crate::log(&format!("Couldn't show the toast: {e}"));
            return;
        }
    }
    close_later(app, id);
}

/// Closes the toast after 1.5 seconds, unless a newer one has replaced it by then.
fn close_later(app: &AppHandle, id: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        if SHOWN.load(Ordering::SeqCst) == id {
            if let Some(window) = app.get_webview_window(LABEL) {
                let _ = window.destroy();
            }
        }
    });
}
