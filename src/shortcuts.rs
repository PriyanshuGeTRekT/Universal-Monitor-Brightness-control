//! Global keyboard shortcuts for brighter / dimmer, and the small window
//! where they are picked.
//!
//! Shortcuts use `RegisterHotKey`, so there is no keyboard hook and no cost
//! until one is pressed. They are stored in `HKCU\Software\BrightnessTray`
//! in the format of the common "hotkey" control: virtual-key code in the low
//! byte, `HOTKEYF_*` modifier flags in the next byte.

use std::ffi::c_void;
use std::mem::size_of;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const HK_UP: i32 = 1;
pub const HK_DOWN: i32 = 2;
/// Posted by the settings window to its owner; `wParam` is 1 for Save.
pub const WM_SETTINGS: u32 = WM_APP + 4;
pub const STEPS: [u32; 5] = [1, 2, 5, 10, 20];
const DEFAULT_STEP: u32 = 5;

/// When a shortcut shows the popup as feedback.
pub const POPUP_ALWAYS: u32 = 0;
/// Default: not while an exclusive full-screen game owns the display, which
/// would drop out of full screen (and often minimize) if a window appeared.
pub const POPUP_EXCEPT_GAMES: u32 = 1;
pub const POPUP_NEVER: u32 = 2;
const POPUP_LABELS: [&str; 3] = ["Always", "Except over full-screen games", "Never"];

const KEY: PCWSTR = w!("Software\\BrightnessTray");
const CLASS: PCWSTR = w!("BrightnessTray.Shortcuts");
const HKM_SETHOTKEY: u32 = WM_USER + 1;
const HKM_GETHOTKEY: u32 = WM_USER + 2;
const HOTKEYF_SHIFT: u32 = 0x1;
const HOTKEYF_CONTROL: u32 = 0x2;
const HOTKEYF_ALT: u32 = 0x4;
const HOTKEYF_EXT: u32 = 0x8;
const ID_OK: usize = 1;
const ID_CANCEL: usize = 2;

#[derive(Clone, Copy, PartialEq)]
pub struct Hotkeys {
    pub up: u32,
    pub down: u32,
    pub step: u32,
    pub popup: u32,
}

impl Hotkeys {
    pub fn load() -> Self {
        let step = get(w!("Step")).filter(|s| STEPS.contains(s)).unwrap_or(DEFAULT_STEP);
        let popup = get(w!("Popup")).filter(|&p| p <= POPUP_NEVER).unwrap_or(POPUP_EXCEPT_GAMES);
        Self { up: get(w!("HotkeyUp")).unwrap_or(0) & 0xFFFF, down: get(w!("HotkeyDown")).unwrap_or(0) & 0xFFFF, step, popup }
    }

    pub fn save(&self) {
        set(w!("HotkeyUp"), self.up);
        set(w!("HotkeyDown"), self.down);
        set(w!("Step"), self.step);
        set(w!("Popup"), self.popup);
    }

    /// Whether a shortcut press should show the popup right now.
    pub fn show_popup(&self) -> bool {
        match self.popup {
            POPUP_ALWAYS => true,
            POPUP_NEVER => false,
            _ => !fullscreen_game(),
        }
    }

    /// Registers both shortcuts, or neither if one is already taken.
    pub fn register(&self, hwnd: HWND) -> std::result::Result<(), String> {
        unregister(hwnd);
        for (id, hk) in [(HK_UP, self.up), (HK_DOWN, self.down)] {
            if vk(hk) == 0 {
                continue;
            }
            if unsafe { RegisterHotKey(hwnd, id, mods(hk), vk(hk)) }.is_err() {
                unregister(hwnd);
                return Err(format!("{} is already used by another app. Please pick a different shortcut.", describe(hk)));
            }
        }
        Ok(())
    }

    /// Rejects combinations that would break normal typing.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if vk(self.up) != 0 && vk(self.up) == vk(self.down) && flags(self.up) == flags(self.down) {
            return Err("Please use two different shortcuts.".into());
        }
        for hk in [self.up, self.down] {
            let v = vk(hk);
            let special = (0x70..=0x87).contains(&v) || v == 0x13 || v == 0x91; // F1–F24, Pause, Scroll Lock
            if v != 0 && flags(hk) & (HOTKEYF_CONTROL | HOTKEYF_ALT) == 0 && !special {
                return Err(format!(
                    "{} would stop that key working in other apps. Please add Ctrl or Alt.",
                    describe(hk)
                ));
            }
        }
        Ok(())
    }
}

/// True while an app runs Direct3D in exclusive full-screen mode: the same
/// signal Windows uses to hold back its own notifications during games.
/// Any window appearing over such an app makes it leave full screen.
pub fn fullscreen_game() -> bool {
    unsafe { SHQueryUserNotificationState().is_ok_and(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN) }
}

pub fn unregister(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(hwnd, HK_UP);
        let _ = UnregisterHotKey(hwnd, HK_DOWN);
    }
}

fn vk(hk: u32) -> u32 {
    hk & 0xFF
}

fn flags(hk: u32) -> u32 {
    (hk >> 8) & 0xFF
}

fn mods(hk: u32) -> HOT_KEY_MODIFIERS {
    let f = flags(hk);
    let mut m = HOT_KEY_MODIFIERS(0);
    if f & HOTKEYF_SHIFT != 0 {
        m |= MOD_SHIFT;
    }
    if f & HOTKEYF_CONTROL != 0 {
        m |= MOD_CONTROL;
    }
    if f & HOTKEYF_ALT != 0 {
        m |= MOD_ALT;
    }
    m
}

/// "Ctrl + Alt + Up", as shown in the hotkey control.
fn describe(hk: u32) -> String {
    let f = flags(hk);
    let mut parts: Vec<String> = Vec::new();
    for (bit, name) in [(HOTKEYF_CONTROL, "Ctrl"), (HOTKEYF_ALT, "Alt"), (HOTKEYF_SHIFT, "Shift")] {
        if f & bit != 0 {
            parts.push(name.into());
        }
    }
    unsafe {
        let scan = MapVirtualKeyW(vk(hk), MAPVK_VK_TO_VSC) as i32;
        let ext = if f & HOTKEYF_EXT != 0 { 1 << 24 } else { 0 };
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW((scan << 16) | ext, &mut buf) as usize;
        parts.push(if n > 0 { String::from_utf16_lossy(&buf[..n]) } else { format!("Key {}", vk(hk)) });
    }
    parts.join(" + ")
}

fn get(name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut n = 4u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, KEY, name, RRF_RT_REG_DWORD, None, Some(&mut v as *mut _ as *mut c_void), Some(&mut n))
            .ok()
            .ok()?;
    }
    Some(v)
}

fn set(name: PCWSTR, v: u32) {
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, KEY, name, REG_DWORD.0, Some(&v as *const _ as *const c_void), 4);
    }
}

/// The "Keyboard shortcuts" window.
pub struct Settings {
    pub hwnd: HWND,
    up: HWND,
    down: HWND,
    step: HWND,
    popup: HWND,
    font: HFONT,
    icon: HICON,
}

impl Settings {
    /// Opens the window centred on the monitor under the cursor. `owner`
    /// receives `WM_SETTINGS` when the user saves or cancels.
    pub fn open(owner: HWND, hk: Hotkeys, font_face: PCWSTR, icon: HICON) -> Option<Self> {
        unsafe {
            let hinst = HINSTANCE::from(GetModuleHandleW(None).ok()?);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc),
                hInstance: hinst,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut c_void),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassExW(&wc); // fails harmlessly when already registered
            let icc = INITCOMMONCONTROLSEX {
                dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_HOTKEY_CLASS | ICC_STANDARD_CLASSES,
            };
            let _ = InitCommonControlsEx(&icc);

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let hm = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(hm, &mut mi);
            let (mut dpi, mut _dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(hm, MDT_EFFECTIVE_DPI, &mut dpi, &mut _dy);
            let s = |v: i32| v * dpi as i32 / 96;

            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
            let mut rc = RECT { left: 0, top: 0, right: s(392), bottom: s(256) };
            let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            let wa = mi.rcWork;
            let (x, y) = (wa.left + (wa.right - wa.left - w) / 2, wa.top + (wa.bottom - wa.top - h) / 2);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("Brightness Tray – keyboard shortcuts"),
                style,
                x,
                y,
                w,
                h,
                None,
                None,
                hinst,
                None,
            )
            .ok()?;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, owner.0 as isize);

            let font = crate::ui_font(s(12), font_face);
            let child = |ex: u32, class: PCWSTR, text: PCWSTR, style: u32, id: usize, x: i32, y: i32, cw: i32, ch: i32| {
                let c = CreateWindowExW(
                    WINDOW_EX_STYLE(ex),
                    class,
                    text,
                    WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
                    s(x),
                    s(y),
                    s(cw),
                    s(ch),
                    hwnd,
                    HMENU(id as *mut c_void),
                    hinst,
                    None,
                )
                .unwrap_or_default();
                SendMessageW(c, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                c
            };
            let tab = WS_TABSTOP.0;
            child(0, w!("STATIC"), w!("Change the brightness of all displays from any app. Click a box, then press the keys you want. Backspace clears a box."), 0, 0, 16, 12, 360, 36);
            child(0, w!("STATIC"), w!("Increase brightness"), 0, 0, 16, 64, 140, 20);
            let up = child(WS_EX_CLIENTEDGE.0, w!("msctls_hotkey32"), w!(""), tab, 10, 160, 60, 216, 26);
            child(0, w!("STATIC"), w!("Decrease brightness"), 0, 0, 16, 100, 140, 20);
            let down = child(WS_EX_CLIENTEDGE.0, w!("msctls_hotkey32"), w!(""), tab, 11, 160, 96, 216, 26);
            child(0, w!("STATIC"), w!("Change per press"), 0, 0, 16, 138, 140, 20);
            let list = tab | 0x0003 /* CBS_DROPDOWNLIST */ | WS_VSCROLL.0;
            let step = child(0, w!("COMBOBOX"), w!(""), list, 12, 160, 133, 110, 200);
            child(0, w!("STATIC"), w!("Show popup"), 0, 0, 16, 175, 140, 20);
            let popup = child(0, w!("COMBOBOX"), w!(""), list, 13, 160, 170, 216, 200);
            child(0, w!("BUTTON"), w!("Save"), tab | 0x0001 /* BS_DEFPUSHBUTTON */, ID_OK, 196, 214, 86, 28);
            child(0, w!("BUTTON"), w!("Cancel"), tab, ID_CANCEL, 290, 214, 86, 28);

            SendMessageW(up, HKM_SETHOTKEY, WPARAM(hk.up as usize), LPARAM(0));
            SendMessageW(down, HKM_SETHOTKEY, WPARAM(hk.down as usize), LPARAM(0));
            for v in STEPS {
                let label: Vec<u16> = format!("{v} %").encode_utf16().chain(Some(0)).collect();
                SendMessageW(step, CB_ADDSTRING, WPARAM(0), LPARAM(label.as_ptr() as isize));
            }
            let sel = STEPS.iter().position(|&v| v == hk.step).unwrap_or(2);
            SendMessageW(step, CB_SETCURSEL, WPARAM(sel), LPARAM(0));
            for text in POPUP_LABELS {
                let label: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                SendMessageW(popup, CB_ADDSTRING, WPARAM(0), LPARAM(label.as_ptr() as isize));
            }
            SendMessageW(popup, CB_SETCURSEL, WPARAM(hk.popup as usize), LPARAM(0));

            SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(icon.0 as isize));
            SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(icon.0 as isize));
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(up);
            Some(Self { hwnd, up, down, step, popup, font, icon })
        }
    }

    pub fn read(&self) -> Hotkeys {
        unsafe {
            let get = |c: HWND| SendMessageW(c, HKM_GETHOTKEY, WPARAM(0), LPARAM(0)).0 as u32 & 0xFFFF;
            let sel = SendMessageW(self.step, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
            let step = usize::try_from(sel).ok().and_then(|i| STEPS.get(i).copied()).unwrap_or(DEFAULT_STEP);
            let popup = SendMessageW(self.popup, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
            let popup = u32::try_from(popup).ok().filter(|&p| p <= POPUP_NEVER).unwrap_or(POPUP_EXCEPT_GAMES);
            Hotkeys { up: get(self.up), down: get(self.down), step, popup }
        }
    }

    pub fn error(&self, text: &str) {
        unsafe {
            MessageBoxW(self.hwnd, &HSTRING::from(text), w!("Keyboard shortcuts"), MB_OK | MB_ICONWARNING);
        }
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DeleteObject(HGDIOBJ(self.font.0));
            let _ = DestroyIcon(self.icon);
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let notify = |save: bool| {
        let owner = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut c_void);
        let _ = PostMessageW(owner, WM_SETTINGS, WPARAM(save as usize), LPARAM(0));
    };
    match msg {
        WM_COMMAND if w.0 & 0xFFFF == ID_OK => notify(true),
        WM_COMMAND if w.0 & 0xFFFF == ID_CANCEL => notify(false),
        WM_CLOSE => notify(false),
        _ => return DefWindowProcW(hwnd, msg, w, l),
    }
    LRESULT(0)
}
