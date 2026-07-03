# A Fast Clipboard

windows clipboard is slow but pretty. this is fast but ugly. 

0 security features weighing it down

text+images, configurable sqlite persistence (100 entries by default)

shift-click multi-paste when you need to dump a few clips/images at once

autostartup by default.

ctrl+alt+v default hotkey, you can disable windows clipboard with registry edit and use win+v
Open Regedit

Navigate to Computer\HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Clipboard

Set IsCloudAndHistoryFeatureAvailable to 0. If this key isn't there, create a DWORD with the name IsCloudAndHistoryFeatureAvailable and set it to 0.
from: https://superuser.com/questions/1439819/disabling-winv-on-windows



clankerslop:


Current scope:

- Text clipboard history
- Image clipboard history with thumbnails
- Compressed full-size image storage
- SQLite persistence in `%LOCALAPPDATA%\AFastClipboard\clips.sqlite3`
- Default retention of 100 unpinned clips
- Pinned clips that are not pruned
- Searchable Slint popup
- Popup opens near the mouse cursor
- Windows tray icon with show/hide/quit menu
- First run opens the picker so users can see what is running
- Per-user Windows startup registration is enabled by default
- Startup registration is repaired automatically if the portable exe moves
- Windows startup launches hidden to tray; normal launches open the picker
- Single-instance guard to avoid duplicate tray apps
- Configurable hotkey, default `Ctrl+Alt+V`, with a simple recorder button
- Copy or paste-on-select behavior
- Shift-click batch selection; releasing Shift pastes selected clips in order when auto-paste has a target, otherwise copies the last selected clip
- Keyboard selection with Enter, Delete, Ctrl+P, and Ctrl+1 through Ctrl+9
- Hotkeys support letters, digits, F1-F24, arrows/navigation, numpad, punctuation, media/browser keys, and raw virtual-key codes like `VK242` or `VK_0xF2`

`Win+V` is reserved by Windows for its own clipboard history, so this app defaults
to `Ctrl+Alt+V`. If PowerToys remaps `Win+V` to another shortcut, set that
shortcut in the app's hotkey field and press Enter.

Run with `cargo run` to open the picker during development. Run with
`cargo run -- --background` to mimic the hidden Windows startup launch.

GitHub Actions runs formatting, check, tests, and clippy on pushes to `main`.
Create and push a version tag such as `v0.1.0` to build a Windows portable exe
and publish a GitHub release.

There is no in-app updater. To update, fully quit the app, download
`a_fast_clipboard.exe` from the latest release, replace the old portable exe,
and run it once so startup registration points at the current path.

The app writes a per-user `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
entry named `AFastClipboard` when Startup is enabled. It stores the current exe
path there, so moving the portable binary is fixed on the next launch.

The app does not edit Windows clipboard-history registry or policy settings. If
someone wants to disable the built-in clipboard manager, that should stay a
manual, clearly documented power-user step.
