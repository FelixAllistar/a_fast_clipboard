#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod native;
mod store;
mod updater;

use std::borrow::Cow;
use std::env;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arboard::{Clipboard, Error as ClipboardError, ImageData};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    CloseRequestResponse, ComponentHandle, Image, ModelRc, SharedPixelBuffer, SharedString,
    VecModel,
};

use crate::native::{
    NativeController, SingleInstanceGuard, activate_window, autostart_enabled, focus_and_paste,
    is_foreground_window, is_window_minimized, is_window_visible, set_autostart_enabled,
    start_native_listener, system_prefers_dark,
};
use crate::store::{Clip, Store};
use crate::updater::UpdateInfo;

slint::include_modules!();

struct AppState {
    store: Store,
    clipboard_lock: Arc<Mutex<()>>,
    self_write_events: AtomicUsize,
    show_grace_ticks: AtomicUsize,
    paste_target_hwnd: Arc<Mutex<isize>>,
    native_controller: Mutex<Option<NativeController>>,
    app_hwnd: Arc<Mutex<isize>>,
    clear_confirm_until: Mutex<Option<Instant>>,
    update_info: Mutex<Option<UpdateInfo>>,
    update_busy: AtomicBool,
}

struct StartupSync {
    enabled: bool,
    error: Option<String>,
}

fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if updater::apply_update_from_args(&args)? {
        return Ok(());
    }

    let Some(_single_instance) = SingleInstanceGuard::acquire()? else {
        return Ok(());
    };

    let store = Store::open_default()?;
    let first_run = !store.onboarding_seen()?;
    let startup_sync = sync_startup_registration(&store);
    let app = AppWindow::new().context("failed to create UI")?;
    let state = Arc::new(AppState {
        store,
        clipboard_lock: Arc::new(Mutex::new(())),
        self_write_events: AtomicUsize::new(0),
        show_grace_ticks: AtomicUsize::new(0),
        paste_target_hwnd: Arc::new(Mutex::new(0)),
        native_controller: Mutex::new(None),
        app_hwnd: Arc::new(Mutex::new(0)),
        clear_confirm_until: Mutex::new(None),
        update_info: Mutex::new(None),
        update_busy: AtomicBool::new(false),
    });

    initialize_ui(&app, &state, first_run, &startup_sync)?;
    wire_callbacks(&app, state.clone());
    start_native(&app, state.clone())?;

    let start_hidden = cfg!(windows) && should_start_hidden(first_run);
    if !start_hidden {
        app.show().context("failed to show UI")?;
        remember_app_window(&app, &state);
    }
    start_auto_hide_monitor(&app, state.clone());
    #[cfg(windows)]
    let _tray_icon = setup_tray(&app, state.clone())?;
    if first_run && let Err(error) = state.store.set_onboarding_seen(true) {
        set_status(&app, format!("First-run save failed: {error:#}"));
    }
    if start_hidden {
        app.hide()
            .context("failed to prepare hidden startup window")?;
    }
    slint::run_event_loop_until_quit().context("Slint event loop failed")?;

    if let Some(controller) = state.native_controller.lock().unwrap().as_ref() {
        let _ = controller.stop();
    }

    Ok(())
}

fn initialize_ui(
    app: &AppWindow,
    state: &AppState,
    first_run: bool,
    startup_sync: &StartupSync,
) -> Result<()> {
    app.set_query(SharedString::default());
    app.set_selected_index(0);
    app.set_retention_limit(state.store.retention_limit()? as i32);
    app.set_hotkey_text(state.store.hotkey()?.into());
    app.set_recording_hotkey(false);
    app.set_paste_on_select(state.store.paste_on_select()?);
    app.set_start_with_windows(startup_sync.enabled);
    app.set_quit_confirm_visible(false);
    app.set_settings_visible(false);
    app.set_update_available(false);
    app.set_update_version(SharedString::default());
    app.set_app_version(env!("CARGO_PKG_VERSION").into());
    app.set_update_status("Updates not checked".into());
    apply_theme(app, &state.store.theme_mode()?);
    app.set_onboarding_visible(first_run);
    let status = startup_sync.error.clone().unwrap_or_else(|| {
        if first_run {
            "Startup is on. Set a hotkey, then Hide keeps it in the tray.".to_string()
        } else {
            format!("History: {}", state.store.path().display())
        }
    });
    app.set_status_text(status.into());
    refresh_ui(app, state)?;
    Ok(())
}

fn apply_theme(app: &AppWindow, mode: &str) {
    let mode = normalized_theme_mode(mode);
    app.set_theme_mode(mode.into());
    app.set_dark_mode(match mode {
        "dark" => true,
        "light" => false,
        _ => system_prefers_dark().unwrap_or(false),
    });
}

fn normalized_theme_mode(mode: &str) -> &'static str {
    match mode.trim().to_ascii_lowercase().as_str() {
        "dark" => "dark",
        "light" => "light",
        _ => "system",
    }
}

fn sync_startup_registration(store: &Store) -> StartupSync {
    let mut error = None;
    let preferred = match store.start_with_windows_preference() {
        Ok(Some(value)) => value,
        Ok(None) => {
            if let Err(save_error) = store.set_start_with_windows_preference(true) {
                error = Some(format!("Startup preference save failed: {save_error:#}"));
            }
            true
        }
        Err(load_error) => {
            error = Some(format!("Startup preference load failed: {load_error:#}"));
            true
        }
    };

    if let Err(sync_error) = set_autostart_enabled(preferred) {
        let message = format!("Startup sync failed: {sync_error:#}");
        error = Some(match error {
            Some(existing) => format!("{existing}; {message}"),
            None => message,
        });
    }

    let enabled = autostart_enabled().unwrap_or(preferred && error.is_none());
    StartupSync { enabled, error }
}

fn should_start_hidden(first_run: bool) -> bool {
    !first_run && env::args().any(|arg| arg == "--background" || arg == "--hidden")
}

fn recorded_hotkey_spec(
    key: &SharedString,
    control: bool,
    shift: bool,
    alt: bool,
    meta: bool,
) -> Option<String> {
    let key = recorded_key_label(key)?;

    if !(control || shift || alt || meta) {
        return None;
    }

    let mut parts = Vec::new();
    if control {
        parts.push("Ctrl".to_string());
    }
    if shift {
        parts.push("Shift".to_string());
    }
    if alt {
        parts.push("Alt".to_string());
    }
    if meta {
        parts.push("Win".to_string());
    }
    parts.push(key);

    Some(parts.join("+"))
}

fn recorded_key_label(key: &SharedString) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }

    if let Some(label) = slint_special_key_label(key) {
        return Some(label.to_string());
    }

    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }

    if ch.is_ascii_alphanumeric() {
        return Some(ch.to_ascii_uppercase().to_string());
    }

    punctuation_key_label(ch).map(str::to_string)
}

fn slint_special_key_label(key: &str) -> Option<&'static str> {
    use slint::platform::Key as SlintKey;

    for (candidate, label) in [
        (SlintKey::Backspace, "Backspace"),
        (SlintKey::Tab, "Tab"),
        (SlintKey::Return, "Enter"),
        (SlintKey::Escape, "Esc"),
        (SlintKey::Backtab, "Tab"),
        (SlintKey::Delete, "Delete"),
        (SlintKey::CapsLock, "CapsLock"),
        (SlintKey::Space, "Space"),
        (SlintKey::UpArrow, "Up"),
        (SlintKey::DownArrow, "Down"),
        (SlintKey::LeftArrow, "Left"),
        (SlintKey::RightArrow, "Right"),
        (SlintKey::F1, "F1"),
        (SlintKey::F2, "F2"),
        (SlintKey::F3, "F3"),
        (SlintKey::F4, "F4"),
        (SlintKey::F5, "F5"),
        (SlintKey::F6, "F6"),
        (SlintKey::F7, "F7"),
        (SlintKey::F8, "F8"),
        (SlintKey::F9, "F9"),
        (SlintKey::F10, "F10"),
        (SlintKey::F11, "F11"),
        (SlintKey::F12, "F12"),
        (SlintKey::F13, "F13"),
        (SlintKey::F14, "F14"),
        (SlintKey::F15, "F15"),
        (SlintKey::F16, "F16"),
        (SlintKey::F17, "F17"),
        (SlintKey::F18, "F18"),
        (SlintKey::F19, "F19"),
        (SlintKey::F20, "F20"),
        (SlintKey::F21, "F21"),
        (SlintKey::F22, "F22"),
        (SlintKey::F23, "F23"),
        (SlintKey::F24, "F24"),
        (SlintKey::Insert, "Insert"),
        (SlintKey::Home, "Home"),
        (SlintKey::End, "End"),
        (SlintKey::PageUp, "PageUp"),
        (SlintKey::PageDown, "PageDown"),
        (SlintKey::ScrollLock, "ScrollLock"),
        (SlintKey::Pause, "Pause"),
        (SlintKey::SysReq, "PrintScreen"),
        (SlintKey::Stop, "BrowserStop"),
        (SlintKey::Menu, "Menu"),
        (SlintKey::Back, "BrowserBack"),
        (SlintKey::Semicolon, "Semicolon"),
        (SlintKey::Colon, "Semicolon"),
        (SlintKey::Equals, "Equals"),
        (SlintKey::Plus, "Equals"),
        (SlintKey::Comma, "Comma"),
        (SlintKey::LessThan, "Comma"),
        (SlintKey::HyphenMinus, "Hyphen"),
        (SlintKey::Underscore, "Hyphen"),
        (SlintKey::Period, "Period"),
        (SlintKey::GreaterThan, "Period"),
        (SlintKey::Slash, "Slash"),
        (SlintKey::QuestionMark, "Slash"),
        (SlintKey::BackQuote, "BackQuote"),
        (SlintKey::Tilde, "BackQuote"),
        (SlintKey::OpenBracket, "OpenBracket"),
        (SlintKey::OpenCurlyBracket, "OpenBracket"),
        (SlintKey::BackSlash, "Backslash"),
        (SlintKey::Pipe, "Backslash"),
        (SlintKey::CloseBracket, "CloseBracket"),
        (SlintKey::CloseCurlyBracket, "CloseBracket"),
        (SlintKey::Quote, "Quote"),
        (SlintKey::DoubleQuote, "Quote"),
    ] {
        let candidate_text: SharedString = candidate.into();
        if candidate_text == key {
            return Some(label);
        }
    }

    None
}

fn punctuation_key_label(ch: char) -> Option<&'static str> {
    match ch {
        ';' | ':' => Some("Semicolon"),
        '=' | '+' => Some("Equals"),
        ',' | '<' => Some("Comma"),
        '-' | '_' => Some("Hyphen"),
        '.' | '>' => Some("Period"),
        '/' | '?' => Some("Slash"),
        '`' | '~' => Some("BackQuote"),
        '[' | '{' => Some("OpenBracket"),
        '\\' | '|' => Some("Backslash"),
        ']' | '}' => Some("CloseBracket"),
        '\'' | '"' => Some("Quote"),
        _ => None,
    }
}

fn wire_callbacks(app: &AppWindow, state: Arc<AppState>) {
    let weak = app.as_weak();

    app.window().on_close_requested({
        let weak = weak.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                app.set_settings_visible(false);
                app.set_quit_confirm_visible(true);
            }
            CloseRequestResponse::KeepWindowShown
        }
    });

    app.on_search_changed({
        let weak = weak.clone();
        let state = state.clone();
        move |_| {
            if let Some(app) = weak.upgrade() {
                app.set_selected_index(0);
                set_status(&app, "Ready");
                if let Err(error) = refresh_ui(&app, &state) {
                    set_status(&app, format!("Search failed: {error:#}"));
                }
            }
        }
    });

    app.on_move_selection({
        let weak = weak.clone();
        let state = state.clone();
        move |index| {
            if let Some(app) = weak.upgrade() {
                clamp_selection(&app, &state, index);
            }
        }
    });

    app.on_select_index({
        let weak = weak.clone();
        let state = state.clone();
        move |index| {
            if let Some(app) = weak.upgrade() {
                match clip_id_at_index(&app, &state, index) {
                    Ok(Some(id)) => select_clip(&app, &state, id),
                    Ok(None) => set_status(&app, "No clip selected"),
                    Err(error) => set_status(&app, format!("Selection failed: {error:#}")),
                }
            }
        }
    });

    app.on_select_clip({
        let weak = weak.clone();
        let state = state.clone();
        move |id| {
            if let Some(app) = weak.upgrade() {
                select_clip(&app, &state, id as i64);
            }
        }
    });

    app.on_delete_selected({
        let weak = weak.clone();
        let state = state.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                match clip_id_at_index(&app, &state, app.get_selected_index()) {
                    Ok(Some(id)) => {
                        if let Err(error) = state.store.delete(id) {
                            set_status(&app, format!("Delete failed: {error:#}"));
                            return;
                        }
                        if let Err(error) = refresh_ui(&app, &state) {
                            set_status(&app, format!("Refresh failed: {error:#}"));
                        }
                    }
                    Ok(None) => set_status(&app, "No clip selected"),
                    Err(error) => set_status(&app, format!("Delete failed: {error:#}")),
                }
            }
        }
    });

    app.on_toggle_selected_pin({
        let weak = weak.clone();
        let state = state.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                match clip_id_at_index(&app, &state, app.get_selected_index()) {
                    Ok(Some(id)) => {
                        if let Err(error) = state.store.toggle_pin(id) {
                            set_status(&app, format!("Pin failed: {error:#}"));
                            return;
                        }
                        if let Err(error) = refresh_ui(&app, &state) {
                            set_status(&app, format!("Refresh failed: {error:#}"));
                        }
                    }
                    Ok(None) => set_status(&app, "No clip selected"),
                    Err(error) => set_status(&app, format!("Pin failed: {error:#}")),
                }
            }
        }
    });

    app.on_toggle_pin({
        let weak = weak.clone();
        let state = state.clone();
        move |id| {
            if let Some(app) = weak.upgrade() {
                if let Err(error) = state.store.toggle_pin(id as i64) {
                    set_status(&app, format!("Pin failed: {error:#}"));
                    return;
                }
                if let Err(error) = refresh_ui(&app, &state) {
                    set_status(&app, format!("Refresh failed: {error:#}"));
                }
            }
        }
    });

    app.on_delete_clip({
        let weak = weak.clone();
        let state = state.clone();
        move |id| {
            if let Some(app) = weak.upgrade() {
                if let Err(error) = state.store.delete(id as i64) {
                    set_status(&app, format!("Delete failed: {error:#}"));
                    return;
                }
                if let Err(error) = refresh_ui(&app, &state) {
                    set_status(&app, format!("Refresh failed: {error:#}"));
                }
            }
        }
    });

    app.on_clear_unpinned({
        let weak = weak.clone();
        let state = state.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                let now = Instant::now();
                let confirmed = {
                    let mut clear_confirm_until = state.clear_confirm_until.lock().unwrap();
                    let confirmed = clear_confirm_until
                        .map(|deadline| now <= deadline)
                        .unwrap_or(false);
                    if confirmed {
                        *clear_confirm_until = None;
                    } else {
                        *clear_confirm_until = Some(now + Duration::from_secs(5));
                    }
                    confirmed
                };

                if !confirmed {
                    set_status(&app, "Click Clear again within 5s to delete unpinned clips");
                    return;
                }

                if let Err(error) = state.store.clear_unpinned() {
                    set_status(&app, format!("Clear failed: {error:#}"));
                    return;
                }
                if let Err(error) = refresh_ui(&app, &state) {
                    set_status(&app, format!("Refresh failed: {error:#}"));
                } else {
                    set_status(&app, "Cleared unpinned clips");
                }
            }
        }
    });

    app.on_retention_changed({
        let weak = weak.clone();
        let state = state.clone();
        move |value| {
            if let Some(app) = weak.upgrade() {
                if let Err(error) = state.store.set_retention_limit(value as i64) {
                    set_status(&app, format!("Retention failed: {error:#}"));
                    return;
                }
                if let Err(error) = refresh_ui(&app, &state) {
                    set_status(&app, format!("Refresh failed: {error:#}"));
                } else {
                    set_status(&app, format!("Keeping {} clips", value.clamp(10, 10_000)));
                }
            }
        }
    });

    app.on_hotkey_changed({
        let weak = weak.clone();
        let state = state.clone();
        move |hotkey| {
            if let Some(app) = weak.upgrade() {
                let hotkey = hotkey.trim().to_string();
                if hotkey.is_empty() {
                    set_status(&app, "Hotkey cannot be empty");
                    return;
                }

                apply_hotkey(&app, &state, hotkey);
            }
        }
    });

    app.on_hotkey_recorded({
        let weak = weak.clone();
        let state = state.clone();
        move |key, control, shift, alt, meta| {
            if let Some(app) = weak.upgrade() {
                let Some(hotkey) = recorded_hotkey_spec(&key, control, shift, alt, meta) else {
                    set_status(
                        &app,
                        "Record a key with Ctrl, Alt, Shift, or Win; raw VK_0xNN also works",
                    );
                    return;
                };

                app.set_hotkey_text(hotkey.clone().into());
                apply_hotkey(&app, &state, hotkey);
            }
        }
    });

    app.on_paste_setting_changed({
        let weak = weak.clone();
        let state = state.clone();
        move |enabled| {
            if let Some(app) = weak.upgrade() {
                if let Err(error) = state.store.set_paste_on_select(enabled) {
                    set_status(&app, format!("Paste setting failed: {error:#}"));
                    return;
                }
                set_status(
                    &app,
                    if enabled {
                        "Auto-paste enabled"
                    } else {
                        "Copy only"
                    },
                );
            }
        }
    });

    app.on_theme_setting_changed({
        let weak = weak.clone();
        let state = state.clone();
        move |mode| {
            if let Some(app) = weak.upgrade() {
                let mode = normalized_theme_mode(&mode);
                if let Err(error) = state.store.set_theme_mode(mode) {
                    set_status(&app, format!("Theme setting failed: {error:#}"));
                    return;
                }
                apply_theme(&app, mode);
                set_status(
                    &app,
                    match mode {
                        "dark" => "Theme: dark",
                        "light" => "Theme: light",
                        _ => "Theme: system",
                    },
                );
            }
        }
    });

    app.on_check_update_requested({
        let weak = weak.clone();
        let state = state.clone();
        move || {
            if state.update_busy.swap(true, Ordering::SeqCst) {
                if let Some(app) = weak.upgrade() {
                    app.set_update_status("Update task already running".into());
                }
                return;
            }

            if let Some(app) = weak.upgrade() {
                app.set_update_available(false);
                app.set_update_status("Checking...".into());
            }

            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let result = if cfg!(debug_assertions) {
                    Err(anyhow::anyhow!(
                        "update install is disabled in debug builds"
                    ))
                } else {
                    updater::latest_update()
                };
                let _ = slint::invoke_from_event_loop(move || {
                    state.update_busy.store(false, Ordering::SeqCst);
                    if let Some(app) = weak.upgrade() {
                        match result {
                            Ok(Some(info)) => {
                                app.set_update_available(true);
                                app.set_update_version(info.version.clone().into());
                                app.set_update_status(format!("{} available", info.version).into());
                                *state.update_info.lock().unwrap() = Some(info);
                            }
                            Ok(None) => {
                                app.set_update_available(false);
                                app.set_update_version(SharedString::default());
                                app.set_update_status("Up to date".into());
                                *state.update_info.lock().unwrap() = None;
                            }
                            Err(error) => {
                                app.set_update_available(false);
                                app.set_update_status(
                                    format!("Update check failed: {error:#}").into(),
                                );
                                *state.update_info.lock().unwrap() = None;
                            }
                        }
                    }
                });
            });
        }
    });

    app.on_install_update_requested({
        let weak = weak.clone();
        let state = state.clone();
        move || {
            if state.update_busy.swap(true, Ordering::SeqCst) {
                if let Some(app) = weak.upgrade() {
                    app.set_update_status("Update task already running".into());
                }
                return;
            }

            let info = state.update_info.lock().unwrap().clone();
            let Some(info) = info else {
                state.update_busy.store(false, Ordering::SeqCst);
                if let Some(app) = weak.upgrade() {
                    app.set_update_status("Check for updates first".into());
                }
                return;
            };

            if let Some(app) = weak.upgrade() {
                app.set_update_status(format!("Downloading {}...", info.version).into());
            }

            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || match updater::stage_and_launch_update(&info) {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        state.update_busy.store(false, Ordering::SeqCst);
                        if let Some(app) = weak.upgrade() {
                            app.set_update_status("Restarting to update...".into());
                        }
                        if let Some(controller) = state.native_controller.lock().unwrap().as_ref() {
                            let _ = controller.stop();
                        }
                        let _ = slint::quit_event_loop();
                    });
                }
                Err(error) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        state.update_busy.store(false, Ordering::SeqCst);
                        if let Some(app) = weak.upgrade() {
                            app.set_update_status(format!("Update failed: {error:#}").into());
                        }
                    });
                }
            });
        }
    });

    app.on_startup_setting_changed({
        let weak = weak.clone();
        let state = state.clone();
        move |enabled| {
            if let Some(app) = weak.upgrade() {
                if let Err(error) = state.store.set_start_with_windows_preference(enabled) {
                    app.set_start_with_windows(!enabled);
                    set_status(&app, format!("Startup preference failed: {error:#}"));
                    return;
                }

                match set_autostart_enabled(enabled) {
                    Ok(()) => set_status(
                        &app,
                        if enabled {
                            "Starts with Windows"
                        } else {
                            "Startup disabled"
                        },
                    ),
                    Err(error) => {
                        let _ = state.store.set_start_with_windows_preference(!enabled);
                        app.set_start_with_windows(!enabled);
                        set_status(&app, format!("Startup setting failed: {error:#}"));
                    }
                }
            }
        }
    });

    app.on_hide_requested({
        let weak = weak.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                app.set_settings_visible(false);
                app.set_quit_confirm_visible(false);
                let _ = app.hide();
            }
        }
    });

    app.on_quit_requested({
        let state = state.clone();
        move || {
            if let Some(controller) = state.native_controller.lock().unwrap().as_ref() {
                let _ = controller.stop();
            }
            let _ = slint::quit_event_loop();
        }
    });
}

fn apply_hotkey(app: &AppWindow, state: &AppState, hotkey: String) {
    let previous = state.store.hotkey().unwrap_or_default();
    let controller = state.native_controller.lock().unwrap().clone();
    let Some(controller) = controller else {
        app.set_hotkey_text(previous.into());
        set_status(app, "Native listener is not ready");
        return;
    };

    if let Err(error) = controller.set_hotkey(hotkey.clone()) {
        app.set_hotkey_text(previous.into());
        set_status(app, format!("Hotkey update failed: {error:#}"));
        return;
    }

    if let Err(error) = state.store.set_hotkey(&hotkey) {
        let rollback = controller.set_hotkey(previous.clone());
        app.set_hotkey_text(previous.into());
        match rollback {
            Ok(()) => set_status(
                app,
                format!("Hotkey save failed; restored previous: {error:#}"),
            ),
            Err(rollback_error) => set_status(
                app,
                format!("Hotkey save failed: {error:#}; rollback failed: {rollback_error:#}"),
            ),
        }
    }
}

fn start_native(app: &AppWindow, state: Arc<AppState>) -> Result<()> {
    let weak_for_clipboard = app.as_weak();
    let weak_for_hotkey = app.as_weak();
    let weak_for_status = app.as_weak();

    let clipboard_state = state.clone();
    let on_clipboard = Arc::new(move || match capture_clipboard(&clipboard_state) {
        Ok(true) => {
            let weak = weak_for_clipboard.clone();
            let state = clipboard_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade()
                    && let Err(error) = refresh_ui(&app, &state)
                {
                    set_status(&app, format!("Refresh failed: {error:#}"));
                }
            });
        }
        Ok(false) => {}
        Err(error) => {
            let weak = weak_for_clipboard.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    set_status(&app, format!("Clipboard capture failed: {error:#}"));
                }
            });
        }
    });

    let hotkey_state = state.clone();
    let on_hotkey = Arc::new(move |foreground_hwnd: isize| {
        set_paste_target(&hotkey_state, foreground_hwnd);

        let weak = weak_for_hotkey.clone();
        let state = hotkey_state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                show_palette(&app, &state);
            }
        });
    });

    let on_status = Arc::new(move |message: String| {
        let weak = weak_for_status.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                set_status(&app, message);
            }
        });
    });

    let controller =
        start_native_listener(state.store.hotkey()?, on_clipboard, on_hotkey, on_status)?;
    *state.native_controller.lock().unwrap() = Some(controller);
    Ok(())
}

fn remember_app_window(app: &AppWindow, state: &AppState) {
    if let Some(hwnd) = app_hwnd(app)
        && let Ok(mut app_hwnd) = state.app_hwnd.lock()
    {
        *app_hwnd = hwnd;
    }
}

fn start_auto_hide_monitor(app: &AppWindow, state: Arc<AppState>) {
    let weak = app.as_weak();
    let app_hwnd = state.app_hwnd.clone();

    thread::spawn(move || {
        let mut seen_foreground = false;
        let mut away_ticks = 0;

        loop {
            thread::sleep(Duration::from_millis(80));

            let hwnd = app_hwnd.lock().map(|guard| *guard).unwrap_or_default();
            if hwnd == 0 {
                request_app_hwnd_refresh(&weak, &app_hwnd);
                continue;
            }

            if !is_window_visible(hwnd) {
                seen_foreground = false;
                away_ticks = 0;
                continue;
            }

            if is_window_minimized(hwnd) {
                seen_foreground = false;
                away_ticks = 0;
                hide_app_window(&weak);
                continue;
            }

            if consume_show_grace_tick(&state) {
                continue;
            }

            if is_foreground_window(hwnd) {
                seen_foreground = true;
                away_ticks = 0;
                continue;
            }

            away_ticks += 1;
            let threshold = if seen_foreground { 2 } else { 8 };
            if away_ticks >= threshold {
                seen_foreground = false;
                away_ticks = 0;
                if !hide_app_window(&weak) {
                    break;
                }
            }
        }
    });
}

fn consume_show_grace_tick(state: &AppState) -> bool {
    state
        .show_grace_ticks
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            value.checked_sub(1)
        })
        .is_ok()
}

fn request_app_hwnd_refresh(weak: &slint::Weak<AppWindow>, app_hwnd: &Arc<Mutex<isize>>) {
    let weak = weak.clone();
    let app_hwnd = app_hwnd.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade()
            && let Some(hwnd) = app_hwnd_from_window(&app)
            && let Ok(mut app_hwnd) = app_hwnd.lock()
        {
            *app_hwnd = hwnd;
        }
    });
}

fn hide_app_window(weak: &slint::Weak<AppWindow>) -> bool {
    let weak = weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            app.set_settings_visible(false);
            app.set_quit_confirm_visible(false);
            let _ = app.hide();
        }
    })
    .is_ok()
}

fn app_hwnd(app: &AppWindow) -> Option<isize> {
    app_hwnd_from_window(app)
}

fn app_hwnd_from_window(app: &AppWindow) -> Option<isize> {
    let handle = app.window().window_handle();
    let raw = handle.window_handle().ok()?.as_raw();

    match raw {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

fn capture_clipboard(state: &AppState) -> Result<bool> {
    let _guard = state.clipboard_lock.lock().unwrap();

    if should_skip_self_write(state) {
        return Ok(false);
    }

    let image_error = match read_clipboard_image_with_retry() {
        Ok(Some(image)) => return Ok(state.store.upsert_image(&image)?.is_some()),
        Ok(None) => None,
        Err(error) => Some(error),
    };

    if let Some(text) = read_clipboard_text_with_retry()? {
        return Ok(state.store.upsert_text(&text)?.is_some());
    }

    if let Some(error) = image_error {
        return Err(error);
    }

    Ok(false)
}

fn read_clipboard_text_with_retry() -> Result<Option<String>> {
    let mut attempts = 0;

    loop {
        attempts += 1;
        let mut clipboard = match Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(ClipboardError::ClipboardOccupied) if attempts < 6 => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(error) => return Err(error).context("failed to open clipboard"),
        };

        match clipboard.get_text() {
            Ok(text) => return Ok(Some(text)),
            Err(ClipboardError::ContentNotAvailable) => return Ok(None),
            Err(ClipboardError::ClipboardOccupied) if attempts < 6 => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("failed to read clipboard text"),
        }
    }
}

fn read_clipboard_image_with_retry() -> Result<Option<ImageData<'static>>> {
    let mut attempts = 0;

    loop {
        attempts += 1;
        let mut clipboard = match Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(ClipboardError::ClipboardOccupied) if attempts < 6 => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(error) => return Err(error).context("failed to open clipboard"),
        };

        match clipboard.get_image() {
            Ok(image) => return Ok(Some(image.to_owned_img())),
            Err(ClipboardError::ContentNotAvailable) => return Ok(None),
            Err(ClipboardError::ClipboardOccupied) if attempts < 6 => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("failed to read clipboard image"),
        }
    }
}

fn show_palette(app: &AppWindow, state: &AppState) {
    state.show_grace_ticks.store(12, Ordering::SeqCst);
    app.set_settings_visible(false);
    app.set_quit_confirm_visible(false);
    app.set_query(SharedString::default());
    app.set_selected_index(0);
    if let Err(error) = refresh_ui(app, state) {
        set_status(app, format!("Refresh failed: {error:#}"));
    }

    let _ = app.show();
    remember_app_window(app, state);
    if let Some(hwnd) = app_hwnd(app) {
        let _ = activate_window(hwnd);
    }
}

fn select_clip(app: &AppWindow, state: &AppState, id: i64) {
    let clip = match state.store.get(id) {
        Ok(Some(clip)) => clip,
        Ok(None) => {
            set_status(app, "Clip no longer exists");
            return;
        }
        Err(error) => {
            set_status(app, format!("Load failed: {error:#}"));
            return;
        }
    };

    if let Err(error) = write_clipboard_clip(state, &clip) {
        set_status(app, format!("Copy failed: {error:#}"));
        return;
    }

    if let Err(error) = state.store.mark_used(id) {
        set_status(app, format!("Usage update failed: {error:#}"));
    }

    let paste = state.store.paste_on_select().unwrap_or(true);
    let target_hwnd = state
        .paste_target_hwnd
        .lock()
        .map(|guard| *guard)
        .unwrap_or_default();

    if paste && target_hwnd != 0 {
        let _ = app.hide();
        let weak = app.as_weak();
        thread::spawn(move || {
            if let Err(error) = focus_and_paste(target_hwnd) {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        let _ = app.show();
                        set_status(&app, format!("Paste skipped: {error:#}"));
                    }
                });
            }
        });
    } else {
        set_status(
            app,
            if paste {
                "Copied; no target window"
            } else {
                "Copied"
            },
        );
        if let Err(error) = refresh_ui(app, state) {
            set_status(app, format!("Refresh failed: {error:#}"));
        }
    }
}

fn write_clipboard_text(state: &AppState, text: &str) -> Result<()> {
    let _guard = state.clipboard_lock.lock().unwrap();
    let mut clipboard = Clipboard::new().context("failed to open clipboard")?;
    mark_self_write(state);
    clipboard
        .set_text(text.to_string())
        .inspect_err(|_| {
            cancel_self_write(state);
        })
        .context("failed to write clipboard")
}

fn write_clipboard_clip(state: &AppState, clip: &Clip) -> Result<()> {
    if clip.kind == "image" {
        let width = clip.image_width.context("image clip is missing width")? as usize;
        let height = clip.image_height.context("image clip is missing height")? as usize;
        let bytes = decode_image_bytes(clip)?;

        let _guard = state.clipboard_lock.lock().unwrap();
        let mut clipboard = Clipboard::new().context("failed to open clipboard")?;
        mark_self_write(state);
        clipboard
            .set_image(ImageData {
                width,
                height,
                bytes: Cow::Owned(bytes),
            })
            .inspect_err(|_| {
                cancel_self_write(state);
            })
            .context("failed to write image clipboard")
    } else {
        write_clipboard_text(state, &clip.text)
    }
}

fn refresh_ui(app: &AppWindow, state: &AppState) -> Result<()> {
    let query = app.get_query().to_string();
    let clips = state.store.list(&query, 250)?;
    let selected_index = clamp_index(app.get_selected_index(), clips.len());

    let rows = clips
        .iter()
        .map(|clip| ClipRow {
            id: clip.id as i32,
            kind: clip.kind.clone().into(),
            preview: preview_text(&clip.text).into(),
            subtitle: subtitle_text(clip).into(),
            pinned: clip.pinned,
            has_thumbnail: clip.thumbnail_bytes.is_some(),
            thumbnail: clip_thumbnail(clip),
        })
        .collect::<Vec<_>>();

    app.set_selected_index(selected_index);
    app.set_count_text(format!("{} saved", state.store.count()?).into());
    app.set_clips(ModelRc::from(Rc::new(VecModel::from(rows))));
    Ok(())
}

fn clip_id_at_index(app: &AppWindow, state: &AppState, index: i32) -> Result<Option<i64>> {
    let query = app.get_query().to_string();
    let clips = state.store.list(&query, 250)?;
    let clamped = clamp_index(index, clips.len());
    app.set_selected_index(clamped);
    Ok(clips.get(clamped as usize).map(|clip| clip.id))
}

fn clamp_selection(app: &AppWindow, state: &AppState, index: i32) {
    let query = app.get_query();
    let len = state
        .store
        .list(query.as_ref(), 250)
        .map(|clips| clips.len())
        .unwrap_or_default();
    app.set_selected_index(clamp_index(index, len));
}

fn clamp_index(index: i32, len: usize) -> i32 {
    if len == 0 {
        0
    } else {
        index.clamp(0, len.saturating_sub(1) as i32)
    }
}

fn set_status(app: &AppWindow, message: impl Into<SharedString>) {
    app.set_status_text(message.into());
}

fn mark_self_write(state: &AppState) {
    state.self_write_events.fetch_add(1, Ordering::SeqCst);
}

fn cancel_self_write(state: &AppState) {
    let _ = state
        .self_write_events
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            value.checked_sub(1)
        });
}

fn should_skip_self_write(state: &AppState) -> bool {
    state
        .self_write_events
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            value.checked_sub(1)
        })
        .is_ok()
}

fn decode_image_bytes(clip: &Clip) -> Result<Vec<u8>> {
    let bytes = clip
        .image_bytes
        .as_ref()
        .context("image clip is missing image data")?;

    match clip.image_encoding.as_str() {
        "rgba" => Ok(bytes.clone()),
        "lz4-rgba" => lz4_flex::decompress_size_prepended(bytes)
            .context("failed to decompress image clipboard data"),
        other => anyhow::bail!("unsupported image encoding: {other}"),
    }
}

fn set_paste_target(state: &AppState, hwnd: isize) {
    let app_hwnd = state
        .app_hwnd
        .lock()
        .map(|guard| *guard)
        .unwrap_or_default();
    let target = if hwnd != 0 && hwnd != app_hwnd {
        hwnd
    } else {
        0
    };

    if let Ok(mut paste_target) = state.paste_target_hwnd.lock() {
        *paste_target = target;
    }
}

fn clear_paste_target(state: &AppState) {
    if let Ok(mut paste_target) = state.paste_target_hwnd.lock() {
        *paste_target = 0;
    }
}

fn clip_thumbnail(clip: &Clip) -> Image {
    let Some(bytes) = clip.thumbnail_bytes.as_ref() else {
        return Image::default();
    };
    let width = clip.thumbnail_width.unwrap_or_default();
    let height = clip.thumbnail_height.unwrap_or_default();
    if width <= 0 || height <= 0 {
        return Image::default();
    }

    let width = width as u32;
    let height = height as u32;
    if bytes.len() < width as usize * height as usize * 4 {
        return Image::default();
    }

    let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(bytes, width, height);
    Image::from_rgba8(buffer)
}

#[cfg(windows)]
fn setup_tray(app: &AppWindow, state: Arc<AppState>) -> Result<tray_icon::TrayIcon> {
    use tray_icon::{
        MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuId, MenuItem},
    };

    let show_id = MenuId::new("show");
    let hide_id = MenuId::new("hide");
    let quit_id = MenuId::new("quit");

    let tray_menu = Menu::new();
    let show_item = MenuItem::with_id(show_id.clone(), "Show", true, None);
    let hide_item = MenuItem::with_id(hide_id.clone(), "Hide", true, None);
    let quit_item = MenuItem::with_id(quit_id.clone(), "Quit", true, None);
    tray_menu
        .append_items(&[&show_item, &hide_item, &quit_item])
        .context("failed to build tray menu")?;

    let weak_for_menu = app.as_weak();
    let state_for_menu = state.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id;
        let weak = weak_for_menu.clone();
        let state = state_for_menu.clone();
        let show_id = show_id.clone();
        let hide_id = hide_id.clone();
        let quit_id = quit_id.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                if id == show_id {
                    clear_paste_target(&state);
                    show_palette(&app, &state);
                } else if id == hide_id {
                    app.set_settings_visible(false);
                    app.set_quit_confirm_visible(false);
                    let _ = app.hide();
                } else if id == quit_id {
                    if let Some(controller) = state.native_controller.lock().unwrap().as_ref() {
                        let _ = controller.stop();
                    }
                    let _ = slint::quit_event_loop();
                }
            }
        });
    }));

    let weak_for_tray = app.as_weak();
    let state_for_tray = state.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let should_show = matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            }
        );
        if !should_show {
            return;
        }

        let weak = weak_for_tray.clone();
        let state = state_for_tray.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                clear_paste_target(&state);
                show_palette(&app, &state);
            }
        });
    }));

    let icon = tray_icon_pixels()?;
    TrayIconBuilder::new()
        .with_id("fast-clipboard")
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Fast Clipboard")
        .with_icon(icon)
        .build()
        .context("failed to create tray icon")
}

#[cfg(windows)]
fn tray_icon_pixels() -> Result<tray_icon::Icon> {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let index = ((y * size + x) * 4) as usize;
            let inside = (4..28).contains(&x) && (3..29).contains(&y);
            let tab = (9..23).contains(&x) && (1..7).contains(&y);
            let mark = (9..23).contains(&x) && (13..17).contains(&y)
                || (13..17).contains(&x) && (9..21).contains(&y);

            let color = if mark {
                [255, 255, 255, 255]
            } else if tab {
                [43, 119, 230, 255]
            } else if inside {
                [31, 99, 201, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba[index..index + 4].copy_from_slice(&color);
        }
    }

    tray_icon::Icon::from_rgba(rgba, size, size).context("failed to create tray icon pixels")
}

fn preview_text(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flattened, 150)
}

fn subtitle_text(clip: &Clip) -> String {
    let age = age_label(clip.created_at);
    let chars = clip.text.chars().count();
    let size = if clip.kind == "image" {
        match (clip.image_width, clip.image_height) {
            (Some(width), Some(height)) => format!("{width}x{height}"),
            _ => "image".to_string(),
        }
    } else {
        format!("{chars} chars")
    };

    match (clip.use_count, clip.last_used_at) {
        (0, _) => format!("{age} | {size}"),
        (count, Some(last_used)) => {
            format!("{age} | used {count}x | last {}", age_label(last_used))
        }
        (count, None) => format!("{age} | used {count}x | {size}"),
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            output.push_str("...");
            return output;
        }
        output.push(ch);
    }
    output
}

fn age_label(timestamp_millis: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let seconds = ((now - timestamp_millis) / 1000).max(0);

    if seconds < 60 {
        "now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}
