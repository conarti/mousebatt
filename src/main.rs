//! mousebatt — tray battery monitor for Pulsar / VAXEE wireless mice.
//! Zero-CPU design: blocked in GetMessage, woken by a coarse poll timer,
//! device-change broadcasts, or resume-from-sleep.
#![windows_subsystem = "windows"]
// False positive in clippy 1.98: the thread_local! initializers below are already `const`.
#![allow(clippy::missing_const_for_thread_local)]
#![warn(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

mod hid;
mod icon;
mod protocol;

use std::cell::RefCell;
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use icon::Hicon;
use protocol::{read_battery, set_polling, BatteryStatus, ReadResult};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage,
    CW_USEDEFAULT, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW, WS_OVERLAPPED,
};

const POLL_INTERVAL_MS: u32 = 4 * 60 * 1000; // user preference: every 3-5 minutes
const DEBOUNCE_MS: u32 = 2500; // one replug fires many WM_DEVICECHANGE broadcasts

const WMAPP_TRAY: u32 = WM_APP + 1;
const WMAPP_POLLDONE: u32 = WM_APP + 2;
const TIMER_POLL: usize = 1;
const TIMER_DEBOUNCE: usize = 2;
const MENU_REFRESH: usize = 1;
const MENU_AUTOSTART: usize = 2;
const MENU_EXIT: usize = 3;
/// Polling-rate items are `MENU_RATE_BASE + index into PollingInfo::rates`.
const MENU_RATE_BASE: usize = 100;

// Not re-exported cleanly by windows-sys; values are stable Win32 ABI.
const WM_DEVICECHANGE: u32 = 0x0219;
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

static POLLING: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
/// Polling rate the user picked from the menu, applied by the next poll
/// thread before it reads the battery (0 = nothing pending).
static REQUESTED_HZ: AtomicU16 = AtomicU16::new(0);

// Main-thread UI state.
thread_local! {
    static LAST_GOOD: RefCell<Option<BatteryStatus>> = const { RefCell::new(None) };
    static CUR_ICON: RefCell<Option<Hicon>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    let class_name = wide("mousebatt_tray_wnd");
    // SAFETY: WNDCLASSW is plain data for which all-zero is valid; `class_name`
    // outlives the window (it lives until `main` returns), and the other
    // string temporaries live for the full statement that uses them.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name.as_ptr();
        RegisterClassW(&wc);

        TASKBAR_CREATED_MSG.store(
            RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            Ordering::Relaxed,
        );

        // Hidden top-level window (message-only windows miss WM_DEVICECHANGE).
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("mousebatt").as_ptr(),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        )
    };
    if hwnd.is_null() {
        return;
    }

    // SAFETY: MSG is plain data for which all-zero is valid and `msg` outlives
    // every call that borrows it.
    unsafe {
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Window procedure. Runs only on the main thread, invoked by DispatchMessageW.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            update_tray(hwnd, "…", icon::COLOR_STALE, "mousebatt — reading…", true);
            // SAFETY: plain handle + id arguments; no callback pointer.
            unsafe { SetTimer(hwnd, TIMER_POLL, POLL_INTERVAL_MS, None) };
            start_poll(hwnd);
            0
        }
        WM_TIMER => {
            if wparam == TIMER_POLL {
                // Periodic heartbeat. This is what re-discovers a mouse that
                // slept (and woke) while the system stayed up: no
                // WM_DEVICECHANGE or power broadcast is sent for that, so
                // without this the tray would sit stale until a manual
                // refresh.
                start_poll(hwnd);
            } else if wparam == TIMER_DEBOUNCE {
                // SAFETY: plain handle + id arguments.
                unsafe { KillTimer(hwnd, TIMER_DEBOUNCE) };
            }
            start_poll(hwnd);
            0
        }
        WM_DEVICECHANGE => {
            if wparam == DBT_DEVNODES_CHANGED {
                // SAFETY: plain handle + id arguments; no callback pointer.
                unsafe { SetTimer(hwnd, TIMER_DEBOUNCE, DEBOUNCE_MS, None) };
            }
            0
        }
        WM_POWERBROADCAST => {
            if wparam == PBT_APMRESUMEAUTOMATIC {
                // SAFETY: plain handle + id arguments; no callback pointer.
                unsafe { SetTimer(hwnd, TIMER_DEBOUNCE, 5000, None) };
            }
            0
        }
        WMAPP_POLLDONE => {
            // SAFETY: WMAPP_POLLDONE is only ever posted by `start_poll` with
            // `lparam` = a `Box<ReadResult>` leaked via `Box::into_raw`; this is
            // the single place that reclaims it, so it is freed exactly once.
            let result = unsafe { *Box::from_raw(lparam as *mut ReadResult) };
            on_poll_done(hwnd, result);
            0
        }
        WMAPP_TRAY => match (lparam & 0xffff) as u32 {
            WM_LBUTTONUP => {
                start_poll(hwnd);
                0
            }
            WM_RBUTTONUP => {
                show_menu(hwnd);
                0
            }
            _ => 0,
        },
        WM_DESTROY => {
            remove_tray(hwnd);
            // SAFETY: no preconditions.
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => {
            if msg != 0 && msg == TASKBAR_CREATED_MSG.load(Ordering::Relaxed) {
                // Explorer restarted; re-add our icon.
                update_tray(hwnd, "…", icon::COLOR_STALE, "mousebatt — reading…", true);
                start_poll(hwnd);
                return 0;
            }
            // SAFETY: forwarding the exact arguments we were called with.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

fn start_poll(hwnd: HWND) {
    if POLLING.swap(true, Ordering::SeqCst) {
        return; // a poll is already in flight
    }
    let hwnd_addr = hwnd as usize;
    std::thread::spawn(move || {
        let hz = REQUESTED_HZ.swap(0, Ordering::SeqCst);
        if hz != 0 {
            // Success shows up as the moved check mark after the re-read.
            set_polling(hz);
        }
        let result = Box::into_raw(Box::new(read_battery()));
        // SAFETY: on success, ownership of `result` passes to the message queue
        // and `wndproc` reclaims it. PostMessageW only reads its arguments.
        let posted =
            unsafe { PostMessageW(hwnd_addr as HWND, WMAPP_POLLDONE, 0, result as LPARAM) };
        if posted == 0 {
            // SAFETY: the post failed (window gone), so nobody else has seen
            // the pointer; reclaiming it here frees it exactly once.
            drop(unsafe { Box::from_raw(result) });
        }
        POLLING.store(false, Ordering::SeqCst);
    });
}

fn on_poll_done(hwnd: HWND, result: ReadResult) {
    let last_pct = LAST_GOOD.with(|g| g.borrow().as_ref().map(|s| s.percent));
    let view = tray_view(&result, last_pct);
    update_tray(hwnd, &view.text, view.color, &view.tip, false);
    match result {
        ReadResult::Ok(s) => LAST_GOOD.with(|g| *g.borrow_mut() = Some(s)),
        // Forget the last reading so a different mouse plugged in later
        // can't be shown with this one's percentage.
        ReadResult::NoDevice => LAST_GOOD.with(|g| *g.borrow_mut() = None),
        ReadResult::NoResponse(_) => {}
    }
    // A rate picked while this poll was in flight is still waiting.
    if REQUESTED_HZ.load(Ordering::SeqCst) != 0 {
        start_poll(hwnd);
    }
}

/// What the tray should show for a poll result (pure; unit-tested).
struct TrayView {
    text: String,
    color: u32,
    tip: String,
}

fn tray_view(result: &ReadResult, last_pct: Option<u8>) -> TrayView {
    match result {
        ReadResult::Ok(s) => {
            let color = if s.charging {
                icon::COLOR_CHARGING
            } else if s.percent <= 20 {
                icon::COLOR_LOW
            } else {
                icon::COLOR_NORMAL
            };
            let mut tip = format!("{} — {}%", s.product, s.percent);
            if s.charging {
                tip.push_str(" (charging)");
            }
            if let Some(mv) = s.voltage_mv {
                tip.push_str(&format!(" · {:.2} V", mv as f32 / 1000.0));
            }
            if let Some(p) = s.polling {
                tip.push_str(&format!(" · {} Hz", p.hz));
            }
            TrayView {
                text: s.percent.to_string(),
                color,
                tip,
            }
        }
        ReadResult::NoResponse(product) => match last_pct {
            Some(pct) => TrayView {
                text: pct.to_string(),
                color: icon::COLOR_STALE,
                tip: format!("{} — {}% (stale, mouse not responding)", product, pct),
            },
            None => TrayView {
                text: "?".into(),
                color: icon::COLOR_STALE,
                tip: format!("{} — not responding (asleep?)", product),
            },
        },
        ReadResult::NoDevice => TrayView {
            text: "?".into(),
            color: icon::COLOR_STALE,
            tip: "No supported mouse found".into(),
        },
    }
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: NOTIFYICONDATAW is plain data for which all-zero is valid.
    let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

fn update_tray(hwnd: HWND, text: &str, color: u32, tip: &str, add: bool) {
    let new_icon = icon::battery_icon(text, color);
    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WMAPP_TRAY;
    nid.hIcon = new_icon.raw();
    let wtip = wide(tip);
    let n = wtip.len().min(nid.szTip.len() - 1); // keep the trailing NUL
    nid.szTip[..n].copy_from_slice(&wtip[..n]);
    // If the icon was never successfully added (e.g. NIM_ADD failed while the
    // shell was busy at logon), NIM_MODIFY fails; fall back to NIM_ADD.
    // SAFETY: `nid` is fully initialised with cbSize set; the shell copies the
    // icon, so `new_icon` need only live for the call.
    unsafe {
        if add || Shell_NotifyIconW(NIM_MODIFY, &nid) == 0 {
            Shell_NotifyIconW(NIM_ADD, &nid);
        }
    }
    // Replacing drops (DestroyIcon) the previous icon.
    CUR_ICON.with(|c| *c.borrow_mut() = Some(new_icon));
}

fn remove_tray(hwnd: HWND) {
    let nid = base_nid(hwnd);
    // SAFETY: `nid` is fully initialised with cbSize set.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
}

fn show_menu(hwnd: HWND) {
    let autostart = autostart_enabled();
    let polling = LAST_GOOD.with(|g| g.borrow().as_ref().and_then(|s| s.polling));
    let mut pt = POINT { x: 0, y: 0 };
    // SAFETY: the menu (and the submenu it owns) is created and destroyed
    // within this block; every string temporary lives for the full statement
    // that passes it; `pt` is a valid out-pointer.
    let cmd = unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_REFRESH, wide("Refresh now").as_ptr());
        // Only shown once the mouse has reported its rate; lists what the
        // current link (cable or dongle) can do.
        if let Some(p) = polling {
            let sub = CreatePopupMenu();
            for (i, &hz) in p.rates.iter().enumerate() {
                if hz > p.max_hz {
                    break;
                }
                AppendMenuW(
                    sub,
                    MF_STRING | if hz == p.hz { MF_CHECKED } else { 0 },
                    MENU_RATE_BASE + i,
                    wide(&format!("{hz} Hz")).as_ptr(),
                );
            }
            // DestroyMenu(menu) below also destroys the attached submenu.
            AppendMenuW(menu, MF_POPUP, sub as usize, wide("Polling rate").as_ptr());
        }
        AppendMenuW(
            menu,
            MF_STRING | if autostart { MF_CHECKED } else { 0 },
            MENU_AUTOSTART,
            wide("Start with Windows").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, wide("Exit").as_ptr());

        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd); // required so the menu dismisses on outside click
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            hwnd,
            null(),
        );
        DestroyMenu(menu);
        cmd
    };
    match cmd as usize {
        MENU_REFRESH => start_poll(hwnd),
        MENU_AUTOSTART => set_autostart(!autostart),
        MENU_EXIT => {
            remove_tray(hwnd);
            // SAFETY: no preconditions.
            unsafe { PostQuitMessage(0) };
        }
        id => {
            if let Some(&hz) = id
                .checked_sub(MENU_RATE_BASE)
                .and_then(|i| polling?.rates.get(i))
            {
                REQUESTED_HZ.store(hz, Ordering::SeqCst);
                start_poll(hwnd);
            }
        }
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "MouseBatt";

/// Open registry key, closed on drop.
struct RegKey(HKEY);

impl RegKey {
    fn open_run(access: u32) -> Option<RegKey> {
        let mut key = null_mut();
        // SAFETY: the path temporary lives for the full statement; `key` is a
        // valid out-pointer, and is only wrapped if the open succeeded.
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                0,
                access,
                &mut key,
            )
        };
        (rc == 0).then_some(RegKey(key))
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful RegOpenKeyExW and is closed
        // exactly once.
        unsafe { RegCloseKey(self.0) };
    }
}

fn autostart_enabled() -> bool {
    let Some(key) = RegKey::open_run(KEY_QUERY_VALUE) else {
        return false;
    };
    // SAFETY: all-null out-pointers are permitted (existence check only).
    unsafe {
        RegQueryValueExW(
            key.0,
            wide(RUN_VALUE).as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        ) == 0
    }
}

fn set_autostart(enable: bool) {
    let Some(key) = RegKey::open_run(KEY_SET_VALUE) else {
        return;
    };
    if enable {
        if let Ok(exe) = std::env::current_exe() {
            let value = wide(&format!("\"{}\"", exe.display()));
            // SAFETY: `value` is a NUL-terminated wide string and the byte
            // length passed (u16 count * 2) covers it including the terminator,
            // as REG_SZ requires.
            unsafe {
                RegSetValueExW(
                    key.0,
                    wide(RUN_VALUE).as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr() as *const u8,
                    (value.len() * 2) as u32,
                );
            }
        }
    } else {
        // SAFETY: the name temporary lives for the full statement.
        unsafe { RegDeleteValueW(key.0, wide(RUN_VALUE).as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(percent: u8, charging: bool, mv: Option<u16>) -> ReadResult {
        ReadResult::Ok(BatteryStatus {
            percent,
            charging,
            voltage_mv: mv,
            product: "X3".into(),
            polling: None,
        })
    }

    #[test]
    fn ok_tip_includes_polling_rate() {
        let mut r = status(85, false, Some(3912));
        if let ReadResult::Ok(s) = &mut r {
            s.polling = Some(protocol::PollingInfo {
                hz: 4000,
                max_hz: 8000,
                rates: &protocol::PULSAR_RATES,
            });
        }
        assert_eq!(tray_view(&r, None).tip, "X3 — 85% · 3.91 V · 4000 Hz");
    }

    #[test]
    fn ok_normal_with_voltage() {
        let v = tray_view(&status(85, false, Some(3912)), None);
        assert_eq!(v.text, "85");
        assert_eq!(v.color, icon::COLOR_NORMAL);
        assert_eq!(v.tip, "X3 — 85% · 3.91 V");
    }

    #[test]
    fn ok_low_threshold_is_inclusive() {
        assert_eq!(
            tray_view(&status(20, false, None), None).color,
            icon::COLOR_LOW
        );
        assert_eq!(
            tray_view(&status(21, false, None), None).color,
            icon::COLOR_NORMAL
        );
    }

    #[test]
    fn ok_charging_overrides_low() {
        let v = tray_view(&status(5, true, None), None);
        assert_eq!(v.color, icon::COLOR_CHARGING);
        assert_eq!(v.tip, "X3 — 5% (charging)");
    }

    #[test]
    fn no_response_shows_stale_last_value() {
        let v = tray_view(&ReadResult::NoResponse("X3".into()), Some(60));
        assert_eq!(v.text, "60");
        assert_eq!(v.color, icon::COLOR_STALE);
        assert_eq!(v.tip, "X3 — 60% (stale, mouse not responding)");
    }

    #[test]
    fn no_response_without_history() {
        let v = tray_view(&ReadResult::NoResponse("X3".into()), None);
        assert_eq!(v.text, "?");
        assert_eq!(v.tip, "X3 — not responding (asleep?)");
    }

    #[test]
    fn no_device() {
        let v = tray_view(&ReadResult::NoDevice, Some(60));
        assert_eq!(v.text, "?");
        assert_eq!(v.color, icon::COLOR_STALE);
        assert_eq!(v.tip, "No supported mouse found");
    }
}
