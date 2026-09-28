# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

massier87shell is a customizable top bar for Windows 11 (in the spirit of Noctalia Shell and DankMaterialShell), written in Rust with Slint UI. It is Windows-only and depends heavily on Win32/WinRT through the `windows` crate.

## Build & run

```sh
cargo build --release          # needs Rust + MSVC C++ Build Tools
cargo run --release            # flags: --settings, --control, --config <path>
```

- **Stop the running bar before building**, or the build fails with "Access is denied" on `massier87shell.exe`: `taskkill //F //IM massier87shell.exe` (Git Bash) or `Stop-Process -Name massier87shell`.
- The app is single-instance (a named mutex), so a second launch exits immediately.
- `--settings` / `--control` open the settings window or control panel at startup. These are the easiest way to screenshot-test UI.
- There are no tests and no lint setup. `cargo check --release` is the quick compile check.
- `build.rs` compiles `ui/app.slint` with the `fluent` style. Slint errors show up as a build-script panic, with file:line in the stderr.
- A new Win32 API usually needs a new `windows` crate feature in `Cargo.toml`. The error is that the item is not found or not cfg-enabled.

## Runtime config

- The user config lives at `%APPDATA%\massier87shell\config.toml`. It is created from `config.default.toml`, which is embedded with `include_str!`.
- It is hot-reloaded: the 1s timer polls the file's mtime.
- The Settings window writes it back through `config::save`, which uses `toml_edit` so user comments survive.
- When adding a config field, you must do all of the following:
  1. Add it to the struct in `config.rs`. Every section is `#[serde(default)]`: a manual `Default` impl is needed when a field's default isn't the type default.
  2. Add a `set(&mut doc, ...)` line in `config::save`.
  3. For a new section, add its name to the table list at the top of `save`, or it gets written as an inline table.
  4. Document it in `config.default.toml` and `README.md`.

## Architecture

**Main thread / state.**
- `main.rs` owns a single `Rc<RefCell<State>>`: config, one `BarWindow` per monitor, the audio/cpu/taskbar/media sources, and handles to the popup windows.
- Two `slint::Timer`s drive everything:
  - `fast_tick` (250 ms): focused window, volume, media, taskbar apps, tray, the open control panel, `winbar::keep_hidden`, stats at the configured interval.
  - The 1s timer: clock, config hot-reload, wallpaper-accent check, AppBar self-heal, `sync_bars` for monitor changes.
- Slow or blocking work runs on background threads that talk back through channels or `Arc<Mutex<..>>`:
  - `control.rs` worker: radios, WLAN, brightness via WMI, Ethernet.
  - `media.rs`: GSMTC.
  - `wallpaper.rs`: WIC decode.
  - `temp.rs`: NVML.
- Callbacks that open windows often run while `State` is borrowed. Hide/close paths therefore take `Weak`/`Rc` handles to the small UI structs (`ControlUi`, `PowerUi`, `TrayUi`), not `State`.

**Windows and themes.**
- Every Slint window has its **own copy** of the `Theme` and `Sys` globals (declared in `ui/bar.slint`).
- `set_theme` / `theme_with_glass` in `main.rs` must be called on each window. The bar gets `[theme] opacity`; everything else gets `panel_opacity`.
- `Theme.glass` is the window's background alpha. Below 1, surfaces are made translucent too. Use `Theme.surface-solid` for dropdowns drawn over content.
- Window kinds are selected via the global `NEXT_WINDOW` (`WINDOW_BAR` / `WINDOW_POPUP` / `WINDOW_TOOLTIP`). Set it right before `X::new()`: the winit attributes hook in `select_backend` reads it (skip-taskbar, always-on-top, no-activate…).
- **The native HWND does not exist right after `window.show()`** (winit creates it lazily). Never style or hook a freshly shown window synchronously:
  - Popups go through `show_popup` → `finish_popup`, which retries until `hwnd_of` succeeds, then applies the DWM corners/border, the blur (`backdrop::apply_soon`), focus, and outside-click closing (`sys::close_on_outside_click`).
- Blur/acrylic (`backdrop.rs`) uses the undocumented `SetWindowCompositionAttribute`. Slint windows on Windows are always created transparent, so an alpha background plus the accent policy is enough, even with the software renderer.

**UI files.**
- `ui/app.slint` re-exports every component/struct Rust uses. New exported structs must be added there.
- Bar modules are a big `if root.name == "<module>"` switch in `ui/bar.slint`. Their data comes from the `Sys` global, and events go through `Sys` callbacks wired in `connect_callbacks` (`main.rs`).
- **Adding a module:**
  1. Add the UI branch in `bar.slint`.
  2. Add the `Sys` properties and callbacks.
  3. Wire it up in `connect_callbacks`.
  4. Add it to `settings::MODULES`.
  5. Add a label and icon in `settings::module_info`.
  6. If it has options: add it to `settings::configurable` and a `root.editing == "<module>"` block on page 5 of `ui/settings.slint`.
- Popups: `control.slint` (quick settings, plus Wi-Fi/Ethernet/Sound pages), `tray.slint`, `power.slint`, `menu.slint` (right-click menus), `preview.slint` (hover thumbnails).
  - Their Rust sides: `panel.rs`, the tray and power code in `main.rs`, and `apps.rs`.
- **Models that refresh while visible must be updated in place.** Keep a persistent `Rc<VecModel>` and diff it with `panel::sync`. Recreating a model every tick rebuilds the elements under the pointer and loses clicks.
- Settings flow:
  - `settings::fill` loads config into the window.
  - UI edits call `changed()` → `settings::read` → `commit`: apply the config to the bars live, then a debounced `schedule_save`.

**Windows shell integration (the fragile parts).**
- `appbar.rs`:
  - The bar reserves screen space as an AppBar (`SHAppBarMessage`) and heals it when Explorer drops it.
  - A floating bar uses a "shield" window over the reserved strip.
  - A focus-sink window stops Windows from activating the auto-hidden taskbar when the last window is minimized.
- `tray.rs`:
  - When the `tray` module is on, the bar hosts its own hidden `Shell_TrayWnd`. It receives `NOTIFYICONDATA` via `WM_COPYDATA` and forwards everything to Explorer's tray.
  - Our own AppBar/`SHAppBarMessage` calls must go through `tray::bypass` so they reach Explorer and not ourselves.
- `winbar.rs`:
  - Hides or shows Explorer's taskbar. The original auto-hide state is restored on exit.
  - The hotkey and the control panel tile flip the saved `hide_windows_taskbar` setting (`take_user_change`, persisted in `fast_tick`). They are not a temporary toggle.
- `autostart.rs`: "Start with Windows" is an at-logon Task Scheduler task (`schtasks /XML`). No admin is needed, and unlike the Run key it has no startup delay. The Run key is only a fallback.
  - Because the bar can then start before Explorer, `main` waits for Explorer's `Shell_TrayWnd` (`sys::wait_for_explorer_taskbar`) before touching the shell.
- `taskbar.rs`: the taskbar-apps module. It enumerates windows, keeps its own pin list, loads exact-pixel icons through `IShellItemImageFactory` (`load_icon`), and does drag-to-move of maximized windows.
- `audio.rs`:
  - Core Audio: master and mic volume, and devices.
  - Setting the default device uses the undocumented `IPolicyConfig` through a raw vtable slot.
  - Per-app volume groups audio sessions by executable path.
- `network.rs`: the WLAN API and `GetAdaptersAddresses`. On Windows 11 24H2+, listing Wi-Fi networks returns ACCESS_DENIED unless Location is on for desktop apps. This is surfaced as `wifi_blocked`.

## Conventions and gotchas

- **Renderer:** the default is `software`: partial redraws, about 30 MB and the lowest CPU. The GPU renderers redraw the whole window every frame. Avoid always-running or per-second animations. All animation durations are `Xms * Theme.motion`, so the animation settings apply.
- **Slint escapes:**
  - Icon glyphs are Segoe Fluent/MDL2 codepoints written as `"\u{E713}"`, and string interpolation is `"\{expr}"`.
  - Never write lines containing these through `sed`/`perl` replacements: `\u` uppercases the next char, and `\{` loses its backslash.
  - Use a proper file edit, then grep `.slint` files for `"{E` to catch damage.
- Don't call `sys::bring_to_front` while the bar holds the mouse (mid-drag). Its `AttachThreadInput` trick drops the bar's mouse capture. To raise a window without activating it, use `SetWindowPos(HWND_TOP, SWP_NOACTIVATE)`, and activate after the button is released (see `taskbar::drag_begin`/`drag_end`).
- Slint components must be declared before they are used in the same file.
- A Slint `id` inside an `if` block can't be referenced from outside that block.
