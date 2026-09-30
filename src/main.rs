//! Brightness Tray: a tray icon that pops up a brightness slider per display.
//! Displays without hardware brightness control are dimmed with a
//! click-through overlay window instead.
//!
//! When idle the process is blocked in `GetMessageW` (UI thread) and `recv()`
//! (worker thread), holds no monitor/WMI handles, runs under EcoQoS and has
//! its working set trimmed.

#![windows_subsystem = "windows"]

mod backend;
mod gfx;
mod shortcuts;

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::Sender;

use backend::{Cmd, Row};
use gfx::Canvas;
use shortcuts::{Hotkeys, Settings, HK_DOWN, HK_UP, WM_SETTINGS};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_ROWS: u32 = WM_APP + 2;
const WM_SHOW: u32 = WM_APP + 3;
const WM_SHORTCUTS: u32 = WM_APP + 5; // WM_APP + 4 is shortcuts::WM_SETTINGS
const WM_MOUSELEAVE: u32 = 0x02A3;
const ID_AUTOSTART: usize = 1;
const ID_EXIT: usize = 2;
const ID_SHORTCUTS: usize = 3;
const TIMER_TRAY: usize = 1;
const TIMER_OSD: usize = 2;
const TIMER_RELEASE: usize = 3;
/// How long the popup stays up after a keyboard shortcut.
const OSD_MS: u32 = 1500;
/// After a shortcut used without the popup, how long to keep the monitor
/// handles open for the next press before releasing them.
const RELEASE_MS: u32 = 3000;

const CLASS: PCWSTR = w!("BrightnessTray.Popup");
const DIM_CLASS: PCWSTR = w!("BrightnessTray.Dim");
/// Overlay opacity at 0 % software brightness (out of 255), so the screen
/// never goes fully black.
const DIM_MAX_ALPHA: u32 = 220;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_NAME: PCWSTR = w!("BrightnessTray");
const BACKGROUND_ARG: &str = "--background";
const SHORTCUTS_ARG: &str = "--shortcuts";

// Layout, in DIPs.
const WIDTH: f32 = 360.0;
const PAD_X: f32 = 20.0;
const PAD_TOP: f32 = 14.0;
const ROW_H: f32 = 56.0;
const WHEEL_STEP: i32 = 2;

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

struct Theme {
    dark: bool,
    bg: u32,
    text: u32,
    sub: u32,
    track: u32,
    thumb: u32,
    thumb_edge: u32,
    accent: u32,
}

impl Theme {
    fn load() -> Self {
        let personalize = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let dark = match std::env::var("BRIGHTNESS_TRAY_THEME").as_deref() {
            Ok("light") => false,
            Ok("dark") => true,
            _ => reg_dword(personalize, w!("SystemUsesLightTheme")) == Some(0),
        };
        // AccentPalette holds 8 RGBA swatches, light3 → dark3. Windows 11 uses
        // light2 for controls in dark mode and dark1 in light mode.
        let mut pal = [0u8; 32];
        let accent = if reg_binary(w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent"), w!("AccentPalette"), &mut pal) {
            let i = if dark { 4 } else { 16 };
            (pal[i] as u32) << 16 | (pal[i + 1] as u32) << 8 | pal[i + 2] as u32
        } else if dark {
            0x4CC2FF
        } else {
            0x005FB8
        };
        if dark {
            Self { dark, bg: 0x2C2C2C, text: 0xFFFFFF, sub: 0xCFCFCF, track: 0x9A9A9A, thumb: 0x454545, thumb_edge: 0x575757, accent }
        } else {
            Self { dark, bg: 0xF3F3F3, text: 0x1A1A1A, sub: 0x5F5F5F, track: 0x8A8A8A, thumb: 0xFFFFFF, thumb_edge: 0xD5D5D5, accent }
        }
    }
}

struct Geo {
    top: f32,
    cy: f32,
    x0: f32,
    x1: f32,
}

struct App {
    hwnd: HWND,
    tx: Sender<Cmd>,
    rows: Vec<Row>,
    loaded: bool,
    /// A value was changed since the popup opened, so a late refresh must
    /// not overwrite what the user just picked.
    touched: bool,
    theme: Theme,
    dpi: u32,
    fonts: Option<(u32, HFONT, HFONT)>,
    icon: HICON,
    drag: Option<usize>,
    hover: Option<usize>,
    focus: usize,
    tracking: bool,
    wheel: i32,
    hidden_at: u32,
    /// Popup anchor: x, and the y of the edge nearest the taskbar.
    anchor: (i32, i32, bool),
    taskbar_created: u32,
    tray_retries: u32,
    face: PCWSTR,
    /// Software-dimming overlays, one per dimmed display.
    dims: Vec<Dim>,
    hotkeys: Hotkeys,
    settings: Option<Settings>,
    /// The popup was opened by a keyboard shortcut: shown without taking
    /// focus, and hidden again after `OSD_MS`.
    osd: bool,
}

struct Dim {
    dev: String,
    hwnd: HWND,
    value: u32,
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let open_shortcuts = std::env::args().any(|a| a == SHORTCUTS_ARG);
        // Second launch: ask the running instance to open its popup (or the
        // shortcuts window) instead.
        let _mutex = CreateMutexW(None, true, w!("Local\\BrightnessTray.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(h) = FindWindowW(CLASS, None) {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let msg = if open_shortcuts { WM_SHORTCUTS } else { WM_SHOW };
                let _ = PostMessageW(h, msg, WPARAM(0), LPARAM(0));
            }
            return;
        }

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let dim = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dim_proc),
            hInstance: hinst.into(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: DIM_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&dim);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Brightness"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            HINSTANCE::from(hinst),
            None,
        ) else {
            return;
        };

        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const c_void, 4);

        let app = Box::new(App {
            hwnd,
            tx: backend::spawn(hwnd, WM_ROWS),
            rows: Vec::new(),
            loaded: false,
            touched: false,
            theme: Theme::load(),
            dpi: 96,
            fonts: None,
            icon: HICON::default(),
            drag: None,
            hover: None,
            focus: 0,
            tracking: false,
            wheel: 0,
            hidden_at: 0,
            anchor: (0, 0, true),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            tray_retries: 0,
            face: pick_font_face(),
            dims: Vec::new(),
            hotkeys: Hotkeys::load(),
            settings: None,
            osd: false,
        });
        let app = Box::into_raw(app);
        APP.store(app, Ordering::Relaxed);
        let app = &mut *app;
        app.update_icon();
        // At logon we can start before Explorer's taskbar is ready; keep
        // retrying for a while (TaskbarCreated alone isn't always sent).
        if !app.tray(NIM_ADD) {
            SetTimer(hwnd, TIMER_TRAY, 2000, None);
        }
        // Keep the Run entry pointing at this exe if it has been moved.
        if autostart_enabled() {
            set_autostart(true);
        }
        // A shortcut taken by another app since it was saved is skipped quietly.
        let _ = app.hotkeys.register(hwnd);

        if open_shortcuts {
            app.open_settings();
        } else if std::env::args().any(|a| a == BACKGROUND_ARG) {
            set_eco(true);
            let _ = app.tx.send(Cmd::Release); // trims the startup working set
        } else {
            app.show();
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Tab / Esc handling for the shortcuts window.
            if let Some(s) = &app.settings {
                if IsDialogMessageW(s.hwnd, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn dim_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(-1); // HTTRANSPARENT: clicks go through
    }
    DefWindowProcW(hwnd, msg, w, l)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let p = APP.load(Ordering::Relaxed);
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    (*p).handle(msg, w, l).unwrap_or_else(|| DefWindowProcW(hwnd, msg, w, l))
}

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

fn mouse_xy(l: LPARAM) -> (f32, f32) {
    ((l.0 & 0xFFFF) as i16 as f32, ((l.0 >> 16) & 0xFFFF) as i16 as f32)
}

impl App {
    fn handle(&mut self, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_TRAY => match loword(l.0 as usize) {
                    WM_LBUTTONUP => self.toggle(),
                    WM_RBUTTONUP => self.menu(),
                    _ => {}
                },
                WM_SHOW => self.show(),
                WM_SHORTCUTS => self.open_settings(),
                WM_ROWS => self.on_rows(*Box::from_raw(l.0 as *mut Vec<Row>), w.0 as isize as i32),
                WM_ACTIVATE if loword(w.0) == WA_INACTIVE => self.hide(),
                WM_ACTIVATE => {
                    // Clicked while shown by a shortcut: now a normal popup.
                    self.end_osd();
                    return None;
                }
                WM_HOTKEY => {
                    let step = self.hotkeys.step as i32;
                    match w.0 as i32 {
                        HK_UP => self.shortcut(step),
                        HK_DOWN => self.shortcut(-step),
                        _ => {}
                    }
                }
                WM_SETTINGS => self.settings_done(w.0 != 0),
                WM_TIMER if w.0 == TIMER_OSD => {
                    if self.hover.is_some() || self.drag.is_some() {
                        SetTimer(self.hwnd, TIMER_OSD, OSD_MS, None);
                    } else {
                        self.hide();
                    }
                }
                WM_TIMER if w.0 == TIMER_RELEASE => {
                    let _ = KillTimer(self.hwnd, TIMER_RELEASE);
                    if !self.visible() {
                        let _ = self.tx.send(Cmd::Release);
                    }
                }
                WM_CLOSE => self.hide(),
                WM_PAINT => self.paint(),
                WM_ERASEBKGND => return Some(LRESULT(1)),
                WM_LBUTTONDOWN => {
                    let (x, y) = mouse_xy(l);
                    if let Some(i) = self.hit(x, y) {
                        self.drag = Some(i);
                        self.focus = i;
                        SetCapture(self.hwnd);
                        self.set_from_x(i, x);
                    }
                }
                WM_MOUSEMOVE => {
                    let (x, y) = mouse_xy(l);
                    if let Some(i) = self.drag {
                        self.set_from_x(i, x);
                    } else {
                        let h = self.hit(x, y);
                        if h != self.hover {
                            self.hover = h;
                            self.invalidate();
                        }
                        if !self.tracking {
                            let mut t = TRACKMOUSEEVENT {
                                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: self.hwnd,
                                dwHoverTime: 0,
                            };
                            self.tracking = TrackMouseEvent(&mut t).is_ok();
                        }
                    }
                }
                WM_MOUSELEAVE => {
                    self.tracking = false;
                    if self.hover.take().is_some() {
                        self.invalidate();
                    }
                }
                WM_LBUTTONUP => {
                    let _ = ReleaseCapture();
                }
                WM_CAPTURECHANGED => {
                    if self.drag.take().is_some() {
                        self.invalidate();
                    }
                }
                WM_MOUSEWHEEL => {
                    let mut pt = POINT { x: (l.0 & 0xFFFF) as i16 as i32, y: ((l.0 >> 16) & 0xFFFF) as i16 as i32 };
                    let _ = ScreenToClient(self.hwnd, &mut pt);
                    let i = self.row_at(pt.y as f32).unwrap_or(self.focus);
                    self.wheel += ((w.0 >> 16) & 0xFFFF) as i16 as i32;
                    let steps = self.wheel / WHEEL_DELTA as i32;
                    self.wheel %= WHEEL_DELTA as i32;
                    self.nudge(i, steps * WHEEL_STEP);
                }
                WM_KEYDOWN => {
                    let i = self.focus;
                    match VIRTUAL_KEY(w.0 as u16) {
                        VK_ESCAPE => self.hide(),
                        VK_RIGHT | VK_UP => self.nudge(i, 1),
                        VK_LEFT | VK_DOWN => self.nudge(i, -1),
                        VK_PRIOR => self.nudge(i, 10),
                        VK_NEXT => self.nudge(i, -10),
                        VK_HOME => self.nudge(i, -100),
                        VK_END => self.nudge(i, 100),
                        VK_TAB if !self.rows.is_empty() => {
                            let n = self.rows.len();
                            let back = GetKeyState(VK_SHIFT.0 as i32) < 0;
                            self.focus = if back { (i + n - 1) % n } else { (i + 1) % n };
                        }
                        _ => {}
                    }
                }
                WM_DPICHANGED => {
                    // The window was already sized for the target monitor.
                    if dpi_override().is_none() {
                        self.dpi = loword(w.0);
                    }
                    self.invalidate();
                }
                WM_SETTINGCHANGE => {
                    if l.0 != 0 && PCWSTR(l.0 as *const u16).to_string().is_ok_and(|s| s == "ImmersiveColorSet") {
                        self.theme = Theme::load();
                        self.update_icon();
                        self.tray(NIM_MODIFY);
                        self.invalidate();
                    }
                }
                WM_DESTROY => {
                    self.tray(NIM_DELETE);
                    PostQuitMessage(0);
                }
                WM_DISPLAYCHANGE => self.sync_dims(),
                WM_TIMER if w.0 == TIMER_TRAY => {
                    self.tray_retries += 1;
                    if self.tray(NIM_ADD) || self.tray_retries >= 60 {
                        let _ = KillTimer(self.hwnd, TIMER_TRAY);
                    }
                }
                m if m == self.taskbar_created && m != 0 => {
                    self.update_icon();
                    self.tray(NIM_ADD);
                }
                _ => return None,
            }
        }
        Some(LRESULT(0))
    }

    // ---- tray ---------------------------------------------------------------

    fn tray(&self, op: NOTIFY_ICON_MESSAGE) -> bool {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icon,
            ..Default::default()
        };
        let tip = match self.rows.first() {
            Some(r) if self.loaded => format!("Brightness: {}%", r.value),
            _ => "Brightness".to_string(),
        };
        for (d, s) in nid.szTip.iter_mut().zip(tip.encode_utf16()) {
            *d = s;
        }
        unsafe { Shell_NotifyIconW(op, &nid).as_bool() }
    }

    fn update_icon(&mut self) {
        unsafe {
            let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem());
            let rgb = if self.theme.dark { 0xFFFFFF } else { 0x1A1A1A };
            let old = std::mem::replace(&mut self.icon, make_icon(size, rgb));
            if !old.is_invalid() {
                let _ = DestroyIcon(old);
            }
        }
    }

    fn menu(&mut self) {
        unsafe {
            let Ok(m) = CreatePopupMenu() else { return };
            let auto = autostart_enabled();
            let _ = AppendMenuW(m, MF_STRING, ID_SHORTCUTS, w!("Keyboard shortcuts…"));
            let _ = AppendMenuW(m, MF_STRING | if auto { MF_CHECKED } else { MF_UNCHECKED }, ID_AUTOSTART, w!("Start with Windows"));
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(m, MF_STRING, ID_EXIT, w!("Exit"));
            menu_dark_mode(self.theme.dark);
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, self.hwnd, None);
            let _ = PostMessageW(self.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(m);
            match cmd.0 as usize {
                ID_SHORTCUTS => self.open_settings(),
                ID_AUTOSTART => set_autostart(!auto),
                ID_EXIT => {
                    let _ = DestroyWindow(self.hwnd);
                }
                _ => {}
            }
        }
    }

    // ---- keyboard shortcuts -------------------------------------------------

    /// A brighter/dimmer shortcut: move every display by `delta` and flash
    /// the popup as feedback without taking focus from the current app.
    fn shortcut(&mut self, delta: i32) {
        let _ = self.tx.send(Cmd::Nudge(delta));
        unsafe {
            if self.visible() {
                if self.osd {
                    SetTimer(self.hwnd, TIMER_OSD, OSD_MS, None);
                }
            } else if self.hotkeys.show_popup() {
                self.open(false);
                SetTimer(self.hwnd, TIMER_OSD, OSD_MS, None);
            } else {
                // No popup (e.g. over a full-screen game): change brightness
                // silently, and release the monitors once the presses stop.
                SetTimer(self.hwnd, TIMER_RELEASE, RELEASE_MS, None);
            }
        }
    }

    fn end_osd(&mut self) {
        if self.osd {
            self.osd = false;
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_OSD);
            }
        }
    }

    fn open_settings(&mut self) {
        if let Some(s) = &self.settings {
            unsafe {
                let _ = SetForegroundWindow(s.hwnd);
            }
            return;
        }
        // Released while the window is open, so pressing the current
        // shortcut types it into the box instead of changing brightness.
        shortcuts::unregister(self.hwnd);
        let icon = unsafe { make_icon(GetSystemMetricsForDpi(SM_CXICON, GetDpiForSystem()), 0xFFB347) };
        self.settings = Settings::open(self.hwnd, self.hotkeys, self.face, icon);
        if self.settings.is_none() {
            let _ = self.hotkeys.register(self.hwnd);
        }
    }

    fn settings_done(&mut self, save: bool) {
        let Some(s) = &self.settings else { return };
        if save {
            let new = s.read();
            if let Err(e) = new.validate().and_then(|_| new.register(self.hwnd)) {
                s.error(&e);
                return;
            }
            new.save();
            self.hotkeys = new;
            self.settings = None;
        } else {
            self.settings = None;
            let _ = self.hotkeys.register(self.hwnd);
        }
    }

    // ---- popup --------------------------------------------------------------

    fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    fn toggle(&mut self) {
        // A click on the tray icon while the popup is open first deactivates
        // (and hides) the popup; don't immediately reopen it.
        let since_hide = unsafe { GetTickCount() }.wrapping_sub(self.hidden_at);
        if self.visible() && self.osd {
            // Shown by a shortcut: keep it open as a normal popup.
            self.end_osd();
            unsafe {
                let _ = SetForegroundWindow(self.hwnd);
            }
        } else if self.visible() {
            self.hide();
        } else if since_hide > 300 {
            self.show();
        }
    }

    fn show(&mut self) {
        self.open(true);
    }

    /// Shows the popup in the corner next to the taskbar. `interactive`
    /// takes focus and re-reads every display; otherwise it is the brief,
    /// non-focused feedback shown for keyboard shortcuts.
    fn open(&mut self, interactive: bool) {
        unsafe {
            set_eco(false);
            self.theme = Theme::load();
            let dark = BOOL::from(self.theme.dark);
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const c_void, 4);

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let hm = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(hm, &mut mi);
            let (mut dx, mut dy) = (96, 96);
            let _ = GetDpiForMonitor(hm, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            self.dpi = dpi_override().unwrap_or(dx);

            // Sit in the corner next to the taskbar, like the system flyouts.
            let margin = self.px(12.0);
            let (wa, mr) = (mi.rcWork, mi.rcMonitor);
            let (w, _) = self.size();
            let x = if wa.left > mr.left { wa.left + margin } else { wa.right - w - margin };
            self.anchor = if wa.top > mr.top { (x, wa.top + margin, false) } else { (x, wa.bottom - margin, true) };

            self.touched = false;
            self.hover = None;
            self.focus = self.focus.min(self.rows.len().saturating_sub(1));
            self.osd = !interactive;
            if interactive {
                self.place(SWP_SHOWWINDOW);
                let _ = SetForegroundWindow(self.hwnd);
                let _ = self.tx.send(Cmd::Refresh);
            } else {
                self.place(SWP_SHOWWINDOW | SWP_NOACTIVATE);
            }
        }
    }

    fn hide(&mut self) {
        self.end_osd();
        if !self.visible() {
            return;
        }
        unsafe {
            let _ = ReleaseCapture();
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            self.hidden_at = GetTickCount();
        }
        self.tray(NIM_MODIFY);
        let _ = self.tx.send(Cmd::Release);
        set_eco(true);
    }

    fn place(&self, flags: SET_WINDOW_POS_FLAGS) {
        let (w, h) = self.size();
        let (x, y, bottom) = self.anchor;
        let y = if bottom { y - h } else { y };
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w, h, flags);
            self.invalidate();
        }
    }

    /// Displays and levels from the worker. `delta` is the shortcut nudge the
    /// worker applied to hardware displays; software-dimmed ones get it here.
    fn on_rows(&mut self, mut rows: Vec<Row>, delta: i32) {
        for r in &mut rows {
            if let Some(dev) = r.soft.clone() {
                r.value = self.dim_value(&dev);
                // An overlay appearing over an exclusive full-screen game
                // would knock it out of full screen, and it wouldn't be
                // visible there anyway.
                if delta != 0 && !shortcuts::fullscreen_game() {
                    r.value = (r.value as i32 + delta).clamp(0, 100) as u32;
                    self.set_dim(&dev, r.value);
                }
            }
        }
        let same = rows.len() == self.rows.len();
        if same && self.touched && delta == 0 {
            for (old, new) in self.rows.iter_mut().zip(rows) {
                if old.soft != new.soft {
                    old.value = new.value;
                }
                old.name = new.name;
                old.soft = new.soft;
            }
        } else {
            self.rows = rows;
        }
        self.loaded = true;
        self.focus = self.focus.min(self.rows.len().saturating_sub(1));
        if self.drag.is_some_and(|i| i >= self.rows.len()) {
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        if self.visible() {
            if same {
                self.invalidate();
            } else {
                self.place(SWP_NOACTIVATE);
            }
        }
        self.tray(NIM_MODIFY);
    }

    fn set_value(&mut self, i: usize, v: u32) {
        let Some(r) = self.rows.get_mut(i) else { return };
        let v = v.min(100);
        if r.value != v {
            r.value = v;
            self.touched = true;
            match r.soft.clone() {
                Some(dev) => self.set_dim(&dev, v),
                None => {
                    let _ = self.tx.send(Cmd::Set(i, v));
                }
            }
            self.invalidate();
        }
    }

    // ---- software dimming ---------------------------------------------------

    fn dim_value(&self, dev: &str) -> u32 {
        self.dims.iter().find(|d| d.dev == dev).map_or(100, |d| d.value)
    }

    fn set_dim(&mut self, dev: &str, v: u32) {
        let pos = self.dims.iter().position(|d| d.dev == dev);
        unsafe {
            if v >= 100 {
                // Fully bright: no overlay at all, so no compositing cost.
                if let Some(i) = pos {
                    let _ = DestroyWindow(self.dims.remove(i).hwnd);
                }
                return;
            }
            let i = match pos {
                Some(i) => i,
                None => {
                    let Some((_, _, rc)) = backend::hmonitors().into_iter().find(|m| m.1 == dev) else { return };
                    let hinst = GetModuleHandleW(None).unwrap_or_default();
                    let Ok(h) = CreateWindowExW(
                        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                        DIM_CLASS,
                        w!(""),
                        WS_POPUP,
                        rc.left,
                        rc.top,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        None,
                        None,
                        HINSTANCE::from(hinst),
                        None,
                    ) else {
                        return;
                    };
                    // Keep screenshots and screen shares undimmed.
                    let _ = SetWindowDisplayAffinity(h, WINDOW_DISPLAY_AFFINITY(0x11));
                    self.dims.push(Dim { dev: dev.to_string(), hwnd: h, value: 100 });
                    self.dims.len() - 1
                }
            };
            let d = &mut self.dims[i];
            d.value = v;
            let alpha = ((100 - v) * DIM_MAX_ALPHA / 100) as u8;
            let _ = SetLayeredWindowAttributes(d.hwnd, COLORREF(0), alpha, LWA_ALPHA);
            let _ = ShowWindow(d.hwnd, SW_SHOWNOACTIVATE);
            if self.visible() {
                // Keep the popup above the overlay so it stays readable.
                let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    /// Follows resolution/layout changes and drops overlays of unplugged displays.
    fn sync_dims(&mut self) {
        let mons = backend::hmonitors();
        self.dims.retain(|d| unsafe {
            match mons.iter().find(|m| m.1 == d.dev) {
                Some((_, _, rc)) => {
                    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
                    let _ = SetWindowPos(d.hwnd, HWND_TOPMOST, rc.left, rc.top, w, h, SWP_NOACTIVATE);
                    true
                }
                None => {
                    let _ = DestroyWindow(d.hwnd);
                    false
                }
            }
        });
    }

    fn nudge(&mut self, i: usize, delta: i32) {
        if let Some(r) = self.rows.get(i) {
            self.focus = i;
            self.set_value(i, (r.value as i32 + delta).clamp(0, 100) as u32);
        }
    }

    fn set_from_x(&mut self, i: usize, x: f32) {
        let g = self.geo(i);
        let t = ((x - g.x0) / (g.x1 - g.x0)).clamp(0.0, 1.0);
        self.set_value(i, (t * 100.0).round() as u32);
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    // ---- layout -------------------------------------------------------------

    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale()).round() as i32
    }

    fn size(&self) -> (i32, i32) {
        let n = self.rows.len().max(1) as f32;
        (self.px(WIDTH), self.px(PAD_TOP + ROW_H * n + 8.0))
    }

    fn geo(&self, i: usize) -> Geo {
        let s = self.scale();
        let top = (PAD_TOP + ROW_H * i as f32) * s;
        Geo { top, cy: top + 34.0 * s, x0: (PAD_X + 36.0) * s, x1: (WIDTH - PAD_X - 46.0) * s }
    }

    fn row_at(&self, y: f32) -> Option<usize> {
        let i = ((y / self.scale() - PAD_TOP) / ROW_H).floor();
        (i >= 0.0 && (i as usize) < self.rows.len()).then_some(i as usize)
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let s = self.scale();
        let i = self.row_at(y)?;
        let g = self.geo(i);
        (y >= g.top + 16.0 * s && x >= g.x0 - 12.0 * s && x <= g.x1 + 12.0 * s).then_some(i)
    }

    fn fonts(&mut self) -> (HFONT, HFONT) {
        if let Some((dpi, a, b)) = self.fonts {
            if dpi == self.dpi {
                return (a, b);
            }
            unsafe {
                let _ = DeleteObject(HGDIOBJ(a.0));
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
        }
        let mk = |size: f32| ui_font((size * self.scale()).round() as i32, self.face);
        let (small, value) = (mk(12.0), mk(14.0));
        self.fonts = Some((self.dpi, small, value));
        (small, value)
    }

    // ---- painting -----------------------------------------------------------

    fn paint(&mut self) {
        let (small, big) = self.fonts();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let (w, h) = (rc.right, rc.bottom);
            if w > 0 && h > 0 {
                let mdc = CreateCompatibleDC(hdc);
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -h,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits = std::ptr::null_mut();
                if let Ok(bmp) = CreateDIBSection(mdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                    let old = SelectObject(mdc, HGDIOBJ(bmp.0));
                    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
                    self.draw(&mut Canvas { w, h, px });
                    self.draw_text(mdc, w, small, big);
                    let _ = BitBlt(hdc, 0, 0, w, h, mdc, 0, 0, SRCCOPY);
                    SelectObject(mdc, old);
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                }
                let _ = DeleteDC(mdc);
            }
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    fn draw(&self, cv: &mut Canvas) {
        let t = &self.theme;
        let s = self.scale();
        cv.clear(t.bg);
        for (i, r) in self.rows.iter().enumerate() {
            let g = self.geo(i);
            let v = r.value as f32 / 100.0;
            let tx = g.x0 + (g.x1 - g.x0) * v;
            let (ty0, ty1) = (g.cy - 2.0 * s, g.cy + 2.0 * s);
            cv.sun((PAD_X + 10.0) * s, g.cy, 20.0 * s, v, t.text);
            cv.rrect(g.x0, ty0, g.x1, ty1, 2.0 * s, t.track);
            cv.rrect(g.x0, ty0, tx, ty1, 2.0 * s, t.accent);
            cv.circle(tx, g.cy, 10.0 * s, t.thumb_edge);
            cv.circle(tx, g.cy, 9.0 * s, t.thumb);
            let inner = if self.drag == Some(i) {
                5.0
            } else if self.hover == Some(i) {
                7.0
            } else {
                6.0
            };
            cv.circle(tx, g.cy, inner * s, t.accent);
        }
    }

    fn draw_text(&self, dc: HDC, w: i32, small: HFONT, big: HFONT) {
        let t = &self.theme;
        let s = self.scale();
        let right = w - self.px(PAD_X);
        unsafe {
            SetBkMode(dc, TRANSPARENT);
            if self.rows.is_empty() {
                let msg = if self.loaded { "No displays with adjustable brightness" } else { "Looking for displays…" };
                let mut text: Vec<u16> = msg.encode_utf16().collect();
                let (_, h) = self.size();
                let mut rc = RECT { left: 0, top: 0, right: w, bottom: h };
                SelectObject(dc, HGDIOBJ(big.0));
                SetTextColor(dc, colorref(t.sub));
                DrawTextW(dc, &mut text, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                return;
            }
            for (i, r) in self.rows.iter().enumerate() {
                let g = self.geo(i);
                let mut name = r.name.clone();
                let mut rc = RECT { left: self.px(PAD_X), top: g.top as i32, right, bottom: (g.top + 18.0 * s) as i32 };
                SelectObject(dc, HGDIOBJ(small.0));
                SetTextColor(dc, colorref(t.sub));
                DrawTextW(dc, &mut name, &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

                let mut val: Vec<u16> = r.value.to_string().encode_utf16().collect();
                let mut rc = RECT {
                    left: (g.x1 + 8.0 * s) as i32,
                    top: (g.cy - 12.0 * s) as i32,
                    right,
                    bottom: (g.cy + 12.0 * s) as i32,
                };
                SelectObject(dc, HGDIOBJ(big.0));
                SetTextColor(dc, colorref(t.text));
                DrawTextW(dc, &mut val, &mut rc, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            }
        }
    }
}

fn ui_font(px: i32, face: PCWSTR) -> HFONT {
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            0,
            face,
        )
    }
}

/// Segoe UI Variable ships with Windows 11; fall back to Segoe UI on Windows 10.
fn pick_font_face() -> PCWSTR {
    let want = w!("Segoe UI Variable Text");
    unsafe {
        let font = ui_font(12, want);
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut buf = [0u16; 64];
        let n = (GetTextFaceW(dc, Some(&mut buf)) as usize).min(buf.len());
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let got = String::from_utf16_lossy(&buf[..n]);
        if got.trim_end_matches('\0') == "Segoe UI Variable Text" {
            want
        } else {
            w!("Segoe UI")
        }
    }
}

/// `BRIGHTNESS_TRAY_DPI` renders the popup at a fixed DPI (for screenshots).
fn dpi_override() -> Option<u32> {
    std::env::var("BRIGHTNESS_TRAY_DPI").ok()?.parse().ok()
}

fn colorref(rgb: u32) -> COLORREF {
    COLORREF((rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16) & 0xFF)
}

fn make_icon(size: i32, rgb: u32) -> HICON {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        let px = gfx::sun_icon(size, rgb);
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(size, size, 1, 1, None);
        let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

/// EcoQoS: lets Windows run us on efficiency cores at low clocks while idle.
fn set_eco(on: bool) {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
    };
    unsafe {
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

/// Makes the tray context menu follow the system theme. Uses the
/// undocumented uxtheme `SetPreferredAppMode` (ordinal 135), as Explorer does.
fn menu_dark_mode(dark: bool) {
    unsafe {
        let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
            let set_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_mode(if dark { 2 } else { 3 }); // ForceDark / ForceLight
        }
        if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
            let flush: extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

fn reg_dword(sub: PCWSTR, name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut n = 4u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, sub, name, RRF_RT_REG_DWORD, None, Some(&mut v as *mut _ as *mut c_void), Some(&mut n))
            .ok()
            .ok()?;
    }
    Some(v)
}

fn reg_binary(sub: PCWSTR, name: PCWSTR, buf: &mut [u8]) -> bool {
    let mut n = buf.len() as u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, sub, name, RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr() as *mut c_void), Some(&mut n))
            .is_ok()
            && n as usize == buf.len()
    }
}

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

fn set_autostart(on: bool) {
    unsafe {
        if !on {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME);
            return;
        }
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(None, &mut buf) as usize;
        let cmd = format!("\"{}\" {BACKGROUND_ARG}", String::from_utf16_lossy(&buf[..n]));
        let wide: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_NAME,
            REG_SZ.0,
            Some(wide.as_ptr() as *const c_void),
            (wide.len() * 2) as u32,
        );
    }
}
