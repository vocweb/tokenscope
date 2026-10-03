mod config;
mod limits;
mod model;
mod parser;
mod pricing;
mod store;

use model::Dashboard;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
// Positioner is only used for the non-macOS fallback; macOS positions the
// NSPanel manually (see position_panel).
#[cfg(not(target_os = "macos"))]
use tauri_plugin_positioner::{Position, WindowExt};
// NSPanel: lets the popover float over apps in native fullscreen (a plain
// NSWindow from a background/Accessory app cannot overlay another app's
// fullscreen Space). `get_webview_panel` / `to_panel` come from these traits.
#[cfg(target_os = "macos")]
use tauri_nspanel::{ManagerExt as _, WebviewWindowExt as _};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Rebuild the dashboard (incremental), update the tray's token count, and push
/// the fresh data to the UI so an open popover updates live.
fn refresh(app: &tauri::AppHandle) {
    let dash = parser::build_dashboard();
    if let Some(tray) = app.tray_by_id("main") {
        let label = fmt_tokens_m(dash.today_tokens);
        // macOS shows the label next to the menu-bar icon (set_title). Windows'
        // taskbar tray has no equivalent — set_title is a no-op there — so we
        // surface the same number through the hover tooltip instead, the only
        // text channel Shell_NotifyIcon exposes for a tray icon.
        let _ = tray.set_title(Some(label.clone()));
        let _ = tray.set_tooltip(Some(format!("Tokenscope · today {}", label)));
    }
    let _ = app.emit("dashboard-updated", &dash);
}

// ── Detached window (an ordinary app window) ────────────────────────
// `main` (from tauri.conf.json) is always the menu-bar popover: an NSPanel on
// macOS, a borderless floating window elsewhere, toggled by the tray icon. The
// detached window is a *second*, ordinary decorated window the user opens from
// the tray menu — the two coexist, so a quick look at the popover never costs
// the user the window they were working in.
const MODE_POPOVER: &str = "popover";
const MODE_WINDOW: &str = "window";

/// Window label of the detached window. Built on demand (see
/// ensure_detached_window) so a popover-only user never pays for a second
/// webview.
const DETACHED_LABEL: &str = "dashboard";

/// Whether this launch opens the detached window, carried over from the last
/// session. Read once at startup; the tray checkbox flips the live state after.
static START_DETACHED: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| load_window_mode().as_deref() == Some(MODE_WINDOW));

/// Live detached-window state: whether it is open (drives the macOS activation
/// policy and the tray checkbox) plus the checkbox handle itself, so the
/// window's own close button can untick it.
struct DetachedWindow {
    open: AtomicBool,
    check: std::sync::Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

fn data_dir() -> Option<std::path::PathBuf> {
    let dir = dirs::data_dir()?.join("tokenscope");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

/// `window-mode.json` holds the detached window's last visibility ("window" =
/// open, "popover" = closed), so the next launch restores it.
fn load_window_mode() -> Option<String> {
    data_dir()
        .and_then(|d| std::fs::read_to_string(d.join("window-mode.json")).ok())
        .and_then(|t| serde_json::from_str::<String>(&t).ok())
}

fn save_window_mode(mode: &str) {
    if let Some(dir) = data_dir() {
        if let Ok(t) = serde_json::to_string(&mode) {
            let _ = std::fs::write(dir.join("window-mode.json"), t);
        }
    }
}

/// Remembered geometry for window mode. A plain window that reopens somewhere
/// random (or at the popover's tiny size) doesn't feel like a normal app, and
/// Tauri only persists window state with the window-state plugin — so we store
/// position + size ourselves, next to the other prefs.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct WindowGeom {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

fn load_window_geom() -> Option<WindowGeom> {
    let t = std::fs::read_to_string(data_dir()?.join("window-geom.json")).ok()?;
    serde_json::from_str(&t).ok()
}

fn save_window_geom(win: &tauri::WebviewWindow) {
    let (Ok(pos), Ok(size)) = (win.outer_position(), win.outer_size()) else {
        return;
    };
    let geom = WindowGeom {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
    };
    if let Some(dir) = data_dir() {
        if let Ok(t) = serde_json::to_string(&geom) {
            let _ = std::fs::write(dir.join("window-geom.json"), t);
        }
    }
}

/// Apply the saved geometry to the detached window (no-op on first run, which
/// keeps the builder's default size).
fn restore_window_geom(win: &tauri::WebviewWindow) {
    let Some(g) = load_window_geom() else {
        return;
    };
    let _ = win.set_size(tauri::PhysicalSize::new(g.w, g.h));
    let _ = win.set_position(tauri::PhysicalPosition::new(g.x, g.y));
}

/// Create the detached window on first use. Built here rather than declared in
/// tauri.conf.json so a popover-only user never pays for a second webview; it
/// loads the same bundle as the popover and the frontend renders the shape that
/// matches its window label (see get_window_mode).
fn ensure_detached_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window(DETACHED_LABEL) {
        return Some(w);
    }
    let win = tauri::WebviewWindowBuilder::new(
        app,
        DETACHED_LABEL,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("Tokenscope")
    .inner_size(400.0, 660.0)
    .min_inner_size(360.0, 480.0)
    .decorations(true)
    .shadow(true)
    .always_on_top(false)
    .skip_taskbar(false)
    .resizable(true)
    .visible(false)
    .build()
    .ok()?;
    restore_window_geom(&win);
    // Closing the window hides it — the app keeps living in the menu bar — and
    // set_detached_open unticks the tray item and remembers the geometry.
    let w = win.clone();
    win.on_window_event(move |e| {
        if let WindowEvent::CloseRequested { api, .. } = e {
            api.prevent_close();
            set_detached_open(w.app_handle(), false);
        }
    });
    Some(win)
}

fn is_detached_open(app: &tauri::AppHandle) -> bool {
    app.try_state::<DetachedWindow>()
        .map(|s| s.open.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Open or close the detached window, keeping its mirrors in sync: the tray
/// checkbox, the persisted preference (so the next launch restores it) and the
/// macOS activation policy. The policy only becomes Regular while a real window
/// is on screen, so a pure menu-bar user never gets a Dock icon.
fn set_detached_open(app: &tauri::AppHandle, open: bool) {
    let Some(state) = app.try_state::<DetachedWindow>() else {
        return;
    };

    if open {
        let Some(win) = ensure_detached_window(app) else {
            return;
        };
        // Regular first: an Accessory app can show a window, but it owns no menu
        // bar and gets no Dock icon / Cmd-Tab entry, so the window would not
        // behave like a normal app window.
        #[cfg(target_os = "macos")]
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    } else {
        if let Some(win) = app.get_webview_window(DETACHED_LABEL) {
            save_window_geom(&win);
            let _ = win.hide();
        }
        // Back to a menu-bar–only app while no real window is open.
        #[cfg(target_os = "macos")]
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    }

    state.open.store(open, Ordering::SeqCst);
    if let Ok(g) = state.check.lock() {
        if let Some(item) = g.as_ref() {
            let _ = item.set_checked(open);
        }
    }
    save_window_mode(if open { MODE_WINDOW } else { MODE_POPOVER });
}

// ── Launch-at-login preference ──────────────────────────────────────
// Persisted in the data dir (survives restarts/updates, like the window-mode
// and geometry prefs). The
// on/off toggle lives in the tray's right-click menu; on startup we reconcile
// the OS registration to this preference rather than force-enabling every
// launch (which silently undid a user who had turned autostart off).
fn autostart_pref_path() -> Option<std::path::PathBuf> {
    Some(data_dir()?.join("autostart.json"))
}

fn load_autostart_pref() -> Option<bool> {
    let t = std::fs::read_to_string(autostart_pref_path()?).ok()?;
    serde_json::from_str(&t).ok()
}

fn save_autostart_pref(on: bool) {
    if let Some(p) = autostart_pref_path() {
        if let Ok(t) = serde_json::to_string(&on) {
            let _ = std::fs::write(p, t);
        }
    }
}

/// Bring the OS launch-at-login registration in line with the saved preference,
/// returning the effective preference (used to seed the menu checkbox). First
/// run (no saved pref) defaults to on and records it; thereafter we honor the
/// user's choice and only touch the registration when it actually differs.
fn reconcile_autostart(app: &tauri::AppHandle) -> bool {
    let pref = match load_autostart_pref() {
        Some(p) => p,
        None => {
            save_autostart_pref(true);
            true
        }
    };
    let mgr = app.autolaunch();
    let cur = mgr.is_enabled().unwrap_or(false);
    if pref && !cur {
        let _ = mgr.enable();
    } else if !pref && cur {
        let _ = mgr.disable();
    }
    pref
}

/// Last tray-icon rectangle (physical px: x, y, width, height), captured on tray
/// click. Used to anchor the panel like tauri-plugin-positioner's
/// TrayBottomCenter — but we can't use the positioner itself on a swizzled
/// NSPanel: its calculate_position calls current_monitor().unwrap(), which fails
/// for a hidden/panel window, so positioning silently no-ops (panel stays
/// top-left). We also must add the icon height ourselves (see position_panel).
///
/// On Windows the cached tray rect is used only to pick which monitor the
/// popover opens on (see position_popover_windows); the popover itself is then
/// pinned to that monitor's top-right work-area corner with a small margin.
struct TrayAnchor(std::sync::Mutex<Option<(f64, f64, f64, f64)>>);

/// Timestamp (ms) of the last drag start. The popover hides on focus loss; on
/// Windows `start_dragging` enters the OS move loop which briefly blurs the
/// window, so we ignore the hide for a short window after a drag.
#[cfg(not(target_os = "macos"))]
struct DragGuard(AtomicI64);

/// Start dragging the borderless popover (Windows/Linux). Done via a command
/// (not the JS drag-region) so we can record the drag start and suppress the
/// imminent hide-on-blur. The frontend only calls this once a real drag begins.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn begin_drag(window: tauri::Window) -> Result<(), String> {
    if let Some(g) = window.try_state::<DragGuard>() {
        g.0.store(now_ms(), Ordering::Relaxed);
    }
    window.start_dragging().map_err(|e| e.to_string())
}

/// macOS uses a menu-bar NSPanel that isn't user-draggable, so begin_drag is a
/// no-op there. It's also never invoked (the frontend gates it out) — this just
/// keeps the shared invoke_handler list valid and guarantees zero macOS effect.
#[cfg(target_os = "macos")]
#[tauri::command]
fn begin_drag(_window: tauri::Window) -> Result<(), String> {
    Ok(())
}

/// Anchor the panel under the tray icon, top flush with the menu-bar bottom:
///   x = tray_x + tray_width/2 − window_width/2
///   y = tray_y + tray_height
/// The tray rect's y is the icon *top* (≈ screen top, 0); adding its height
/// lands the panel just below the menu bar. (tauri-plugin-positioner gets away
/// with y = tray_y because macOS auto-constrains a normal window out from under
/// the menu bar — but a floating NSPanel isn't constrained, so we offset it
/// ourselves.) All physical px; no monitor lookup, so it works while hidden.
#[cfg(target_os = "macos")]
fn position_panel(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let Ok(size) = w.outer_size() else {
        return;
    };
    let win_w = size.width as f64;

    if let Some(state) = app.try_state::<TrayAnchor>() {
        if let Some((tx, ty, tw, th)) = *state.0.lock().unwrap() {
            let x = tx + tw / 2.0 - win_w / 2.0;
            let y = ty + th;
            let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
            return;
        }
    }

    // Fallback (e.g. opened from the menu before any tray click): centre near
    // the top of the current monitor.
    if let Ok(Some(monitor)) = w.current_monitor() {
        let mp = monitor.position();
        let ms = monitor.size();
        let x = mp.x as f64 + (ms.width as f64 - win_w) / 2.0;
        let y = mp.y as f64 + 24.0 * monitor.scale_factor();
        let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
    }
}

// ── Popover position memory (Windows/Linux) ─────────────────────────
// The borderless popover can be dragged (a header drag region calls
// startDragging in the frontend); we remember where the user left it and reopen
// there next time, falling back to the default top-right when there's no saved
// position on a connected monitor. macOS uses a menu-bar-anchored NSPanel and
// does not persist a position.
#[cfg(not(target_os = "macos"))]
fn popover_pos_path() -> Option<std::path::PathBuf> {
    let dir = dirs::data_dir()?.join("tokenscope");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("popover_pos.json"))
}

#[cfg(not(target_os = "macos"))]
fn load_popover_pos() -> Option<(i32, i32)> {
    let t = std::fs::read_to_string(popover_pos_path()?).ok()?;
    serde_json::from_str(&t).ok()
}

#[cfg(not(target_os = "macos"))]
fn save_popover_pos(x: i32, y: i32) {
    if let Some(p) = popover_pos_path() {
        if let Ok(t) = serde_json::to_string(&(x, y)) {
            let _ = std::fs::write(p, t);
        }
    }
}

/// Position AND right-size the popover for the monitor it opens on. Reopens at
/// the user's last-dragged spot if it's still on a connected monitor, else pins
/// to the top-right of the tray monitor's work area (margin from the edges).
///
/// Everything is derived from the *intended* logical size × the target monitor's
/// scale — never the window's current physical size — and the size is re-asserted
/// on every open. A borderless window can otherwise get stuck at the previous
/// monitor's physical size after a DPI/monitor change (e.g. unplugging a 175%
/// display drops back to 100% but the window stays oversized until restart);
/// forcing the size here makes it recover on the next open. The monitor is
/// resolved from the cached tray rect -> current -> primary; work_area excludes
/// the taskbar so the margin is clean wherever the taskbar sits.
#[cfg(not(target_os = "macos"))]
fn position_popover_windows(app: &tauri::AppHandle) {
    // Logical size — must match app.windows[0] width/height in tauri.conf.json.
    const POPOVER_W: f64 = 400.0;
    const POPOVER_H: f64 = 660.0;
    const MARGIN: f64 = 12.0; // logical px gap from the screen edges

    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    // Force the intended size at the target monitor's DPI (recovers a stuck size).
    let fit = |scale: f64| {
        let _ = w.set_size(tauri::PhysicalSize::new(
            (POPOVER_W * scale).round() as u32,
            (POPOVER_H * scale).round() as u32,
        ));
    };

    // 1. Reopen at the last position if a point just inside it is still on a
    //    connected monitor (a disconnected/shrunk monitor falls through to the
    //    default rather than opening off-screen).
    if let Some((sx, sy)) = load_popover_pos() {
        if let Ok(Some(m)) = w.monitor_from_point(sx as f64 + 20.0, sy as f64 + 20.0) {
            let _ = w.set_position(tauri::PhysicalPosition::new(sx, sy));
            fit(m.scale_factor());
            return;
        }
    }

    // 2. Default: top-right of the tray monitor's work area.
    //    Prefer the monitor under the tray icon; fall back to current, then primary.
    let anchor = app
        .try_state::<TrayAnchor>()
        .and_then(|s| *s.0.lock().unwrap());
    let monitor = anchor
        .and_then(|(tx, ty, _, _)| w.monitor_from_point(tx, ty).ok().flatten())
        .or_else(|| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());

    if let Some(m) = monitor {
        let area = m.work_area(); // excludes the taskbar
        let scale = m.scale_factor();
        let margin = MARGIN * scale; // keep the visual gap DPI-consistent
        let win_w = POPOVER_W * scale; // intended physical width on this monitor
        let right = area.position.x as f64 + area.size.width as f64;
        let x = right - win_w - margin;
        let y = area.position.y as f64 + margin;
        let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
        fit(scale);
    } else {
        // Couldn't resolve a monitor (rare) → let the positioner place it.
        let _ = w.move_window(Position::TopRight);
    }
}

/// True if our (Accessory) app is currently the frontmost application.
#[cfg(target_os = "macos")]
// The macOS interop below (and in the other `tauri_nspanel` users further down)
// goes through `cocoa`/`objc` 0.2, which both crates now mark deprecated in
// favour of `objc2`. `tauri-nspanel` re-exports them and allows the deprecation
// crate-side itself; moving to objc2 means its 2.1 release, whose panel API
// (PanelBuilder / panel events) is a rewrite of the setup below, not a drop-in.
#[allow(deprecated)]
fn app_is_frontmost() -> bool {
    use tauri_nspanel::cocoa::base::id;
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let proc_info: id = msg_send![class!(NSProcessInfo), processInfo];
        let our_pid: i32 = msg_send![proc_info, processIdentifier];
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let front: id = msg_send![workspace, frontmostApplication];
        if front.is_null() {
            return false;
        }
        let front_pid: i32 = msg_send![front, processIdentifier];
        front_pid == our_pid
    }
}

/// Hide the panel when the user switches Space or activates another app, so it
/// doesn't linger over the new (e.g. fullscreen) Space until the next click.
/// resign-key alone misses pure Space switches because the panel joins all
/// Spaces and can stay key across the transition.
#[cfg(target_os = "macos")]
fn hide_panel_on_context_switch(app: &tauri::AppHandle) {
    if app_is_frontmost() {
        return;
    }
    if let Ok(panel) = app.get_webview_panel("main") {
        if panel.is_visible() {
            panel.order_out(None);
        }
    }
}

/// Register NSWorkspace observers that auto-hide the panel on Space change / app
/// activation (mirrors tauri-nspanel's menu-bar example). The observers live for
/// the whole app lifetime, so the returned tokens are intentionally dropped.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // cocoa/objc 0.2 re-exported by tauri-nspanel — see app_is_frontmost
fn register_panel_autohide(app: &tauri::AppHandle) {
    use std::ffi::CString;
    use tauri_nspanel::block::ConcreteBlock;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let center: id = msg_send![workspace, notificationCenter];
        for name in [
            "NSWorkspaceActiveSpaceDidChangeNotification",
            "NSWorkspaceDidActivateApplicationNotification",
        ] {
            let app = app.clone();
            let block = ConcreteBlock::new(move |_notif: id| {
                hide_panel_on_context_switch(&app);
            });
            let block = block.copy();
            let ns_name: id = msg_send![
                class!(NSString),
                stringWithUTF8String: CString::new(name).unwrap().as_ptr()
            ];
            let _: id = msg_send![
                center,
                addObserverForName: ns_name object: nil queue: nil usingBlock: block
            ];
        }
    }
}

/// Read the user's GLOBAL macOS appearance preference: true when dark mode is on.
/// We read `AppleInterfaceStyle` from NSUserDefaults (present and "Dark" => dark,
/// absent => light) rather than the app's NSApp.effectiveAppearance — an
/// Accessory (menu-bar) app never becomes frontmost, so its effective appearance
/// (and thus the webview's `prefers-color-scheme`) can lag the real system value.
/// The user default reflects the system setting directly, regardless of focus.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // ditto
fn system_is_dark() -> bool {
    use std::ffi::CStr;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key: id = msg_send![
            class!(NSString),
            stringWithUTF8String: c"AppleInterfaceStyle".as_ptr()
        ];
        let val: id = msg_send![defaults, stringForKey: key];
        if val == nil {
            return false;
        }
        let raw: *const std::os::raw::c_char = msg_send![val, UTF8String];
        if raw.is_null() {
            return false;
        }
        CStr::from_ptr(raw)
            .to_string_lossy()
            .eq_ignore_ascii_case("dark")
    }
}

/// Watch for live system dark/light-mode changes and push them to the frontend.
/// `AppleInterfaceThemeChangedNotification` is posted on the DISTRIBUTED
/// notification center the instant the user flips Appearance, and is delivered
/// to every registered app regardless of activation policy or frontmost status —
/// so it works for our hidden, non-activating menu-bar panel where the webview's
/// own `prefers-color-scheme` `change` event does not reliably fire. The observer
/// lives for the whole app lifetime, so the returned token is intentionally
/// dropped (same as register_panel_autohide).
#[cfg(target_os = "macos")]
#[allow(deprecated)] // ditto
fn watch_system_theme(app: &tauri::AppHandle) {
    use std::ffi::CString;
    use tauri_nspanel::block::ConcreteBlock;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let center: id = msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
        let app = app.clone();
        let block = ConcreteBlock::new(move |_notif: id| {
            let _ = app.emit("system-theme", system_is_dark());
        });
        let block = block.copy();
        let ns_name: id = msg_send![
            class!(NSString),
            stringWithUTF8String: CString::new("AppleInterfaceThemeChangedNotification").unwrap().as_ptr()
        ];
        let _: id = msg_send![
            center,
            addObserverForName: ns_name object: nil queue: nil usingBlock: block
        ];
    }
}

/// Show the panel as a popover anchored under the tray icon, and focus it.
/// Always reset the scroll to the top so it doesn't reopen mid-scroll.
fn show_popover(app: &tauri::AppHandle) {
    // On macOS the window is an NSPanel — position it manually, then show()
    // (makes it key and orders it front, incl. over fullscreen Spaces).
    #[cfg(target_os = "macos")]
    {
        position_panel(app);
        if let Ok(panel) = app.get_webview_panel("main") {
            panel.show();
        }
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(w) = app.get_webview_window("main") {
        // Pin the popover to the monitor's top-right corner (see
        // position_popover_windows).
        position_popover_windows(app);
        let _ = w.show();
        let _ = w.set_focus();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(
            "(function(){var e=document.querySelector('.om-scroll');if(e){e.scrollTop=0;}else{window.scrollTo(0,0);}})()",
        );
    }
}

/// The shape the *calling* window should render as: the menu-bar popover
/// ("popover") or the detached normal window ("window"). The frontend uses it
/// to pick an opaque background and to drop the custom popover drag region.
#[tauri::command]
fn get_window_mode(window: tauri::WebviewWindow) -> String {
    if window.label() == DETACHED_LABEL {
        MODE_WINDOW.to_string()
    } else {
        MODE_POPOVER.to_string()
    }
}

#[tauri::command]
async fn get_dashboard(app: tauri::AppHandle) -> Dashboard {
    // build_dashboard does blocking IO (reads/writes the cache, parses logs) and
    // holds the store lock — running it inline would block the command on the
    // async runtime and, with a large cache, stall the UI. Hop to a blocking
    // worker (the 30s refresh thread already runs the same work off the main
    // thread).
    let dash = tauri::async_runtime::spawn_blocking(parser::build_dashboard)
        .await
        .unwrap_or_else(|_| parser::build_dashboard());
    // Sync the tray count to this freshly-fetched value. The panel refetches the
    // instant it opens, while the tray otherwise only refreshes every 30s — so
    // without this the two could disagree for up to 30s during heavy usage.
    if let Some(tray) = app.tray_by_id("main") {
        let label = fmt_tokens_m(dash.today_tokens);
        let _ = tray.set_title(Some(label.clone()));
        // Mirror refresh(): keep the tooltip in sync for Windows, where the
        // title isn't shown next to the icon.
        let _ = tray.set_tooltip(Some(format!("Tokenscope · today {}", label)));
    }
    dash
}

/// Cooldown for manual force-refreshes (the tray "Refresh" item). Price tables
/// change at most a few times a day, so back-to-back clicks inside this window
/// coalesce into one fetch.
const FORCE_COOLDOWN_MS: i64 = 30_000;
static LAST_FORCE_MS: AtomicI64 = AtomicI64::new(0);

/// Off-thread, silent price-table refresh (models.dev + LiteLLM) bypassing the
/// 24h cache, folded into the tray's "Refresh" item. Returns immediately; once
/// the new table is swapped in, refresh() pushes dashboard-updated so an open
/// panel re-prices live, same silent path as the 30s background poll (no
/// loading state, no UI feedback). Throttled to one per FORCE_COOLDOWN_MS via
/// compare_exchange (fixed window, not sliding) so rapid clicks can't spawn
/// concurrent fetches racing on the cache.
fn refresh_pricing_bg(app: &tauri::AppHandle) {
    let now = now_ms();
    loop {
        let prev = LAST_FORCE_MS.load(Ordering::Relaxed);
        if now - prev < FORCE_COOLDOWN_MS {
            return;
        }
        match LAST_FORCE_MS.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(_) => continue,
        }
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        pricing::Pricing::reload_shared(true);
        refresh(&handle);
    });
}

#[tauri::command]
fn refresh_pricing(app: tauri::AppHandle) {
    refresh_pricing_bg(&app);
}

/// Off-thread, forced plan-limits refresh (provider usage endpoints + Oh My Pi
/// snapshots) bypassing the 15-minute gate, folded into the tray's "Refresh"
/// item next to pricing. This is the manual escape hatch for the moment right
/// after a window resets: the cached row still shows the dead window's fill
/// level (e.g. "100% exhausted") and the next background tick is minutes
/// away. Once the new snapshot lands, refresh() pushes dashboard-updated so an
/// open panel re-renders live. Same 30s coalescing as pricing so rapid clicks
/// can't stack concurrent fetches.
fn refresh_limits_bg(app: &tauri::AppHandle) {
    // Own cooldown slot: sharing LAST_FORCE_MS with pricing would let the
    // pricing call above claim the window and silently skip the limits fetch.
    static LAST_LIMITS_FORCE_MS: AtomicI64 = AtomicI64::new(0);
    let now = now_ms();
    loop {
        let prev = LAST_LIMITS_FORCE_MS.load(Ordering::Relaxed);
        if now - prev < FORCE_COOLDOWN_MS {
            return;
        }
        match LAST_LIMITS_FORCE_MS.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(_) => continue,
        }
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        limits::reload_shared(true);
        refresh(&handle);
    });
}

/// Save a full-panel screenshot (a `data:image/png;base64,...` URL captured in
/// the webview) to the user's Desktop as `Tokenscope <date> at <time>.png`.
/// DOM rasterization sidesteps macOS Screen Recording permission entirely.
/// Returns the written file path on success.
#[tauri::command]
fn save_screenshot(data_url: String) -> Result<String, String> {
    use base64::Engine;
    let body = data_url
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(|| "expected a data:image/png;base64,... URL".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| format!("invalid base64: {e}"))?;

    let dir =
        dirs::desktop_dir().ok_or_else(|| "could not resolve the Desktop directory".to_string())?;
    let stamp = chrono::Local::now().format("Tokenscope %Y-%m-%d at %H.%M.%S.png");
    let path = dir.join(stamp.to_string());

    std::fs::write(&path, &bytes).map_err(|e| format!("failed to write file: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// For CLI/example validation against real logs.
pub fn dashboard_json() -> String {
    serde_json::to_string_pretty(&parser::build_dashboard()).unwrap_or_default()
}

/// Load the full price table (local cache, or network on a cold/stale cache)
/// before building a dashboard. The app does this off-thread at startup; a
/// one-shot caller such as `examples/dump.rs` must ask explicitly, or every
/// model the built-in snapshot doesn't cover renders as unpriced.
pub fn load_pricing() {
    pricing::Pricing::reload_shared(false);
}

/// Same idea for the plan-usage windows (limits.rs).
pub fn load_limits() {
    limits::warm();
}

fn fmt_tokens_m(m: f64) -> String {
    // Several agents' usage summed can pass 1000M in a day (Oh My Pi runs an
    // advisor on nearly every step), and a 7-character "3750.93M" doesn't fit a
    // menu-bar item — switch unit instead.
    if m >= 1000.0 {
        format!("{:.2}B", m / 1000.0)
    } else if m >= 1.0 {
        format!("{:.2}M", m)
    } else {
        let k = (m * 1000.0).round() as i64;
        // no usage yet (e.g. just past midnight) — "0K" reads like "OK", so
        // show a clearer idle label instead.
        if k <= 0 {
            "Ready".to_string()
        } else {
            format!("{k}K")
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Leave a trace in the system log if a background thread panics: release
    // builds abort on panic, which otherwise kills the menu-bar process with
    // no explanation (no crash reporter entry for an abort-on-panic helper
    // thread in some configurations). Future "the numbers froze" reports
    // can then be correlated with (or cleared by) a panic line in Console.app.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[tokenscope] PANIC: {info}");
    }));
    // Tracks when the popover was last hidden, so a click on the tray icon
    // while it's open (which first blurs/hides it) doesn't immediately reopen.
    let last_hidden = Arc::new(AtomicI64::new(0));

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        // Must be the FIRST plugin: a second launch (e.g. reinstall/relaunch)
        // hands off to the already-running instance and exits, so the menu bar
        // never shows two icons.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // The plugin runs this on its IPC worker thread, and every window
            // path inside show_popover pokes AppKit (NSPanel order-front / NSWindow
            // order-out), which is main-thread-only — doing it off-thread crashes
            // inside AppKit's NSWMWindowCoordinator (EXC_BREAKPOINT). Hop to the
            // main thread instead.
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || show_popover(&handle));
        }))
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));
    // Registers the WebviewPanelManager state used by `to_panel`/`get_webview_panel`.
    #[cfg(target_os = "macos")]
    {
        builder = builder.plugin(tauri_nspanel::init());
    }

    builder
        .invoke_handler(tauri::generate_handler![
            get_dashboard,
            get_window_mode,
            save_screenshot,
            begin_drag,
            refresh_pricing,
        ])
        .setup(move |app| {
            // Menu-bar–only by default: no Dock icon, runs in the background. A
            // detached window makes it a regular app while it is open — Dock
            // icon, Cmd-Tab, app menu — see set_detached_open, which flips the
            // policy back the moment that window closes.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(if *START_DETACHED {
                tauri::ActivationPolicy::Regular
            } else {
                tauri::ActivationPolicy::Accessory
            });

            // Holds the latest tray-icon rect so show_popover can anchor the panel.
            // Captured in the tray click handler on every platform — see
            // position_panel (macOS, below the icon) and position_popover_windows
            // (Windows/Linux, above the icon).
            app.manage(TrayAnchor(std::sync::Mutex::new(None)));
            // Drag-start timestamp so a drag doesn't hide the popover (non-macOS).
            #[cfg(not(target_os = "macos"))]
            app.manage(DragGuard(AtomicI64::new(0)));

            // Detached-window state. The checkbox handle is filled in once the
            // tray menu below has been built.
            app.manage(DetachedWindow {
                open: AtomicBool::new(false),
                check: std::sync::Mutex::new(None),
            });

            // Reconcile launch-at-login with the user's saved preference. The
            // on/off toggle lives in the tray's right-click menu (built below);
            // we do NOT force-enable on every start, which would undo a manual
            // opt-out. `autostart_on` seeds the menu checkbox.
            let autostart_on = reconcile_autostart(app.handle());

            // `main` is always the popover. On macOS convert it to a
            // non-activating NSPanel so it can float over apps in native
            // fullscreen, and hide it on resign-key (clicking outside / switching
            // apps) like a popover. The detached window is a separate, ordinary
            // window created on demand (see ensure_detached_window).
            #[cfg(target_os = "macos")]
            // see app_is_frontmost: tauri-nspanel still ships the objc/cocoa 0.2 API
            #[allow(deprecated)]
            if let Some(window) = app.get_webview_window("main") {
                use tauri_nspanel::cocoa::appkit::NSWindowCollectionBehavior;
                // NSWindowStyleMaskNonActivatingPanel — receive events without
                // activating (stealing focus from) the frontmost app.
                #[allow(non_upper_case_globals)]
                const NS_NONACTIVATING_PANEL: i32 = 1 << 7;

                let lh = last_hidden.clone();
                let handle = app.handle().clone();
                let delegate = tauri_nspanel::panel_delegate!(TokenscopePanelDelegate {
                    window_did_resign_key
                });
                delegate.set_listener(Box::new(move |name: String| {
                    if name == "window_did_resign_key" {
                        lh.store(now_ms(), Ordering::Relaxed);
                        if let Ok(panel) = handle.get_webview_panel("main") {
                            panel.order_out(None);
                        }
                    }
                }));

                if let Ok(panel) = window.to_panel() {
                    panel.set_level(25); // NSMainMenuWindowLevel (24) + 1
                    panel.set_style_mask(NS_NONACTIVATING_PANEL);
                    // MoveToActiveSpace: the panel relocates onto whatever Space
                    // is active *when shown* — so it appears over a fullscreen app
                    // if you open it there, but it does NOT live on every Space.
                    // (CanJoinAllSpaces + Stationary made it omnipresent and kept
                    // it painted through transitions, so it lingered/ghosted over
                    // a fullscreen Space even after order_out.) FullScreenAuxiliary
                    // is what actually permits coexisting with a fullscreen window.
                    panel.set_collection_behaviour(
                        NSWindowCollectionBehavior::NSWindowCollectionBehaviorMoveToActiveSpace
                            | NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary,
                    );
                    panel.set_delegate(delegate);
                }

                // Also hide on Space change / app activation, not just resign-key.
                register_panel_autohide(app.handle());

                // Follow the system appearance natively (the webview's
                // prefers-color-scheme is unreliable for a hidden, non-activating
                // menu-bar panel). Watch for live changes, and emit the current
                // value once now so the frontend's System mode starts correct even
                // if the webview reported a stale appearance at launch.
                watch_system_theme(app.handle());
                let _ = app.emit("system-theme", system_is_dark());
            }

            // Non-macOS: `main` stays the plain borderless popover — it hides on
            // focus loss. The detached window is a separate, ordinary window
            // created on demand (see ensure_detached_window).
            #[cfg(not(target_os = "macos"))]
            if let Some(win) = app.get_webview_window("main") {
                let w = win.clone();
                let lh = last_hidden.clone();
                win.on_window_event(move |e| match e {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        lh.store(now_ms(), Ordering::Relaxed);
                        if let Ok(p) = w.outer_position() {
                            save_popover_pos(p.x, p.y);
                        }
                        let _ = w.hide();
                    }
                    WindowEvent::Focused(false) => {
                        // A hidden window's "blur" (e.g. the one Windows fires at
                        // startup) carries a meaningless default position — only a
                        // VISIBLE popover the user clicks away from should be saved
                        // and hidden. Without this, startup persisted the OS's
                        // default placement and every open snapped there.
                        if !w.is_visible().unwrap_or(false) {
                            return;
                        }
                        // A title-bar drag momentarily blurs the window (the OS
                        // move loop); don't treat that as a click-away dismiss.
                        let dragging = w
                            .try_state::<DragGuard>()
                            .map(|g| now_ms() - g.0.load(Ordering::Relaxed) < 700)
                            .unwrap_or(false);
                        if dragging {
                            return;
                        }
                        lh.store(now_ms(), Ordering::Relaxed);
                        // Remember where the user left it (dragged or default) so
                        // the next open reuses this spot.
                        if let Ok(p) = w.outer_position() {
                            save_popover_pos(p.x, p.y);
                        }
                        let _ = w.hide();
                    }
                    _ => {}
                });
            }

            // Build the menu-bar tray: app glyph (template icon) + today's tokens.
            let dash = parser::build_dashboard();
            let label = fmt_tokens_m(dash.today_tokens);

            let open_i = MenuItem::with_id(app, "open", "Open Tokenscope", true, None::<&str>)?;
            let refresh_i = MenuItem::with_id(app, "refresh", "Refresh", true, None::<&str>)?;
            // Launch-at-login toggle (a checkbox item). Seeded from the reconciled
            // preference; clicking it flips the OS registration and persists.
            let autostart_i = CheckMenuItem::with_id(
                app,
                "autostart",
                "Launch at Login",
                true,
                autostart_on,
                None::<&str>,
            )?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            // Detached-window toggle. It opens/closes the second, ordinary window
            // live — the menu-bar popover stays available either way — and the
            // state is remembered for the next launch.
            let detached_i = CheckMenuItem::with_id(
                app,
                "open-in-window",
                "Open in Window",
                true,
                *START_DETACHED,
                None::<&str>,
            )?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_i,
                    &refresh_i,
                    &PredefinedMenuItem::separator(app)?,
                    &detached_i,
                    &autostart_i,
                    &PredefinedMenuItem::separator(app)?,
                    &quit_i,
                ],
            )?;

            let lh_tray = last_hidden.clone();
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tauri::include_image!("icons/tray-icon.png"))
                .icon_as_template(false)
                .title(&label)
                .tooltip(format!("Tokenscope · today {}", label))
                .menu(&menu)
                .show_menu_on_left_click(false) // left = toggle panel, right = menu
                .on_tray_icon_event(move |tray, event| {
                    let app = tray.app_handle();
                    tauri_plugin_positioner::on_tray_event(app, &event);
                    // Cache the tray-icon rect (physical px) for panel positioning.
                    // macOS aligns the panel under the menu-bar icon; Windows/Linux
                    // uses it to pick the monitor and pins the popover to that
                    // monitor's top-right — see position_panel / position_popover_windows.
                    if let TrayIconEvent::Click { rect, .. } = &event {
                        if let Some(anchor) = app.try_state::<TrayAnchor>() {
                            let p = rect.position.to_physical::<f64>(1.0);
                            let s = rect.size.to_physical::<f64>(1.0);
                            *anchor.0.lock().unwrap() = Some((p.x, p.y, s.width, s.height));
                        }
                    }
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // if it was just hidden by the blur from this same click, leave it closed
                        let just_hidden = now_ms() - lh_tray.load(Ordering::Relaxed) < 250;
                        #[cfg(target_os = "macos")]
                        {
                            let visible = app
                                .get_webview_panel("main")
                                .map(|p| p.is_visible())
                                .unwrap_or(false);
                            if visible {
                                if let Ok(p) = app.get_webview_panel("main") {
                                    p.order_out(None);
                                }
                            } else if !just_hidden {
                                show_popover(app);
                            }
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            let visible = app
                                .get_webview_window("main")
                                .and_then(|w| w.is_visible().ok())
                                .unwrap_or(false);
                            if visible {
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.hide();
                                }
                            } else if !just_hidden {
                                show_popover(app);
                            }
                        }
                    }
                })
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show_popover(app),
                    "refresh" => {
                        refresh(app);
                        refresh_pricing_bg(app);
                        refresh_limits_bg(app);
                    }
                    "autostart" => {
                        // Flip the OS registration, re-read the real state, mirror
                        // it into the checkbox, and persist the user's choice.
                        let mgr = app.autolaunch();
                        let enabled = mgr.is_enabled().unwrap_or(false);
                        let _ = if enabled { mgr.disable() } else { mgr.enable() };
                        let now_on = mgr.is_enabled().unwrap_or(!enabled);
                        let _ = autostart_i.set_checked(now_on);
                        save_autostart_pref(now_on);
                    }
                    "open-in-window" => set_detached_open(app, !is_detached_open(app)),
                    "quit" => {
                        // Remember the detached window's geometry before leaving,
                        // so the next launch reopens exactly where it was.
                        if let Some(w) = app.get_webview_window(DETACHED_LABEL) {
                            save_window_geom(&w);
                        }
                        app.exit(0)
                    }
                    _ => {}
                })
                .build(app)?;

            // Wire the checkbox handle into the state, then restore the last
            // session's detached window if it was open.
            if let Some(st) = app.try_state::<DetachedWindow>() {
                if let Ok(mut g) = st.check.lock() {
                    *g = Some(detached_i.clone());
                }
            }
            if *START_DETACHED {
                set_detached_open(app.handle(), true);
            }

            // Load prices off the main thread (the fetch can block ~20s on a
            // cold/stale cache) and refresh once a day. build_dashboard reads the
            // memoized copy, so neither JSON parsing nor the network ever runs
            // while the store lock is held.
            std::thread::spawn(|| {
                pricing::Pricing::reload_shared(false);
                loop {
                    std::thread::sleep(Duration::from_secs(24 * 60 * 60));
                    pricing::Pricing::reload_shared(false);
                }
            });

            // Provider-reported plan windows (opencode Go / Zen, plus whatever
            // Oh My Pi's own poller recorded) — fetched off the main thread, since
            // the live call can block and the panel must never wait on it.
            std::thread::spawn(|| {
                limits::reload_shared(false);
                loop {
                    std::thread::sleep(Duration::from_secs(15 * 60));
                    limits::reload_shared(false);
                }
            });

            // Background refresh: keep the tray's token count current and push
            // live updates to an open popover. Cheap thanks to incremental ingest.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                refresh(&handle);
            });

            // Filesystem watcher: reflect a log write within ~1s instead of
            // waiting up to the 30s poll (PRD wants <=5s). Writes land in the
            // agent dirs store::watch_roots() reports (Claude Code, Codex,
            // opencode, Oh My Pi); our own cache lives elsewhere, so this never
            // self-triggers. Debounced so a burst of writes coalesces into one
            // rebuild; the 30s poll above stays as a fallback. (build_dashboard
            // serializes on the store lock, so this and the poll can't race.)
            let roots = store::watch_roots();
            if !roots.is_empty() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    use notify::{RecursiveMode, Watcher};
                    let (tx, rx) = std::sync::mpsc::channel();
                    let mut watcher = match notify::recommended_watcher(
                        move |res: notify::Result<notify::Event>| {
                            if res.is_ok() {
                                let _ = tx.send(());
                            }
                        },
                    ) {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    // watch_roots() creates Claude Code's dir and skips missing
                    // ones, so a failure here means the OS refused the watch —
                    // count what registered rather than aborting the whole
                    // watcher (one bad root would kill live updates everywhere).
                    let mut registered = 0;
                    for root in &roots {
                        if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
                            registered += 1;
                        }
                    }
                    if registered == 0 {
                        return;
                    }
                    // Block for the first change, then drain the burst until quiet.
                    while rx.recv().is_ok() {
                        while rx.recv_timeout(Duration::from_millis(400)).is_ok() {}
                        refresh(&handle);
                    }
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Quitting is the one moment a cache write is worth doing regardless
            // of the checkpoint schedule, so the next launch doesn't re-read
            // everything logged since the last one. `flush` is a no-op when
            // nothing is pending, so handling both events is free.
            match event {
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => parser::flush(),
                _ => {}
            }
            // macOS: clicking the Dock icon (or `open` from the command line) with
            // no visible window should bring a window back. Which one depends on
            // what the user had: the detached window keeps the Dock icon alive, so
            // if it is open it is the one to restore; otherwise the popover.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = event
            {
                if !has_visible_windows {
                    if is_detached_open(app) {
                        set_detached_open(app, true);
                    } else {
                        show_popover(app);
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}
