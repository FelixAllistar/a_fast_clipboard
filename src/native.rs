#[cfg(windows)]
mod imp {
    use std::ptr::{null, null_mut};
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, RemoveClipboardFormatListener,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyW,
        RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::System::Threading::{
        AttachThreadInput, CreateMutexW, GetCurrentThreadId, ReleaseMutex,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        KEYEVENTF_KEYUP, RegisterHotKey, UnregisterHotKey, keybd_event,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GA_ROOT, GetAncestor, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
        HWND_MESSAGE, IsIconic, IsWindow, IsWindowVisible, MSG, PostThreadMessageW, RegisterClassW,
        SW_SHOW, SetForegroundWindow, ShowWindow, TranslateMessage, WM_APP, WM_CLIPBOARDUPDATE,
        WM_HOTKEY, WNDCLASSW,
    };

    const HOTKEY_ID: i32 = 1;
    const WM_FAST_CLIPBOARD_COMMAND: u32 = WM_APP + 41;

    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_SHIFT: u32 = 0x0004;
    const MOD_WIN: u32 = 0x0008;
    const MOD_NOREPEAT: u32 = 0x4000;

    const VK_CONTROL: u8 = 0x11;
    const VK_SPACE: u32 = 0x20;
    const VK_TAB: u32 = 0x09;
    const VK_ENTER: u32 = 0x0D;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_F1: u32 = 0x70;
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const AUTOSTART_VALUE_NAME: &str = "AFastClipboard";
    const INSTANCE_MUTEX_NAME: &str = r"Local\AFastClipboard.Instance";

    pub struct SingleInstanceGuard {
        handle: HANDLE,
    }

    impl SingleInstanceGuard {
        pub fn acquire() -> Result<Option<Self>> {
            let name = wide_null(INSTANCE_MUTEX_NAME);
            let handle = unsafe { CreateMutexW(null(), 1, name.as_ptr()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error()).context("CreateMutexW failed");
            }

            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    CloseHandle(handle);
                }
                return Ok(None);
            }

            Ok(Some(Self { handle }))
        }
    }

    impl Drop for SingleInstanceGuard {
        fn drop(&mut self) {
            unsafe {
                ReleaseMutex(self.handle);
                CloseHandle(self.handle);
            }
        }
    }

    enum NativeCommand {
        SetHotkey(String),
        Stop,
    }

    #[derive(Clone)]
    pub struct NativeController {
        command_tx: Sender<NativeCommand>,
        thread_id: u32,
    }

    impl NativeController {
        pub fn set_hotkey(&self, hotkey: String) -> Result<()> {
            self.command_tx
                .send(NativeCommand::SetHotkey(hotkey))
                .context("native listener is not running")?;
            wake_thread(self.thread_id)
        }

        pub fn stop(&self) -> Result<()> {
            self.command_tx
                .send(NativeCommand::Stop)
                .context("native listener is not running")?;
            wake_thread(self.thread_id)
        }
    }

    pub fn start_native_listener(
        initial_hotkey: String,
        on_clipboard: Arc<dyn Fn() + Send + Sync + 'static>,
        on_hotkey: Arc<dyn Fn(isize) + Send + Sync + 'static>,
        on_status: Arc<dyn Fn(String) + Send + Sync + 'static>,
    ) -> Result<NativeController> {
        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let status_callback = on_status.clone();

        thread::Builder::new()
            .name("fast-clipboard-native-listener".to_string())
            .spawn(move || {
                if let Err(error) = native_thread_main(
                    initial_hotkey,
                    command_rx,
                    ready_tx,
                    on_clipboard,
                    on_hotkey,
                    on_status,
                ) {
                    status_callback(format!("Native listener stopped: {error:#}"));
                }
            })
            .context("failed to spawn native listener thread")?;

        let thread_id = ready_rx
            .recv_timeout(Duration::from_secs(3))
            .context("native listener did not start")?
            .map_err(|error| anyhow!(error))?;

        Ok(NativeController {
            command_tx,
            thread_id,
        })
    }

    pub fn focus_and_paste(target_hwnd: isize) -> Result<()> {
        if target_hwnd == 0 {
            return Ok(());
        }

        let target = target_hwnd as HWND;
        if unsafe { IsWindow(target) } == 0 {
            return Ok(());
        }
        if unsafe { IsIconic(target) } != 0 {
            return Ok(());
        }

        for _ in 0..8 {
            restore_foreground(target);
            if unsafe { GetForegroundWindow() } == target {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }

        thread::sleep(Duration::from_millis(80));
        send_ctrl_v();
        Ok(())
    }

    pub fn activate_window(hwnd: isize) -> Result<()> {
        if hwnd == 0 {
            return Ok(());
        }

        let target = hwnd as HWND;
        if unsafe { IsWindow(target) } == 0 {
            return Ok(());
        }

        activate_foreground(target);
        Ok(())
    }

    pub fn is_foreground_window(hwnd: isize) -> bool {
        if hwnd == 0 {
            return false;
        }

        unsafe {
            let target = hwnd as HWND;
            if IsWindow(target) == 0 {
                return false;
            }

            let foreground = GetForegroundWindow();
            if foreground.is_null() {
                return false;
            }

            foreground == target || GetAncestor(foreground, GA_ROOT) == GetAncestor(target, GA_ROOT)
        }
    }

    pub fn is_window_minimized(hwnd: isize) -> bool {
        hwnd != 0 && unsafe { IsIconic(hwnd as HWND) != 0 }
    }

    pub fn is_window_visible(hwnd: isize) -> bool {
        hwnd != 0 && unsafe { IsWindowVisible(hwnd as HWND) != 0 }
    }

    pub fn autostart_enabled() -> Result<bool> {
        let subkey = wide_null(RUN_KEY);
        let value_name = wide_null(AUTOSTART_VALUE_NAME);
        let mut key = null_mut();

        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if opened != 0 {
            return Ok(false);
        }

        let found = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            ) == 0
        };
        unsafe {
            RegCloseKey(key);
        }

        Ok(found)
    }

    pub fn set_autostart_enabled(enabled: bool) -> Result<()> {
        if enabled {
            enable_autostart()
        } else {
            disable_autostart()
        }
    }

    fn enable_autostart() -> Result<()> {
        let subkey = wide_null(RUN_KEY);
        let value_name = wide_null(AUTOSTART_VALUE_NAME);
        let command = wide_null(&format!(
            "\"{}\" --background",
            std::env::current_exe()?.display()
        ));
        let mut key = null_mut();

        let created = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, subkey.as_ptr(), &mut key) };
        if created != 0 {
            return Err(win32_error(created).into());
        }

        let data = command.as_ptr().cast::<u8>();
        let set = unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                data,
                (command.len() * 2) as u32,
            )
        };
        unsafe {
            RegCloseKey(key);
        }

        if set != 0 {
            return Err(win32_error(set).into());
        }

        Ok(())
    }

    fn disable_autostart() -> Result<()> {
        let subkey = wide_null(RUN_KEY);
        let value_name = wide_null(AUTOSTART_VALUE_NAME);
        let mut key = null_mut();

        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut key,
            )
        };
        if opened != 0 {
            return Ok(());
        }

        let deleted = unsafe { RegDeleteValueW(key, value_name.as_ptr()) };
        unsafe {
            RegCloseKey(key);
        }

        if deleted != 0 && deleted != 2 {
            return Err(win32_error(deleted).into());
        }

        Ok(())
    }

    fn win32_error(code: u32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code as i32)
    }

    fn native_thread_main(
        initial_hotkey: String,
        command_rx: Receiver<NativeCommand>,
        ready_tx: Sender<Result<u32, String>>,
        on_clipboard: Arc<dyn Fn() + Send + Sync + 'static>,
        on_hotkey: Arc<dyn Fn(isize) + Send + Sync + 'static>,
        on_status: Arc<dyn Fn(String) + Send + Sync + 'static>,
    ) -> Result<()> {
        let setup = setup_message_window();
        let (thread_id, hwnd) = match setup {
            Ok(value) => value,
            Err(error) => {
                let message = format!("{error:#}");
                let _ = ready_tx.send(Err(message.clone()));
                return Err(anyhow!(message));
            }
        };

        let _ = ready_tx.send(Ok(thread_id));

        let mut registered_hotkey = false;
        register_hotkey_spec(&initial_hotkey, &mut registered_hotkey, &on_status);

        let mut keep_running = true;
        while keep_running {
            let mut msg = MSG::default();
            let result = unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) };
            if result == -1 {
                return Err(std::io::Error::last_os_error()).context("GetMessageW failed");
            }
            if result == 0 {
                break;
            }

            match msg.message {
                WM_CLIPBOARDUPDATE => on_clipboard(),
                WM_HOTKEY if msg.wParam == HOTKEY_ID as WPARAM => {
                    let foreground = unsafe { GetForegroundWindow() };
                    on_hotkey(foreground as isize);
                }
                WM_FAST_CLIPBOARD_COMMAND => {
                    keep_running =
                        handle_commands(&command_rx, &mut registered_hotkey, &on_status)?;
                }
                _ => unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                },
            }
        }

        unsafe {
            if registered_hotkey {
                UnregisterHotKey(null_mut(), HOTKEY_ID);
            }
            RemoveClipboardFormatListener(hwnd);
            DestroyWindow(hwnd);
        }

        Ok(())
    }

    fn handle_commands(
        command_rx: &Receiver<NativeCommand>,
        registered_hotkey: &mut bool,
        on_status: &Arc<dyn Fn(String) + Send + Sync + 'static>,
    ) -> Result<bool> {
        while let Ok(command) = command_rx.try_recv() {
            match command {
                NativeCommand::SetHotkey(spec) => {
                    register_hotkey_spec(&spec, registered_hotkey, on_status);
                }
                NativeCommand::Stop => return Ok(false),
            }
        }

        Ok(true)
    }

    fn register_hotkey_spec(
        spec: &str,
        registered_hotkey: &mut bool,
        on_status: &Arc<dyn Fn(String) + Send + Sync + 'static>,
    ) {
        unsafe {
            if *registered_hotkey {
                UnregisterHotKey(null_mut(), HOTKEY_ID);
                *registered_hotkey = false;
            }
        }

        let hotkey = match parse_hotkey(spec) {
            Ok(hotkey) => hotkey,
            Err(error) => {
                on_status(format!("Hotkey not active: {error}"));
                return;
            }
        };

        let result = unsafe {
            RegisterHotKey(
                null_mut(),
                HOTKEY_ID,
                hotkey.modifiers | MOD_NOREPEAT,
                hotkey.vk,
            )
        };

        if result == 0 {
            let error = std::io::Error::last_os_error();
            on_status(format!("Hotkey failed: {} ({error})", hotkey.label));
            return;
        }

        *registered_hotkey = true;
        on_status(format!("Hotkey active: {}", hotkey.label));
    }

    fn setup_message_window() -> Result<(u32, HWND)> {
        let thread_id = unsafe { GetCurrentThreadId() };
        let class_name = wide_null("AFastClipboardMessageWindow");
        let h_instance = unsafe { GetModuleHandleW(null()) };
        if h_instance.is_null() {
            return Err(std::io::Error::last_os_error()).context("GetModuleHandleW failed");
        }

        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: h_instance,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };

        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(std::io::Error::last_os_error()).context("RegisterClassW failed");
        }

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                h_instance,
                null_mut(),
            )
        };

        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error()).context("CreateWindowExW failed");
        }

        if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                DestroyWindow(hwnd);
            }
            return Err(error).context("AddClipboardFormatListener failed");
        }

        Ok((thread_id, hwnd))
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn parse_hotkey(spec: &str) -> Result<ParsedHotkey> {
        let mut modifiers = 0;
        let mut key = None;
        let mut labels = Vec::new();

        for part in spec
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => {
                    modifiers |= MOD_CONTROL;
                    labels.push("Ctrl".to_string());
                }
                "shift" => {
                    modifiers |= MOD_SHIFT;
                    labels.push("Shift".to_string());
                }
                "alt" => {
                    modifiers |= MOD_ALT;
                    labels.push("Alt".to_string());
                }
                "win" | "windows" | "meta" | "super" => {
                    modifiers |= MOD_WIN;
                    labels.push("Win".to_string());
                }
                other => {
                    if key.is_some() {
                        return Err(anyhow!("only one non-modifier key is supported"));
                    }

                    let (vk, label) =
                        parse_key(other).ok_or_else(|| anyhow!("unknown key '{}'", part.trim()))?;
                    key = Some((vk, label));
                }
            }
        }

        let (vk, key_label) = key.ok_or_else(|| anyhow!("missing a key"))?;
        labels.push(key_label);

        Ok(ParsedHotkey {
            modifiers,
            vk,
            label: labels.join("+"),
        })
    }

    fn parse_key(value: &str) -> Option<(u32, String)> {
        if let Some(raw_vk) = parse_raw_vk(value) {
            return Some(raw_vk);
        }

        let compact = compact_key_name(value);
        if let Some(rest) = compact.strip_prefix('f') {
            if let Ok(number) = rest.parse::<u32>() {
                if (1..=24).contains(&number) {
                    return Some((VK_F1 + number - 1, format!("F{number}")));
                }
            }
        }

        if let Some((vk, label)) = named_vk(&compact) {
            return Some((vk, label.to_string()));
        }

        let mut chars = value.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }

        if ch.is_ascii_alphanumeric() {
            let upper = ch.to_ascii_uppercase();
            return Some((upper as u32, upper.to_string()));
        }

        punctuation_vk(ch).map(|(vk, label)| (vk, label.to_string()))
    }

    fn parse_raw_vk(value: &str) -> Option<(u32, String)> {
        let normalized = value.trim().to_ascii_lowercase();
        let value = normalized.as_str();
        let raw = value
            .strip_prefix("vk_")
            .or_else(|| value.strip_prefix("vk:"))
            .or_else(|| value.strip_prefix("vk"))?;
        let raw = raw.trim().trim_start_matches('_').trim_start_matches(':');
        if raw.is_empty() {
            return None;
        }

        let vk = if let Some(hex) = raw.strip_prefix("0x") {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            raw.parse::<u32>().ok()?
        };

        if (1..=0xFE).contains(&vk) {
            Some((vk, format!("VK_0x{vk:02X}")))
        } else {
            None
        }
    }

    fn compact_key_name(value: &str) -> String {
        value
            .chars()
            .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
            .collect::<String>()
    }

    fn named_vk(value: &str) -> Option<(u32, &'static str)> {
        match value {
            "space" | "spacebar" => Some((VK_SPACE, "Space")),
            "tab" | "backtab" => Some((VK_TAB, "Tab")),
            "enter" | "return" => Some((VK_ENTER, "Enter")),
            "esc" | "escape" => Some((VK_ESCAPE, "Esc")),
            "backspace" | "bksp" => Some((0x08, "Backspace")),
            "capslock" => Some((0x14, "CapsLock")),
            "pageup" | "pgup" => Some((0x21, "PageUp")),
            "pagedown" | "pgdn" => Some((0x22, "PageDown")),
            "end" => Some((0x23, "End")),
            "home" => Some((0x24, "Home")),
            "left" | "leftarrow" => Some((0x25, "Left")),
            "up" | "uparrow" => Some((0x26, "Up")),
            "right" | "rightarrow" => Some((0x27, "Right")),
            "down" | "downarrow" => Some((0x28, "Down")),
            "printscreen" | "prtsc" | "snapshot" | "sysreq" => Some((0x2C, "PrintScreen")),
            "insert" | "ins" => Some((0x2D, "Insert")),
            "delete" | "del" => Some((0x2E, "Delete")),
            "apps" | "app" | "menu" | "contextmenu" => Some((0x5D, "Menu")),
            "sleep" => Some((0x5F, "Sleep")),
            "numlock" => Some((0x90, "NumLock")),
            "scrolllock" => Some((0x91, "ScrollLock")),
            "browserback" | "back" => Some((0xA6, "BrowserBack")),
            "browserforward" | "forward" => Some((0xA7, "BrowserForward")),
            "browserrefresh" | "refresh" => Some((0xA8, "BrowserRefresh")),
            "browserstop" | "stop" => Some((0xA9, "BrowserStop")),
            "browsersearch" | "search" => Some((0xAA, "BrowserSearch")),
            "browserfavorites" | "favorites" => Some((0xAB, "BrowserFavorites")),
            "browserhome" => Some((0xAC, "BrowserHome")),
            "volumemute" | "mute" => Some((0xAD, "VolumeMute")),
            "volumedown" => Some((0xAE, "VolumeDown")),
            "volumeup" => Some((0xAF, "VolumeUp")),
            "medianext" | "nexttrack" => Some((0xB0, "MediaNext")),
            "mediaprev" | "prevtrack" | "previoustrack" => Some((0xB1, "MediaPrev")),
            "mediastop" => Some((0xB2, "MediaStop")),
            "mediaplaypause" | "playpause" => Some((0xB3, "MediaPlayPause")),
            "launchmail" | "mail" => Some((0xB4, "LaunchMail")),
            "launchmedia" | "mediaselect" => Some((0xB5, "LaunchMedia")),
            "launchapp1" | "app1" => Some((0xB6, "LaunchApp1")),
            "launchapp2" | "app2" => Some((0xB7, "LaunchApp2")),
            "semicolon" | "colon" => Some((0xBA, "Semicolon")),
            "equals" | "equal" | "plus" => Some((0xBB, "Equals")),
            "comma" | "lessthan" => Some((0xBC, "Comma")),
            "hyphen" | "hyphenminus" | "minus" | "underscore" => Some((0xBD, "Hyphen")),
            "period" | "greaterthan" => Some((0xBE, "Period")),
            "slash" | "questionmark" => Some((0xBF, "Slash")),
            "backquote" | "grave" | "tilde" => Some((0xC0, "BackQuote")),
            "openbracket" | "leftbracket" | "opencurlybracket" => Some((0xDB, "OpenBracket")),
            "backslash" | "pipe" => Some((0xDC, "Backslash")),
            "closebracket" | "rightbracket" | "closecurlybracket" => Some((0xDD, "CloseBracket")),
            "quote" | "apostrophe" | "doublequote" => Some((0xDE, "Quote")),
            _ => parse_numpad_vk(value),
        }
    }

    fn parse_numpad_vk(value: &str) -> Option<(u32, &'static str)> {
        let digit = value
            .strip_prefix("numpad")
            .or_else(|| value.strip_prefix("num"))?;
        match digit {
            "0" => Some((0x60, "Numpad0")),
            "1" => Some((0x61, "Numpad1")),
            "2" => Some((0x62, "Numpad2")),
            "3" => Some((0x63, "Numpad3")),
            "4" => Some((0x64, "Numpad4")),
            "5" => Some((0x65, "Numpad5")),
            "6" => Some((0x66, "Numpad6")),
            "7" => Some((0x67, "Numpad7")),
            "8" => Some((0x68, "Numpad8")),
            "9" => Some((0x69, "Numpad9")),
            "multiply" | "star" => Some((0x6A, "NumpadMultiply")),
            "add" | "plus" => Some((0x6B, "NumpadAdd")),
            "separator" => Some((0x6C, "NumpadSeparator")),
            "subtract" | "minus" => Some((0x6D, "NumpadSubtract")),
            "decimal" | "period" => Some((0x6E, "NumpadDecimal")),
            "divide" | "slash" => Some((0x6F, "NumpadDivide")),
            _ => None,
        }
    }

    fn punctuation_vk(ch: char) -> Option<(u32, &'static str)> {
        match ch {
            ';' | ':' => Some((0xBA, "Semicolon")),
            '=' | '+' => Some((0xBB, "Equals")),
            ',' | '<' => Some((0xBC, "Comma")),
            '-' | '_' => Some((0xBD, "Hyphen")),
            '.' | '>' => Some((0xBE, "Period")),
            '/' | '?' => Some((0xBF, "Slash")),
            '`' | '~' => Some((0xC0, "BackQuote")),
            '[' | '{' => Some((0xDB, "OpenBracket")),
            '\\' | '|' => Some((0xDC, "Backslash")),
            ']' | '}' => Some((0xDD, "CloseBracket")),
            '\'' | '"' => Some((0xDE, "Quote")),
            _ => None,
        }
    }

    fn send_ctrl_v() {
        unsafe {
            keybd_event(VK_CONTROL, 0, 0, 0);
            keybd_event(b'V', 0, 0, 0);
            keybd_event(b'V', 0, KEYEVENTF_KEYUP, 0);
            keybd_event(VK_CONTROL, 0, KEYEVENTF_KEYUP, 0);
        }
    }

    fn restore_foreground(target: HWND) {
        activate_foreground(target);
    }

    fn activate_foreground(target: HWND) {
        unsafe {
            let current_thread = GetCurrentThreadId();
            let foreground = GetForegroundWindow();
            let foreground_thread = if foreground.is_null() {
                0
            } else {
                GetWindowThreadProcessId(foreground, null_mut())
            };
            let target_thread = GetWindowThreadProcessId(target, null_mut());

            let attached_foreground = foreground_thread != 0 && foreground_thread != current_thread;
            let attached_target = target_thread != 0 && target_thread != current_thread;

            if attached_foreground {
                AttachThreadInput(current_thread, foreground_thread, 1);
            }
            if attached_target {
                AttachThreadInput(current_thread, target_thread, 1);
            }

            ShowWindow(target, SW_SHOW);
            BringWindowToTop(target);
            SetForegroundWindow(target);

            if attached_target {
                AttachThreadInput(current_thread, target_thread, 0);
            }
            if attached_foreground {
                AttachThreadInput(current_thread, foreground_thread, 0);
            }
        }
    }

    fn wake_thread(thread_id: u32) -> Result<()> {
        let result = unsafe { PostThreadMessageW(thread_id, WM_FAST_CLIPBOARD_COMMAND, 0, 0) };
        if result == 0 {
            return Err(std::io::Error::last_os_error()).context("PostThreadMessageW failed");
        }

        Ok(())
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct ParsedHotkey {
        modifiers: u32,
        vk: u32,
        label: String,
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::Arc;

    use anyhow::Result;

    #[derive(Clone)]
    pub struct NativeController;

    pub struct SingleInstanceGuard;

    impl SingleInstanceGuard {
        pub fn acquire() -> Result<Option<Self>> {
            Ok(Some(Self))
        }
    }

    impl NativeController {
        pub fn set_hotkey(&self, _hotkey: String) -> Result<()> {
            Ok(())
        }

        pub fn stop(&self) -> Result<()> {
            Ok(())
        }
    }

    pub fn start_native_listener(
        _initial_hotkey: String,
        _on_clipboard: Arc<dyn Fn() + Send + Sync + 'static>,
        _on_hotkey: Arc<dyn Fn(isize) + Send + Sync + 'static>,
        on_status: Arc<dyn Fn(String) + Send + Sync + 'static>,
    ) -> Result<NativeController> {
        on_status("Native clipboard listener is only implemented on Windows".to_string());
        Ok(NativeController)
    }

    pub fn focus_and_paste(_target_hwnd: isize) -> Result<()> {
        Ok(())
    }

    pub fn activate_window(_hwnd: isize) -> Result<()> {
        Ok(())
    }

    pub fn is_foreground_window(_hwnd: isize) -> bool {
        false
    }

    pub fn is_window_minimized(_hwnd: isize) -> bool {
        false
    }

    pub fn is_window_visible(_hwnd: isize) -> bool {
        false
    }

    pub fn autostart_enabled() -> Result<bool> {
        Ok(false)
    }

    pub fn set_autostart_enabled(_enabled: bool) -> Result<()> {
        Ok(())
    }
}

pub use imp::{
    NativeController, SingleInstanceGuard, activate_window, autostart_enabled, focus_and_paste,
    is_foreground_window, is_window_minimized, is_window_visible, set_autostart_enabled,
    start_native_listener,
};
