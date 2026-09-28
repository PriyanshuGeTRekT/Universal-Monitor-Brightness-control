//! Display discovery and brightness control.
//!
//! Built-in laptop panels are driven through WMI (`WmiMonitorBrightnessMethods`),
//! external monitors through DDC/CI (VCP code 0x10). Displays that support
//! neither are reported as `soft` and dimmed by an overlay on the UI thread.
//! WMI and DDC/CI calls block for tens of
//! milliseconds per call, so everything runs on one worker thread that sleeps
//! in `recv()` (zero CPU) until the UI sends it a command.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::{mem::size_of, thread};

use windows::core::*;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

use windows::Win32::System::Wmi::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const VCP_BRIGHTNESS: u8 = 0x10;

pub enum Cmd {
    /// Re-enumerate displays and post their current brightness to the UI.
    Refresh,
    /// Set display `.0` to `.1` percent.
    Set(usize, u32),
    /// Drop every handle and COM object and trim the working set.
    Release,
}

pub struct Row {
    pub name: Vec<u16>,
    pub value: u32,
    /// GDI device name (`\\.\DISPLAY2`) of a display without hardware
    /// brightness control; the UI dims it with an overlay instead.
    pub soft: Option<String>,
}

/// Starts the worker. Results of `Refresh` are posted to `hwnd` as `msg`
/// with a `Box<Vec<Row>>` in `lParam`.
pub fn spawn(hwnd: HWND, msg: u32) -> Sender<Cmd> {
    let (tx, rx) = channel();
    let hwnd = hwnd.0 as isize; // HWND is not Send
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || run(rx, HWND(hwnd as _), msg))
        .expect("worker thread");
    tx
}

fn run(rx: Receiver<Cmd>, hwnd: HWND, msg: u32) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        );
    }
    let mut b = Backend::default();
    // Latest requested value per display; slider drags produce far more
    // requests than DDC/CI can apply, so only the newest one is sent.
    let mut pending: Vec<(usize, u32)> = Vec::new();
    while let Ok(cmd) = rx.recv() {
        let mut next = Some(cmd);
        while let Some(cmd) = next.take() {
            match cmd {
                Cmd::Set(i, v) => match pending.iter_mut().find(|p| p.0 == i) {
                    Some(p) => p.1 = v,
                    None => pending.push((i, v)),
                },
                Cmd::Refresh => {
                    b.flush(&mut pending);
                    let rows = Box::into_raw(Box::new(b.refresh()));
                    unsafe {
                        if PostMessageW(hwnd, msg, WPARAM(0), LPARAM(rows as isize)).is_err() {
                            drop(Box::from_raw(rows));
                        }
                    }
                }
                Cmd::Release => {
                    b.flush(&mut pending);
                    b.release();
                    unsafe {
                        CoFreeUnusedLibraries();
                        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
                    }
                }
            }
            next = rx.try_recv().ok();
        }
        b.flush(&mut pending);
    }
}

enum Ctl {
    Wmi,
    Ddc { h: HANDLE, max: u32 },
    Soft(String),
}

struct Mon {
    ctl: Ctl,
    name: String,
    value: u32,
}

#[derive(Default)]
struct Backend {
    mons: Vec<Mon>,
    wmi: Option<Wmi>,
}

impl Backend {
    fn release(&mut self) {
        for m in self.mons.drain(..) {
            if let Ctl::Ddc { h, .. } = m.ctl {
                unsafe {
                    let _ = DestroyPhysicalMonitor(h);
                }
            }
        }
        self.wmi = None;
    }

    fn flush(&mut self, pending: &mut Vec<(usize, u32)>) {
        for (i, v) in pending.drain(..) {
            let Some(m) = self.mons.get_mut(i) else { continue };
            m.value = v;
            match m.ctl {
                Ctl::Wmi => {
                    if let Some(w) = &self.wmi {
                        w.set(v);
                    }
                }
                Ctl::Ddc { h, max } => unsafe {
                    SetVCPFeature(h, VCP_BRIGHTNESS, (v * max + 50) / 100);
                },
                Ctl::Soft(_) => {} // handled by the UI thread
            }
        }
    }

    fn refresh(&mut self) -> Vec<Row> {
        self.release();
        if std::env::var_os("BRIGHTNESS_TRAY_DEMO").is_some() {
            return demo_rows();
        }
        let targets = display_targets();
        let mut hmons = hmonitors();
        hmons.sort_by_key(|m| m.2.left);
        // BRIGHTNESS_TRAY_SOFTWARE=1: skip DDC/CI and WMI, dim everything with overlays.
        let force_soft = std::env::var_os("BRIGHTNESS_TRAY_SOFTWARE").is_some();

        let mut internal = Vec::new();
        let mut external = Vec::new();
        for (hm, dev, _) in hmons {
            let t = targets.iter().find(|t| t.gdi == dev);
            if t.is_some_and(|t| t.internal) {
                internal.push(dev);
                continue;
            }
            let name = t
                .map(|t| t.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Display {}", external.len() + 1));
            if force_soft || !ddc_monitors(hm, &name, &mut external) {
                external.push(Mon { ctl: Ctl::Soft(dev), name, value: 100 });
            }
        }
        if !internal.is_empty() {
            let wmi = if force_soft { None } else { Wmi::connect().and_then(|w| w.get().map(|v| (w, v))) };
            match wmi {
                Some((w, value)) => {
                    self.mons.push(Mon { ctl: Ctl::Wmi, name: "Built-in display".into(), value });
                    self.wmi = Some(w);
                }
                None => {
                    for dev in internal {
                        self.mons.push(Mon { ctl: Ctl::Soft(dev), name: "Built-in display".into(), value: 100 });
                    }
                }
            }
        }
        self.mons.extend(external);
        self.mons
            .iter()
            .map(|m| {
                let soft = match &m.ctl {
                    Ctl::Soft(dev) => Some(dev.clone()),
                    _ => None,
                };
                let name = if soft.is_some() { format!("{} · software dimming", m.name) } else { m.name.clone() };
                Row { name: name.encode_utf16().collect(), value: m.value, soft }
            })
            .collect()
    }
}

/// Fake displays for screenshots (`BRIGHTNESS_TRAY_DEMO=1`); nothing is changed.
fn demo_rows() -> Vec<Row> {
    [("Built-in display", 70), ("DELL U2723QE", 45), ("LG TV · software dimming", 85)]
        .into_iter()
        .map(|(n, v)| Row { name: n.encode_utf16().collect(), value: v, soft: None })
        .collect()
}

/// Adds every DDC/CI-capable physical monitor behind `hm`; false if none.
fn ddc_monitors(hm: HMONITOR, name: &str, out: &mut Vec<Mon>) -> bool {
    let before = out.len();
    unsafe {
        let mut n = 0u32;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(hm, &mut n).is_err() || n == 0 {
            return false;
        }
        let mut pms = vec![PHYSICAL_MONITOR::default(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hm, &mut pms).is_err() {
            return false;
        }
        for (k, pm) in pms.iter().enumerate() {
            let h = pm.hPhysicalMonitor;
            let (mut cur, mut max) = (0u32, 0u32);
            // DDC/CI reads fail transiently on some monitors; retry once.
            let ok = (0..2).any(|_| {
                GetVCPFeatureAndVCPFeatureReply(h, VCP_BRIGHTNESS, None, &mut cur, Some(&mut max)) != 0
            });
            if !ok || max == 0 {
                let _ = DestroyPhysicalMonitor(h);
                continue;
            }
            let name = if n > 1 { format!("{name} ({})", k + 1) } else { name.to_string() };
            out.push(Mon { ctl: Ctl::Ddc { h, max }, name, value: ((cur * 100 + max / 2) / max).min(100) });
        }
    }
    out.len() > before
}

fn wide_str(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Active monitors as (handle, GDI device name, bounds).
pub fn hmonitors() -> Vec<(HMONITOR, String, RECT)> {
    unsafe extern "system" fn cb(hm: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(HMONITOR, String, RECT)>);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hm, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
            v.push((hm, wide_str(&mi.szDevice), mi.monitorInfo.rcMonitor));
        }
        TRUE
    }
    let mut v = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

struct Target {
    gdi: String,
    name: String,
    internal: bool,
}

/// Maps GDI device names (\\.\DISPLAY1) to friendly monitor names and
/// whether the panel is built in.
fn display_targets() -> Vec<Target> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            .is_err()
        {
            return out;
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
                continue;
            }
            let tech = tgt.outputTechnology;
            let internal = tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
                || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS;
            out.push(Target {
                gdi: wide_str(&src.viewGdiDeviceName),
                name: wide_str(&tgt.monitorFriendlyDeviceName),
                internal,
            });
        }
    }
    out
}

/// Connection to `ROOT\WMI` for the built-in panel.
struct Wmi {
    svc: IWbemServices,
    path: BSTR,
    params: IWbemClassObject,
}

impl Wmi {
    fn connect() -> Option<Self> {
        unsafe {
            let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
            let empty = BSTR::new();
            let svc = loc
                .ConnectServer(&BSTR::from("ROOT\\WMI"), &empty, &empty, &empty, 0, &empty, None)
                .ok()?;
            CoSetProxyBlanket(
                &svc,
                10, // RPC_C_AUTHN_WINNT
                0,  // RPC_C_AUTHZ_NONE
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
            .ok()?;

            let inst = first(&svc, "SELECT __PATH FROM WmiMonitorBrightnessMethods WHERE Active=TRUE")?;
            let mut v = VARIANT::default();
            inst.Get(w!("__PATH"), 0, &mut v, None, None).ok()?;
            let path = BSTR::try_from(&v).ok()?;

            let mut class = None;
            svc.GetObject(
                &BSTR::from("WmiMonitorBrightnessMethods"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut class),
                None,
            )
            .ok()?;
            let mut sig = None;
            class?.GetMethod(w!("WmiSetBrightness"), 0, &mut sig, std::ptr::null_mut()).ok()?;
            let params = sig?.SpawnInstance(0).ok()?;
            params.Put(w!("Timeout"), 0, &VARIANT::from(1i32), 0).ok()?;
            Some(Self { svc, path, params })
        }
    }

    fn get(&self) -> Option<u32> {
        unsafe {
            let o = first(&self.svc, "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active=TRUE")?;
            let mut v = VARIANT::default();
            o.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
            u32::try_from(&v).ok().map(|b| b.min(100))
        }
    }

    fn set(&self, pct: u32) {
        unsafe {
            if self.params.Put(w!("Brightness"), 0, &VARIANT::from(pct.min(100) as u8), 0).is_ok() {
                let _ = self.svc.ExecMethod(
                    &self.path,
                    &BSTR::from("WmiSetBrightness"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &self.params,
                    None,
                    None,
                );
            }
        }
    }
}

fn first(svc: &IWbemServices, wql: &str) -> Option<IWbemClassObject> {
    unsafe {
        let e = svc
            .ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(wql),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
            .ok()?;
        let mut objs = [None];
        let mut n = 0u32;
        e.Next(WBEM_INFINITE, &mut objs, &mut n).ok().ok()?;
        objs[0].take()
    }
}
