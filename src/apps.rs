//! Taskbar-module extras: the right-click menu and the hover preview (live window
//! thumbnails, or a name tooltip for pinned apps that aren't running).
//!
//! Like the power menu, these keep their state outside `State` (see `PowerUi` in main).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, GetWindowTextW, IsIconic, SetWindowLongPtrW,
    WINDOWPLACEMENT, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::{Bar, ContextMenu, MenuItem, PreviewSlot, PreviewWindow, State, Theme, hwnd_of, set_theme, sys};

// ---------------------------------------------------------------- right-click menu

enum Action {
    Launch,
    TogglePin,
    Close,
}

#[derive(Default)]
pub struct AppMenu {
    menu: RefCell<Option<ContextMenu>>,
    hidden_at: Cell<Option<Instant>>,
    /// The app the menu is for, and what each item does.
    target: Cell<usize>,
    actions: RefCell<Vec<Option<Action>>>,
}

fn hide_menu(m: &AppMenu) {
    if let Some(w) = m.menu.borrow().as_ref() {
        if w.window().is_visible() {
            if let Some(h) = hwnd_of(w.window()) {
                sys::stop_outside_watch(h);
            }
            w.set_open(false);
            let _ = w.hide();
            m.hidden_at.set(Some(Instant::now()));
        }
    }
}

fn run_action(state: &Rc<RefCell<State>>, index: usize, action: &Action) {
    match action {
        Action::Launch => state.borrow().taskbar.new_instance(index),
        Action::Close => state.borrow().taskbar.close_windows(index),
        Action::TogglePin => {
            let pins = state.borrow_mut().taskbar.toggle_pin(index);
            if let Some(pins) = pins {
                state.borrow_mut().cfg.taskbar.pinned = Some(pins);
                crate::schedule_save(state);
            }
        }
    }
}

pub fn open_menu(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>, index: usize) {
    hide_preview(state);
    let m = state.borrow().app_menu.clone();
    if m.hidden_at.get().is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
        return;
    }
    let Some(bar_hwnd) = bar.upgrade().and_then(|b| hwnd_of(b.window())) else { return };
    let Some((name, icon, pinned, launchable)) = state.borrow_mut().taskbar.info(index) else { return };
    let running = !state.borrow().taskbar.windows_of(index).is_empty();

    let item = |icon: &str, text: &str, danger: bool| MenuItem { icon: icon.into(), text: text.into(), danger, separator: false };
    let mut items = Vec::new();
    let mut actions = Vec::new();
    if launchable {
        items.push(item("\u{E8A7}", if running { "Open new window" } else { "Open" }, false));
        actions.push(Some(Action::Launch));
    }
    items.push(item(if pinned { "\u{E77A}" } else { "\u{E718}" }, if pinned { "Unpin from bar" } else { "Pin to bar" }, false));
    actions.push(Some(Action::TogglePin));
    if running {
        let n = state.borrow().taskbar.windows_of(index).len();
        items.push(MenuItem { icon: "".into(), text: "".into(), danger: false, separator: true });
        actions.push(None);
        items.push(item("\u{E711}", if n > 1 { "Close all windows" } else { "Close window" }, true));
        actions.push(Some(Action::Close));
    }
    m.target.set(index);
    *m.actions.borrow_mut() = actions;

    if m.menu.borrow().is_none() {
        crate::NEXT_WINDOW.store(crate::WINDOW_POPUP, std::sync::atomic::Ordering::Relaxed);
        let Ok(w) = ContextMenu::new() else { return };
        let (mw, st) = (Rc::downgrade(&m), state.clone());
        w.on_chosen(move |i| {
            let Some(m) = mw.upgrade() else { return };
            hide_menu(&m);
            let index = m.target.get();
            let action = m.actions.borrow_mut().get_mut(i as usize).and_then(Option::take);
            if let Some(action) = action {
                run_action(&st, index, &action);
            }
        });
        let mw = Rc::downgrade(&m);
        w.on_dismiss(move || {
            if let Some(m) = mw.upgrade() {
                hide_menu(&m);
            }
        });
        let mw = Rc::downgrade(&m);
        w.window().on_winit_window_event(move |_, event| {
            if let slint::winit_030::winit::event::WindowEvent::Focused(false) = event {
                if let Some(m) = mw.upgrade() {
                    hide_menu(&m);
                }
            }
            slint::winit_030::EventResult::Propagate
        });
        *m.menu.borrow_mut() = Some(w);
    }

    let menu = m.menu.borrow();
    let Some(w) = menu.as_ref() else { return };
    let (top, border) = {
        let st = state.borrow();
        let pal = st.cfg.theme.palette();
        set_theme(w.global::<Theme>(), &st.cfg, &pal);
        (st.cfg.bar.is_top(), pal.surface_hover)
    };
    w.set_heading(name.into());
    w.set_heading_icon(icon);
    w.set_items(ModelRc::new(VecModel::from(items)));
    w.set_open(false);
    let mw = Rc::downgrade(&m);
    crate::show_popup(
        w,
        bar_hwnd,
        top,
        border,
        Rc::new(move || {
            if let Some(m) = mw.upgrade() {
                hide_menu(&m);
            }
        }),
    );
    let ww = w.as_weak();
    Timer::single_shot(Duration::from_millis(16), move || {
        if let Some(w) = ww.upgrade() {
            w.set_open(true);
        }
    });
}

// ---------------------------------------------------------------- hover preview

const SHOW_DELAY: Duration = Duration::from_millis(450);
const HIDE_DELAY: Duration = Duration::from_millis(250);
const SLOT_W: f32 = 210.0;
const PAD: f32 = 8.0;
const GAP: f32 = 8.0;
const TITLE_H: f32 = 26.0;
const MAX_WINDOWS: usize = 4;

#[derive(Default)]
pub struct Preview {
    window: RefCell<Option<PreviewWindow>>,
    /// App being previewed (-1 = none), and the windows shown in its slots.
    index: Cell<i32>,
    windows: RefCell<Vec<HWND>>,
    thumbs: RefCell<Vec<isize>>,
    over_icon: Cell<bool>,
    over_preview: Cell<bool>,
    show_timer: Timer,
    hide_timer: Timer,
}

fn unregister_thumbs(p: &Preview) {
    for t in p.thumbs.borrow_mut().drain(..) {
        let _ = unsafe { DwmUnregisterThumbnail(t) };
    }
}

pub fn hide_preview(state: &Rc<RefCell<State>>) {
    let p = state.borrow().preview.clone();
    p.show_timer.stop();
    p.hide_timer.stop();
    unregister_thumbs(&p);
    p.index.set(-1);
    if let Some(w) = p.window.borrow().as_ref() {
        w.set_open(false);
        let _ = w.hide();
    }
}

fn schedule_hide(state: &Rc<RefCell<State>>) {
    let p = state.borrow().preview.clone();
    let st = state.clone();
    p.hide_timer.start(TimerMode::SingleShot, HIDE_DELAY, move || {
        let p = st.borrow().preview.clone();
        if !p.over_icon.get() && !p.over_preview.get() {
            hide_preview(&st);
        }
    });
}

/// Hover changes on a taskbar icon. `x` is the icon's center in the bar (logical px).
pub fn app_hover(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>, index: i32, hovered: bool, x: f32) {
    let p = state.borrow().preview.clone();
    p.over_icon.set(hovered);
    if !hovered {
        p.show_timer.stop();
        schedule_hide(state);
        return;
    }
    p.hide_timer.stop();
    let showing = p.window.borrow().as_ref().is_some_and(|w| w.window().is_visible());
    if showing {
        if p.index.get() != index {
            show_preview(state, bar, index, x); // already open: switch right away
        }
        return;
    }
    let (st, b) = (state.clone(), bar.clone());
    p.show_timer.start(TimerMode::SingleShot, SHOW_DELAY, move || {
        let p = st.borrow().preview.clone();
        if p.over_icon.get() {
            show_preview(&st, &b, index, x);
        }
    });
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// Aspect ratio (height / width) of a window, using its restored size when minimized.
fn aspect(hwnd: HWND) -> f32 {
    unsafe {
        let r = if IsIconic(hwnd).as_bool() {
            let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            let _ = GetWindowPlacement(hwnd, &mut wp);
            wp.rcNormalPosition
        } else {
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            r
        };
        let (w, h) = ((r.right - r.left).max(1) as f32, (r.bottom - r.top).max(1) as f32);
        (h / w).clamp(0.4, 1.2)
    }
}

fn create_preview(state: &Rc<RefCell<State>>) -> Option<PreviewWindow> {
    crate::NEXT_WINDOW.store(crate::WINDOW_TOOLTIP, std::sync::atomic::Ordering::Relaxed);
    let w = PreviewWindow::new().ok()?;
    let st = state.clone();
    w.on_hovered(move |over| {
        let p = st.borrow().preview.clone();
        p.over_preview.set(over);
        if over {
            p.hide_timer.stop();
        } else {
            schedule_hide(&st);
        }
    });
    let st = state.clone();
    w.on_activate(move |i| {
        let hwnd = st.borrow().preview.windows.borrow().get(i as usize).copied();
        hide_preview(&st);
        if let Some(hwnd) = hwnd {
            crate::taskbar::focus_window(hwnd);
        }
    });
    let st = state.clone();
    w.on_close_window(move |i| {
        let hwnd = st.borrow().preview.windows.borrow().get(i as usize).copied();
        hide_preview(&st);
        if let Some(hwnd) = hwnd {
            crate::taskbar::close_window(hwnd);
        }
    });
    Some(w)
}

fn show_preview(state: &Rc<RefCell<State>>, bar: &slint::Weak<Bar>, index: i32, x: f32) {
    let Some(bar_hwnd) = bar.upgrade().and_then(|b| hwnd_of(b.window())) else { return };
    let Some((name, icon, _, _)) = state.borrow_mut().taskbar.info(index as usize) else { return };
    let windows: Vec<HWND> = state.borrow().taskbar.windows_of(index as usize).into_iter().take(MAX_WINDOWS).collect();
    let p = state.borrow().preview.clone();
    if p.window.borrow().is_none() {
        let Some(w) = create_preview(state) else { return };
        *p.window.borrow_mut() = Some(w);
    }
    unregister_thumbs(&p);
    p.index.set(index);
    *p.windows.borrow_mut() = windows.clone();

    // Lay out one slot per window, side by side, each as tall as its window's shape needs.
    let heights: Vec<f32> = windows.iter().map(|&h| (SLOT_W * aspect(h)).clamp(90.0, 170.0)).collect();
    let max_h = heights.iter().copied().fold(0.0, f32::max);
    let slots: Vec<PreviewSlot> = windows
        .iter()
        .zip(&heights)
        .enumerate()
        .map(|(i, (&hwnd, &h))| PreviewSlot {
            x: PAD + i as f32 * (SLOT_W + GAP),
            y: PAD,
            w: SLOT_W,
            h,
            title: window_title(hwnd).into(),
            icon: icon.clone(),
            live: !unsafe { IsIconic(hwnd) }.as_bool(),
        })
        .collect();
    let n = windows.len() as f32;
    let (content_w, content_h) = (PAD * 2.0 + n * SLOT_W + (n - 1.0).max(0.0) * GAP, PAD * 2.0 + TITLE_H + max_h);

    let window = p.window.borrow();
    let Some(w) = window.as_ref() else { return };
    let (top, border) = {
        let st = state.borrow();
        let pal = st.cfg.theme.palette();
        set_theme(w.global::<Theme>(), &st.cfg, &pal);
        (st.cfg.bar.is_top(), pal.surface_hover)
    };
    w.set_app_name(name.into());
    w.set_content_w(content_w);
    w.set_content_h(content_h);
    w.set_slots(ModelRc::new(VecModel::from(slots.clone())));
    w.set_open(false);
    let _ = w.show();

    let Some(hwnd) = hwnd_of(w.window()) else { return };
    unsafe {
        // Never takes focus: hovering/clicking it must not deactivate the app you're in.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize);
    }
    crate::appbar::style_floating_window(hwnd, border);
    crate::backdrop::apply_soon(hwnd, true);

    // Centered under the icon, just below the bar (or above a bottom bar), on screen.
    let scale = w.window().scale_factor();
    let size = w.window().size();
    let mut bar_rect = RECT::default();
    let _ = unsafe { GetWindowRect(bar_hwnd, &mut bar_rect) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = unsafe { GetMonitorInfoW(MonitorFromWindow(bar_hwnd, MONITOR_DEFAULTTONEAREST), &mut info) };
    let (pw, ph) = (size.width as i32, size.height as i32);
    let gap = (6.0 * scale) as i32;
    let px = (bar_rect.left + (x * scale) as i32 - pw / 2).clamp(info.rcMonitor.left + gap, (info.rcMonitor.right - pw - gap).max(info.rcMonitor.left));
    let py = if top { bar_rect.bottom + gap } else { bar_rect.top - ph - gap };
    w.window().set_position(slint::PhysicalPosition::new(px, py));

    // Live thumbnails over the slots' picture areas.
    for (slot, &src) in slots.iter().zip(&windows) {
        if !slot.live {
            continue;
        }
        let Ok(thumb) = (unsafe { DwmRegisterThumbnail(hwnd, src) }) else { continue };
        let inset = 4.0;
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY | DWM_TNP_OPACITY,
            rcDestination: RECT {
                left: ((slot.x + inset) * scale) as i32,
                top: ((slot.y + TITLE_H) * scale) as i32,
                right: ((slot.x + slot.w - inset) * scale) as i32,
                bottom: ((slot.y + TITLE_H + slot.h - inset) * scale) as i32,
            },
            opacity: 255,
            fVisible: true.into(),
            fSourceClientAreaOnly: false.into(),
            ..Default::default()
        };
        let _ = unsafe { DwmUpdateThumbnailProperties(thumb, &props) };
        p.thumbs.borrow_mut().push(thumb);
    }

    let ww = w.as_weak();
    Timer::single_shot(Duration::from_millis(16), move || {
        if let Some(w) = ww.upgrade() {
            w.set_open(true);
        }
    });
}
