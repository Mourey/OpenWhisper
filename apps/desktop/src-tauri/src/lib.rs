mod jsonl_client;
#[cfg(target_os = "macos")]
mod macos_text_inserter;
#[cfg(target_os = "macos")]
mod modifier_hold;
#[cfg(target_os = "macos")]
mod recording_shortcut;
mod quill_client;
mod worker_client;

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Size, State, Window,
    WindowEvent,
};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;
use uuid::Uuid;
use quill_client::{
    NativeFs, QuillClient, QuillCommandError, QuillEnv, QuillListModesResult,
    QuillReportOutcomeResult, QuillRevertResult, QuillRewriteSelectionResult,
    QuillRewriteTextResult, QUILL_REQUEST_TIMEOUT,
};
use worker_client::WorkerClient;

const SECONDARY_MONITOR_VERTICAL_OFFSET_LOGICAL: f64 = 2.0;
const RECORDING_RETENTION_COUNT: usize = 10;

fn overlay_bottom_anchor(monitor_bottom: i32, work_bottom: i32, is_primary: bool, dpi: u32) -> i32 {
    const BOTTOM_MARGIN: i32 = 10;
    let vertical_offset = if is_primary {
        0
    } else {
        (SECONDARY_MONITOR_VERTICAL_OFFSET_LOGICAL * dpi as f64 / 96.0).round() as i32
    };
    work_bottom
        .saturating_sub(BOTTOM_MARGIN)
        .saturating_add(vertical_offset)
        .min(monitor_bottom.saturating_sub(BOTTOM_MARGIN))
}

// ---------------------------------------------------------------------------
// Platform: text insertion via clipboard + simulated Ctrl+V
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod text_inserter {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

    static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_FOCUS_HWND: AtomicIsize = AtomicIsize::new(0);
    static HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static CTRL_HELD: AtomicBool = AtomicBool::new(false);
    static HOLD_TO_TALK_ENABLED: AtomicBool = AtomicBool::new(true);
    static PRESS_TO_TALK_ACTIVE: AtomicBool = AtomicBool::new(false);
    static SPACE_SWALLOWED: AtomicBool = AtomicBool::new(false);
    static PRESS_TO_TALK_START: AtomicBool = AtomicBool::new(false);
    static PRESS_TO_TALK_STOP: AtomicBool = AtomicBool::new(false);
    static TOGGLE_RECORDING: AtomicBool = AtomicBool::new(false);
    static DEBUG_EVENT_BITS: AtomicU32 = AtomicU32::new(0);

    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const VK_BACK: u16 = 0x08;
    const VK_CONTROL: u16 = 0x11;
    const VK_V: u16 = 0x56;

    const WH_KEYBOARD_LL: i32 = 13;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_SYSKEYDOWN: u32 = 0x0104;
    const WM_SYSKEYUP: u32 = 0x0105;
    const VK_CONTROL_CODE: u32 = 0x11;
    const VK_LCONTROL_CODE: u32 = 0xA2;
    const VK_RCONTROL_CODE: u32 = 0xA3;
    const VK_SPACE: u32 = 0x20;
    const LLKHF_INJECTED: u32 = 0x10;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const MAX_PATH: usize = 260;
    const DBG_CTRL_DOWN: u32 = 1 << 0;
    const DBG_CTRL_UP: u32 = 1 << 1;
    const DBG_SPACE_DOWN: u32 = 1 << 2;
    const DBG_SPACE_REPEAT: u32 = 1 << 3;
    const DBG_SPACE_UP: u32 = 1 << 4;
    const DBG_START_FLAG: u32 = 1 << 5;
    const DBG_STOP_BY_CTRL: u32 = 1 << 6;
    const DBG_STOP_BY_SPACE: u32 = 1 << 7;
    const DBG_TOGGLE_FLAG: u32 = 1 << 8;

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct GuiThreadInfo {
        cb_size: u32,
        flags: u32,
        hwnd_active: isize,
        hwnd_focus: isize,
        hwnd_capture: isize,
        hwnd_menu_owner: isize,
        hwnd_move_size: isize,
        hwnd_caret: isize,
        rc_caret: Rect,
    }

    #[repr(C)]
    struct KbdInput {
        input_type: u32,
        _pad0: u32,
        w_vk: u16,
        w_scan: u16,
        dw_flags: u32,
        time: u32,
        _pad1: u32,
        dw_extra_info: usize,
        _union_pad: [u8; 8],
    }

    #[repr(C)]
    struct KbdLlHookStruct {
        vk_code: u32,
        scan_code: u32,
        flags: u32,
        time: u32,
        dw_extra_info: usize,
    }

    #[link(name = "user32")]
    extern "system" {
        fn SendInput(c_inputs: u32, p_inputs: *const KbdInput, cb_size: i32) -> u32;
        fn GetForegroundWindow() -> isize;
        fn SetForegroundWindow(h_wnd: isize) -> i32;
        fn SetFocus(h_wnd: isize) -> isize;
        fn IsWindow(h_wnd: isize) -> i32;
        fn GetGUIThreadInfo(id_thread: u32, pgui: *mut GuiThreadInfo) -> i32;
        fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: i32) -> i32;
        fn GetWindowThreadProcessId(h_wnd: isize, lp_process_id: *mut u32) -> u32;
        fn SetWindowsHookExW(
            id_hook: i32,
            lpfn: unsafe extern "system" fn(i32, usize, isize) -> isize,
            h_mod: isize,
            dw_thread_id: u32,
        ) -> isize;
        fn CallNextHookEx(hhk: isize, n_code: i32, w_param: usize, l_param: isize) -> isize;
        fn GetMessageW(
            lp_msg: *mut u64,
            h_wnd: isize,
            w_msg_filter_min: u32,
            w_msg_filter_max: u32,
        ) -> i32;
        fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn QueryFullProcessImageNameW(
            h_process: isize,
            dw_flags: u32,
            lp_exe_name: *mut u16,
            lpdw_size: *mut u32,
        ) -> i32;
    }

    fn is_own_process(hwnd: isize) -> bool {
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        pid == std::process::id()
    }

    /// Snapshot the current foreground window, ignoring our own process.
    pub fn save_foreground() {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd != 0 && !is_own_process(hwnd) {
            TARGET_HWND.store(hwnd, Ordering::Relaxed);
            let thread_id = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
            let mut info = GuiThreadInfo {
                cb_size: std::mem::size_of::<GuiThreadInfo>() as u32,
                flags: 0,
                hwnd_active: 0,
                hwnd_focus: 0,
                hwnd_capture: 0,
                hwnd_menu_owner: 0,
                hwnd_move_size: 0,
                hwnd_caret: 0,
                rc_caret: Rect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
            };
            if thread_id != 0 && unsafe { GetGUIThreadInfo(thread_id, &mut info) } != 0 {
                TARGET_FOCUS_HWND.store(info.hwnd_focus, Ordering::Relaxed);
            }
        }
    }

    /// Returns the executable name (without `.exe`) of the saved foreground window.
    pub fn foreground_app_name() -> Option<String> {
        let hwnd = TARGET_HWND.load(Ordering::Relaxed);
        if hwnd == 0 {
            return None;
        }
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == 0 {
            return None;
        }
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            return None;
        }
        let mut buf = [0u16; MAX_PATH];
        let mut size = MAX_PATH as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) };
        unsafe { CloseHandle(handle) };
        if ok == 0 || size == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit('\\')
            .next()
            .map(|f| f.strip_suffix(".exe").unwrap_or(f).to_string())
    }

    /// Returns `true` once when press-to-talk should start.
    pub fn take_press_to_talk_start() -> bool {
        PRESS_TO_TALK_START.swap(false, Ordering::SeqCst)
    }

    /// Returns `true` once when press-to-talk should stop.
    pub fn take_press_to_talk_stop() -> bool {
        PRESS_TO_TALK_STOP.swap(false, Ordering::SeqCst)
    }

    /// Returns `true` once when Ctrl+Space should toggle recording.
    pub fn take_toggle_recording() -> bool {
        TOGGLE_RECORDING.swap(false, Ordering::SeqCst)
    }

    pub fn set_hold_to_talk_enabled(enabled: bool) {
        HOLD_TO_TALK_ENABLED.store(enabled, Ordering::SeqCst);
    }

    pub fn take_debug_events() -> u32 {
        DEBUG_EVENT_BITS.swap(0, Ordering::SeqCst)
    }

    pub fn describe_debug_events(bits: u32) -> String {
        let mut events = Vec::new();
        if bits & DBG_CTRL_DOWN != 0 {
            events.push("ctrl_down");
        }
        if bits & DBG_CTRL_UP != 0 {
            events.push("ctrl_up");
        }
        if bits & DBG_SPACE_DOWN != 0 {
            events.push("space_down");
        }
        if bits & DBG_SPACE_REPEAT != 0 {
            events.push("space_repeat");
        }
        if bits & DBG_SPACE_UP != 0 {
            events.push("space_up");
        }
        if bits & DBG_START_FLAG != 0 {
            events.push("start_flag");
        }
        if bits & DBG_STOP_BY_CTRL != 0 {
            events.push("stop_flag_ctrl_up");
        }
        if bits & DBG_STOP_BY_SPACE != 0 {
            events.push("stop_flag_space_up");
        }
        if bits & DBG_TOGGLE_FLAG != 0 {
            events.push("toggle_flag");
        }
        events.join(",")
    }

    fn restore_target_focus() {
        let hwnd = TARGET_HWND.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { SetForegroundWindow(hwnd) };
            let focus_hwnd = TARGET_FOCUS_HWND.load(Ordering::Relaxed);
            if focus_hwnd != 0 && unsafe { IsWindow(focus_hwnd) } != 0 {
                let target_thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
                let current_thread = unsafe { GetCurrentThreadId() };
                let attached = target_thread != 0
                    && target_thread != current_thread
                    && unsafe { AttachThreadInput(current_thread, target_thread, 1) } != 0;
                unsafe { SetFocus(focus_hwnd) };
                if attached {
                    unsafe { AttachThreadInput(current_thread, target_thread, 0) };
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    }

    fn paste_shortcut() {
        unsafe {
            let size = std::mem::size_of::<KbdInput>() as i32;
            let inputs = [
                kbd(VK_CONTROL, 0),
                kbd(VK_V, 0),
                kbd(VK_V, KEYEVENTF_KEYUP),
                kbd(VK_CONTROL, KEYEVENTF_KEYUP),
            ];
            SendInput(4, inputs.as_ptr(), size);
        }
    }

    pub fn restore_and_paste() {
        restore_target_focus();
        paste_shortcut();
    }

    pub fn restore_and_replace(streamed_chars: usize) {
        restore_target_focus();

        // The caret remains immediately after the chunks pasted during this
        // recording. Remove that exact draft before inserting a refined final.
        let mut remaining = streamed_chars;
        while remaining > 0 {
            let chunk_size = remaining.min(128);
            let mut inputs = Vec::with_capacity(chunk_size * 2);
            for _ in 0..chunk_size {
                inputs.push(kbd(VK_BACK, 0));
                inputs.push(kbd(VK_BACK, KEYEVENTF_KEYUP));
            }
            unsafe {
                SendInput(
                    inputs.len() as u32,
                    inputs.as_ptr(),
                    std::mem::size_of::<KbdInput>() as i32,
                );
            }
            remaining -= chunk_size;
        }
        paste_shortcut();
    }

    // ----- Low-level keyboard hook (Ctrl+Space hold or toggle-to-talk) -----

    // The hook proc must stay extremely small to avoid Windows timing
    // it out. It only flips atomics and swallows Space while the
    // Ctrl+Space gesture is active.
    unsafe extern "system" fn ll_keyboard_proc(
        n_code: i32,
        w_param: usize,
        l_param: isize,
    ) -> isize {
        if n_code >= 0 {
            let info = &*(l_param as *const KbdLlHookStruct);

            if info.flags & LLKHF_INJECTED == 0 {
                let msg = w_param as u32;
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

                match info.vk_code {
                    VK_CONTROL_CODE | VK_LCONTROL_CODE | VK_RCONTROL_CODE => {
                        if down {
                            CTRL_HELD.store(true, Ordering::SeqCst);
                            DEBUG_EVENT_BITS.fetch_or(DBG_CTRL_DOWN, Ordering::SeqCst);
                        } else if up {
                            CTRL_HELD.store(false, Ordering::SeqCst);
                            DEBUG_EVENT_BITS.fetch_or(DBG_CTRL_UP, Ordering::SeqCst);
                            if PRESS_TO_TALK_ACTIVE.swap(false, Ordering::SeqCst) {
                                PRESS_TO_TALK_STOP.store(true, Ordering::SeqCst);
                                DEBUG_EVENT_BITS.fetch_or(DBG_STOP_BY_CTRL, Ordering::SeqCst);
                            }
                        }
                    }
                    VK_SPACE => {
                        // Check repeat first: if we're already in press-to-talk, subsequent downs are key repeat
                        if SPACE_SWALLOWED.load(Ordering::SeqCst) && down {
                            DEBUG_EVENT_BITS.fetch_or(DBG_SPACE_REPEAT, Ordering::SeqCst);
                            return 1;
                        }

                        if down && CTRL_HELD.load(Ordering::SeqCst) {
                            SPACE_SWALLOWED.store(true, Ordering::SeqCst);
                            DEBUG_EVENT_BITS.fetch_or(DBG_SPACE_DOWN, Ordering::SeqCst);
                            if HOLD_TO_TALK_ENABLED.load(Ordering::SeqCst) {
                                if !PRESS_TO_TALK_ACTIVE.swap(true, Ordering::SeqCst) {
                                    PRESS_TO_TALK_START.store(true, Ordering::SeqCst);
                                    DEBUG_EVENT_BITS.fetch_or(DBG_START_FLAG, Ordering::SeqCst);
                                }
                            } else {
                                TOGGLE_RECORDING.store(true, Ordering::SeqCst);
                                DEBUG_EVENT_BITS.fetch_or(DBG_TOGGLE_FLAG, Ordering::SeqCst);
                            }
                            return 1;
                        }

                        if SPACE_SWALLOWED.swap(false, Ordering::SeqCst) && up {
                            DEBUG_EVENT_BITS.fetch_or(DBG_SPACE_UP, Ordering::SeqCst);
                            if PRESS_TO_TALK_ACTIVE.swap(false, Ordering::SeqCst) {
                                PRESS_TO_TALK_STOP.store(true, Ordering::SeqCst);
                                DEBUG_EVENT_BITS.fetch_or(DBG_STOP_BY_SPACE, Ordering::SeqCst);
                            }
                            return 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        CallNextHookEx(
            HOOK_HANDLE.load(Ordering::Relaxed),
            n_code,
            w_param,
            l_param,
        )
    }

    /// Installs the low-level keyboard hook and runs a blocking message pump.
    /// Blocks forever — must be called from a dedicated thread.
    pub fn install_keyboard_hook() {
        let h_mod = unsafe { GetModuleHandleW(std::ptr::null()) };
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, ll_keyboard_proc, h_mod, 0) };
        HOOK_HANDLE.store(hook, Ordering::Relaxed);

        let mut msg_buf = [0u64; 8];
        loop {
            let ret = unsafe { GetMessageW(msg_buf.as_mut_ptr(), 0, 0, 0) };
            if ret <= 0 {
                break;
            }
        }
    }

    fn kbd(vk: u16, flags: u32) -> KbdInput {
        KbdInput {
            input_type: INPUT_KEYBOARD,
            _pad0: 0,
            w_vk: vk,
            w_scan: 0,
            dw_flags: flags,
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _union_pad: [0; 8],
        }
    }
}

#[cfg(target_os = "windows")]
mod overlay_positioner {
    use super::{PhysicalPosition, PhysicalSize, PlacementVisibility};
    use std::mem::size_of;

    const MONITOR_DEFAULTTOPRIMARY: u32 = 1;
    const MONITOR_DEFAULTTONULL: u32 = 0;
    const MDT_EFFECTIVE_DPI: u32 = 0;
    const HWND_TOPMOST: isize = -1;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOREDRAW: u32 = 0x0008;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_SHOWWINDOW: u32 = 0x0040;
    const SWP_HIDEWINDOW: u32 = 0x0080;
    const SWP_NOCOPYBITS: u32 = 0x0100;
    const SWP_NOOWNERZORDER: u32 = 0x0200;
    const SWP_DEFERERASE: u32 = 0x2000;

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: Rect,
        rc_work: Rect,
        dw_flags: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromPoint(point: Point, dw_flags: u32) -> isize;
        fn GetMonitorInfoW(h_monitor: isize, lpmi: *mut MonitorInfo) -> i32;
        fn GetCursorPos(lp_point: *mut Point) -> i32;
        fn GetDpiForWindow(hwnd: isize) -> u32;
        fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn SetWindowPos(
            hwnd: isize,
            insert_after: isize,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> i32;
    }

    #[link(name = "shcore")]
    extern "system" {
        fn GetDpiForMonitor(
            h_monitor: isize,
            dpi_type: u32,
            dpi_x: *mut u32,
            dpi_y: *mut u32,
        ) -> i32;
    }

    fn monitor_anchor_at(point: Point, fallback: u32) -> Option<(isize, i32, i32, u32)> {
        let monitor = unsafe { MonitorFromPoint(point, fallback) };
        if monitor == 0 {
            return None;
        }

        let mut info = MonitorInfo {
            cb_size: size_of::<MonitorInfo>() as u32,
            rc_monitor: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rc_work: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dw_flags: 0,
        };

        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            return None;
        }

        let mut dpi_x = 96;
        let mut dpi_y = 96;
        if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } != 0 {
            dpi_x = 96;
        }

        Some((
            monitor,
            (info.rc_work.left + info.rc_work.right) / 2,
            super::overlay_bottom_anchor(
                info.rc_monitor.bottom,
                info.rc_work.bottom,
                info.dw_flags & 1 != 0,
                dpi_x,
            ),
            dpi_x,
        ))
    }

    pub fn primary_monitor_anchor() -> Option<(isize, i32, i32, u32)> {
        monitor_anchor_at(Point { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
    }

    pub fn cursor_monitor_anchor() -> Option<(isize, i32, i32, u32)> {
        let mut cursor = Point { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut cursor) } == 0 {
            return None;
        }
        // A cursor crossing a physical gap between displays belongs to no
        // monitor yet. Keep the committed placement until it is truly on one.
        monitor_anchor_at(cursor, MONITOR_DEFAULTTONULL)
    }

    pub fn set_window_placement(
        hwnd: isize,
        position: PhysicalPosition<i32>,
        size: PhysicalSize<u32>,
        visibility: PlacementVisibility,
    ) -> Result<(), String> {
        let visibility_flags = match visibility {
            PlacementVisibility::Keep => 0,
            PlacementVisibility::HideWithoutRedraw => {
                SWP_HIDEWINDOW | SWP_NOREDRAW | SWP_NOCOPYBITS | SWP_DEFERERASE
            }
            PlacementVisibility::Show => SWP_SHOWWINDOW,
        };
        let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | visibility_flags;
        let succeeded = unsafe {
            SetWindowPos(
                hwnd,
                0,
                position.x,
                position.y,
                size.width as i32,
                size.height as i32,
                flags,
            )
        };
        if succeeded == 0 {
            Err("SetWindowPos failed while committing overlay placement".to_string())
        } else {
            Ok(())
        }
    }

    /// Re-insert the overlay at the very top of the always-on-top band.
    ///
    /// The window is created WS_EX_TOPMOST, but that only guarantees it sits
    /// above *non*-topmost windows — other topmost windows created after it
    /// (or brought forward later) can still stack on top. Every placement
    /// commit uses SWP_NOZORDER to avoid disturbing z-order during the hidden
    /// settling loop, so nothing otherwise re-raises the pill. The active
    /// listening pill must never be occluded, so we re-assert HWND_TOPMOST
    /// whenever the pill is shown or updated while visible. SWP_NOACTIVATE
    /// keeps it from stealing keyboard focus from the user's real window.
    pub fn reassert_topmost(hwnd: isize) {
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }

    pub fn window_placement(
        hwnd: isize,
    ) -> Option<(PhysicalPosition<i32>, PhysicalSize<u32>, u32)> {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            return None;
        }
        Some((
            PhysicalPosition::new(rect.left, rect.top),
            PhysicalSize::new(
                rect.right.saturating_sub(rect.left) as u32,
                rect.bottom.saturating_sub(rect.top) as u32,
            ),
            unsafe { GetDpiForWindow(hwnd) },
        ))
    }
}

#[cfg(target_os = "windows")]
fn primary_overlay_anchor() -> Option<OverlayAnchor> {
    overlay_positioner::primary_monitor_anchor().map(|(monitor_id, center_x, bottom_y, dpi)| {
        OverlayAnchor {
            monitor_id,
            center_x,
            bottom_y,
            dpi,
        }
    })
}

#[cfg(target_os = "windows")]
fn cursor_overlay_anchor() -> Option<OverlayAnchor> {
    overlay_positioner::cursor_monitor_anchor().map(|(monitor_id, center_x, bottom_y, dpi)| {
        OverlayAnchor {
            monitor_id,
            center_x,
            bottom_y,
            dpi,
        }
    })
}

fn default_transcription_prompt() -> String {
    "I was editing @README.md and checking @package.json in my IDE. Let me also look at @tsconfig.json and the @src directory.".to_string()
}

fn default_transcription_model() -> String {
    "whisper-base-local".to_string()
}

fn is_supported_transcription_model(model: &str) -> bool {
    matches!(
        model,
        "whisper-base-local"
            | "whisper-tiny-local"
            | "whisper-turbo-local"
            | "gpt-4o-mini-transcribe"
            | "gpt-4o-transcribe"
            | "gpt-realtime-transcribe"
    )
}

fn refinement_enabled_for_model(model: &str, requested: bool) -> bool {
    requested
        && !matches!(
            model,
            "whisper-base-local" | "whisper-tiny-local" | "whisper-turbo-local"
        )
}

fn default_refine_enabled() -> bool {
    false
}

fn default_show_idle_pill() -> bool {
    true
}

fn default_hold_to_talk() -> bool {
    true
}

fn default_recording_shortcut() -> String {
    if cfg!(target_os = "macos") {
        "Control+Alt+Space".to_string()
    } else {
        "Control+Space".to_string()
    }
}

/// `$HOME/.local/bin/quill`, where `uv tool install` puts the launcher; empty without a home.
fn default_quill_command_for(home: Option<std::ffi::OsString>) -> String {
    match home {
        Some(home) if !home.is_empty() => PathBuf::from(home)
            .join(".local/bin/quill")
            .to_string_lossy()
            .to_string(),
        _ => String::new(),
    }
}

fn default_quill_command() -> String {
    default_quill_command_for(std::env::var_os("HOME"))
}

/// Stores the requested quill command trimmed. A failing value is kept, not rejected: the
/// settings screen shows `quill_command_error` and the hook stays disabled until it passes.
fn apply_quill_command(settings: &mut AppSettings, requested: Option<String>) {
    if let Some(command) = requested {
        settings.quill_command = command.trim().to_string();
    }
}

fn default_quill_default_mode() -> String {
    "task-claude".to_string()
}

/// The configured quill command's validation string, `None` when it passes.
fn quill_command_error(settings: &AppSettings) -> Option<String> {
    quill_client::validate_quill_command(&settings.quill_command, &NativeFs).err()
}

/// Applies the quill options of a save. A default mode must be 1 to 64 characters of
/// `[a-z0-9-]`; a refused value changes nothing.
fn apply_quill_settings(
    settings: &mut AppSettings,
    default_mode: Option<String>,
    dictation_cleanup: Option<bool>,
    enabled: Option<bool>,
) -> Result<(), String> {
    if let Some(mode) = &default_mode {
        let valid = (1..=64).contains(&mode.len())
            && mode
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err("A quill mode id is 1 to 64 characters of a-z, 0-9 and -.".to_string());
        }
    }
    if let Some(mode) = default_mode {
        settings.quill_default_mode = mode;
    }
    if let Some(cleanup) = dictation_cleanup {
        settings.quill_dictation_cleanup = cleanup;
    }
    if let Some(enabled) = enabled {
        settings.quill_enabled = enabled;
    }
    Ok(())
}

/// Dictation cleanup runs only while the hook is on, its command validated and cleanup is on.
fn cleanup_effective(settings: &AppSettings, command_error: Option<&str>) -> bool {
    settings.quill_enabled && command_error.is_none() && settings.quill_dictation_cleanup
}

/// The model that streams text to the target, which quill does not rewrite.
fn model_streams(model: &str) -> bool {
    model == "gpt-realtime-transcribe"
}

/// The `refine_enabled` sent to `start_session`: as today, and off where quill cleans up.
fn session_refine_enabled(settings: &AppSettings, command_error: Option<&str>) -> bool {
    refinement_enabled_for_model(&settings.transcription_model, settings.refine_enabled)
        && !(cleanup_effective(settings, command_error)
            && !model_streams(&settings.transcription_model))
}

fn hold_to_talk_for_shortcut(shortcut: &str, requested: bool) -> bool {
    requested || (cfg!(target_os = "macos") && shortcut == "RightControl")
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    #[serde(default)]
    openai_api_key: String,
    #[serde(default = "default_transcription_model")]
    transcription_model: String,
    #[serde(default = "default_transcription_prompt")]
    transcription_prompt: String,
    #[serde(default = "default_refine_enabled")]
    refine_enabled: bool,
    #[serde(default)]
    refine_prompt: String,
    #[serde(default = "default_show_idle_pill")]
    show_idle_pill: bool,
    #[serde(default = "default_hold_to_talk")]
    hold_to_talk: bool,
    #[serde(default = "default_recording_shortcut")]
    recording_shortcut: String,
    #[serde(default = "default_quill_command")]
    quill_command: String,
    #[serde(default = "default_quill_default_mode")]
    quill_default_mode: String,
    #[serde(default)]
    quill_dictation_cleanup: bool,
    #[serde(default)]
    quill_enabled: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            openai_api_key: String::new(),
            transcription_model: default_transcription_model(),
            transcription_prompt: default_transcription_prompt(),
            refine_enabled: default_refine_enabled(),
            refine_prompt: String::new(),
            show_idle_pill: true,
            hold_to_talk: true,
            recording_shortcut: default_recording_shortcut(),
            quill_command: default_quill_command(),
            quill_default_mode: default_quill_default_mode(),
            quill_dictation_cleanup: false,
            quill_enabled: false,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FrontendSettings {
    has_openai_api_key: bool,
    openai_api_key_preview: Option<String>,
    transcription_model: String,
    transcription_prompt: String,
    refine_enabled: bool,
    refine_prompt: String,
    show_idle_pill: bool,
    hold_to_talk: bool,
    recording_shortcut: String,
    recording_shortcut_error: Option<String>,
    quill_command: String,
    quill_default_mode: String,
    quill_dictation_cleanup: bool,
    quill_enabled: bool,
    quill_command_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayAnchor {
    monitor_id: isize,
    center_x: i32,
    bottom_y: i32,
    dpi: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OverlayLayoutSpec {
    width: f64,
    height: f64,
    anchor_offset_y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayPlacement {
    position: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    dpi: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementVisibility {
    Keep,
    HideWithoutRedraw,
    Show,
}

const MONITOR_SWITCH_STABLE_SAMPLES: u8 = 5;

#[derive(Default)]
struct MonitorTransitionDebouncer {
    candidate: Option<OverlayAnchor>,
    samples: u8,
}

impl MonitorTransitionDebouncer {
    fn observe(
        &mut self,
        committed: Option<OverlayAnchor>,
        observed: OverlayAnchor,
    ) -> Option<OverlayAnchor> {
        if committed == Some(observed) {
            self.reset();
            return None;
        }
        if self.candidate == Some(observed) {
            self.samples = self.samples.saturating_add(1);
        } else {
            self.candidate = Some(observed);
            self.samples = 1;
        }
        (self.samples >= MONITOR_SWITCH_STABLE_SAMPLES).then_some(observed)
    }

    fn reset(&mut self) {
        self.candidate = None;
        self.samples = 0;
    }
}

struct AppState {
    worker: Mutex<Option<WorkerClient>>,
    /// The quill engine. Never locked together with `worker`.
    quill: Mutex<Option<QuillClient>>,
    settings: std::sync::Mutex<AppSettings>,
    settings_update: Mutex<()>,
    #[cfg(target_os = "macos")]
    recording_shortcut: std::sync::Mutex<recording_shortcut::Runtime>,
    overlay_anchor: std::sync::Mutex<Option<OverlayAnchor>>,
    overlay_layout: std::sync::Mutex<Option<OverlayLayoutSpec>>,
    overlay_layout_locked: AtomicBool,
}

struct OverlayLayoutGuard<'a>(&'a AtomicBool);

impl<'a> OverlayLayoutGuard<'a> {
    fn try_acquire(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for OverlayLayoutGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn resolve_overlay_anchor(state: &Arc<AppState>) -> Result<Option<OverlayAnchor>, String> {
    let mut guard = state
        .overlay_anchor
        .lock()
        .map_err(|_| "Failed to lock overlay anchor".to_string())?;

    #[cfg(target_os = "windows")]
    if guard.is_none() {
        if let Some(anchor) = cursor_overlay_anchor().or_else(primary_overlay_anchor) {
            *guard = Some(anchor);
        }
    }

    Ok(*guard)
}

fn overlay_window_position_for_anchor(
    anchor: OverlayAnchor,
    physical_width: i32,
    physical_height: i32,
) -> PhysicalPosition<i32> {
    let x = anchor.center_x.saturating_sub(physical_width / 2);
    let y = anchor.bottom_y.saturating_sub(physical_height);

    PhysicalPosition::new(x, y)
}

fn overlay_window_position_for_anchor_offset(
    anchor: OverlayAnchor,
    physical_width: i32,
    physical_height: i32,
    physical_bottom_offset: i32,
) -> PhysicalPosition<i32> {
    let mut position = overlay_window_position_for_anchor(anchor, physical_width, physical_height);
    position.y = position.y.saturating_add(physical_bottom_offset);
    position
}

fn physical_dimension(logical: f64, dpi: u32) -> u32 {
    (logical * dpi as f64 / 96.0).round().max(1.0) as u32
}

fn overlay_placement(anchor: OverlayAnchor, layout: OverlayLayoutSpec) -> OverlayPlacement {
    let size = PhysicalSize::new(
        physical_dimension(layout.width, anchor.dpi),
        physical_dimension(layout.height, anchor.dpi),
    );
    let offset = (layout.anchor_offset_y * anchor.dpi as f64 / 96.0).round() as i32;
    OverlayPlacement {
        position: overlay_window_position_for_anchor_offset(
            anchor,
            size.width as i32,
            size.height as i32,
            offset,
        ),
        size,
        dpi: anchor.dpi,
    }
}

#[cfg(target_os = "windows")]
fn commit_native_placement(
    hwnd: isize,
    placement: OverlayPlacement,
    visibility: PlacementVisibility,
) -> Result<(), String> {
    overlay_positioner::set_window_placement(hwnd, placement.position, placement.size, visibility)
}

#[cfg(target_os = "windows")]
fn native_placement_matches(hwnd: isize, expected: OverlayPlacement) -> Result<bool, String> {
    let observed = overlay_positioner::window_placement(hwnd)
        .ok_or_else(|| "GetWindowRect failed while verifying overlay placement".to_string())?;
    Ok(
        observed.0 == expected.position
            && observed.1 == expected.size
            && observed.2 == expected.dpi,
    )
}

#[cfg(target_os = "windows")]
fn sync_overlay_window_to_anchor(app: &AppHandle, anchor: OverlayAnchor) -> Result<bool, String> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let Some(_layout_guard) = OverlayLayoutGuard::try_acquire(&state.overlay_layout_locked) else {
        return Ok(false);
    };

    {
        let current = state
            .overlay_anchor
            .lock()
            .map_err(|_| "Failed to lock overlay anchor".to_string())?;
        if *current == Some(anchor) {
            return Ok(true);
        }
    }

    let layout = *state
        .overlay_layout
        .lock()
        .map_err(|_| "Failed to lock overlay layout".to_string())?;
    let Some(layout) = layout else {
        return Ok(false);
    };
    let Some(window) = app.get_webview_window("main") else {
        return Ok(false);
    };
    let hwnd = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
    let placement = overlay_placement(anchor, layout);
    let was_visible = window.is_visible().map_err(|e| e.to_string())?;

    // A monitor move can dispatch WM_DPICHANGED after SetWindowPos returns.
    // Keep every intermediate rectangle hidden and non-redrawing, then expose
    // exactly one already-settled rectangle to the compositor.
    commit_native_placement(hwnd, placement, PlacementVisibility::HideWithoutRedraw)?;
    let mut stable_samples = 0u8;
    let mut correction_count = 0u8;
    for _ in 0..12 {
        std::thread::sleep(std::time::Duration::from_millis(8));
        if native_placement_matches(hwnd, placement)? {
            stable_samples = stable_samples.saturating_add(1);
            if stable_samples >= 2 {
                break;
            }
        } else {
            stable_samples = 0;
            correction_count = correction_count.saturating_add(1);
            commit_native_placement(hwnd, placement, PlacementVisibility::HideWithoutRedraw)?;
        }
    }
    if !native_placement_matches(hwnd, placement)? {
        return Err(
            "Windows did not settle the overlay at its committed DPI placement".to_string(),
        );
    }
    if was_visible {
        commit_native_placement(hwnd, placement, PlacementVisibility::Show)?;
        overlay_positioner::reassert_topmost(hwnd);
    }
    debug_log_line(
        "window",
        &format!(
            "monitor commit id={} anchor=({}, {}) dpi={} position=({}, {}) size={}x{} hidden_corrections={}",
            anchor.monitor_id,
            anchor.center_x,
            anchor.bottom_y,
            anchor.dpi,
            placement.position.x,
            placement.position.y,
            placement.size.width,
            placement.size.height,
            correction_count
        ),
    );

    let mut current = state
        .overlay_anchor
        .lock()
        .map_err(|_| "Failed to lock overlay anchor".to_string())?;
    *current = Some(anchor);
    Ok(true)
}

#[cfg(target_os = "windows")]
fn position_main_window_before_reveal(app: &AppHandle) -> Result<bool, String> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let Some(window) = app.get_webview_window("main") else {
        return Ok(false);
    };
    let Some(anchor) = resolve_overlay_anchor(&state)? else {
        return Ok(false);
    };
    let layout = *state
        .overlay_layout
        .lock()
        .map_err(|_| "Failed to lock overlay layout".to_string())?;
    let Some(layout) = layout else {
        return Ok(false);
    };
    commit_native_placement(
        window.hwnd().map_err(|e| e.to_string())?.0 as isize,
        overlay_placement(anchor, layout),
        PlacementVisibility::Keep,
    )?;
    Ok(true)
}

fn frontend_settings_from(settings: &AppSettings) -> FrontendSettings {
    let openai_api_key_preview = masked_api_key_preview(&settings.openai_api_key);
    FrontendSettings {
        has_openai_api_key: openai_api_key_preview.is_some(),
        openai_api_key_preview,
        transcription_model: settings.transcription_model.clone(),
        transcription_prompt: settings.transcription_prompt.clone(),
        refine_enabled: refinement_enabled_for_model(
            &settings.transcription_model,
            settings.refine_enabled,
        ),
        refine_prompt: settings.refine_prompt.clone(),
        show_idle_pill: settings.show_idle_pill,
        hold_to_talk: hold_to_talk_for_shortcut(
            &settings.recording_shortcut,
            settings.hold_to_talk,
        ),
        recording_shortcut: settings.recording_shortcut.clone(),
        recording_shortcut_error: None,
        quill_command: settings.quill_command.clone(),
        quill_default_mode: settings.quill_default_mode.clone(),
        quill_dictation_cleanup: settings.quill_dictation_cleanup,
        quill_enabled: settings.quill_enabled,
        quill_command_error: None,
    }
}

fn frontend_settings_with_runtime(
    state: &AppState,
    settings: &AppSettings,
) -> Result<FrontendSettings, String> {
    #[allow(unused_mut)]
    let mut frontend = frontend_settings_from(settings);
    #[cfg(target_os = "macos")]
    {
        frontend.recording_shortcut_error = state
            .recording_shortcut
            .lock()
            .map_err(|_| "Failed to lock recording shortcut".to_string())?
            .error
            .clone();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = state;
    frontend.quill_command_error =
        quill_client::validate_quill_command(&settings.quill_command, &NativeFs).err();
    Ok(frontend)
}

fn masked_api_key_preview(api_key: &str) -> Option<String> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return None;
    }

    let prefix: String = trimmed.chars().take(4).collect();
    let total_chars = trimmed.chars().count();
    if total_chars <= 4 {
        Some(prefix)
    } else {
        Some(format!("{prefix}{}", "*".repeat(8)))
    }
}

#[derive(Debug, Serialize)]
struct SessionStartResponse {
    session_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RecordingMetadata {
    id: String,
    created_at_ms: u128,
    name: String,
    source: String,
    mime_type: String,
    file_name: String,
    status: String,
    bytes: u64,
    transcript: String,
    error: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordingStartResponse {
    id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordingAudioResponse {
    mime_type: String,
    file_base64: String,
}

fn recordings_root(app_handle: &AppHandle) -> Result<PathBuf> {
    let mut root = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| anyhow!("Failed to resolve app data directory: {e}"))?;
    root.push("recordings");
    Ok(root)
}

fn recording_metadata_path(root: &std::path::Path, id: &str) -> PathBuf {
    root.join(id).join("metadata.json")
}

fn read_recording_metadata(root: &std::path::Path, id: &str) -> Result<RecordingMetadata> {
    let raw = fs::read_to_string(recording_metadata_path(root, id))?;
    Ok(serde_json::from_str(&raw)?)
}

fn write_recording_metadata(root: &std::path::Path, metadata: &RecordingMetadata) -> Result<()> {
    let path = recording_metadata_path(root, &metadata.id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(metadata)?)?;
    Ok(())
}

fn list_recording_metadata(root: &std::path::Path) -> Result<Vec<RecordingMetadata>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut recordings = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(metadata) = read_recording_metadata(root, &entry.file_name().to_string_lossy()) {
            recordings.push(metadata);
        }
    }
    recordings.sort_by(|left, right| right.created_at_ms.cmp(&left.created_at_ms));
    Ok(recordings)
}

fn prune_recordings(root: &std::path::Path, keep: usize) -> Result<()> {
    for recording in list_recording_metadata(root)?.into_iter().skip(keep) {
        let path = root.join(recording.id);
        if path.starts_with(root) {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

fn wav_header(pcm_bytes: u32, sample_rate: u32) -> [u8; 44] {
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36_u32.saturating_add(pcm_bytes)).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&1_u16.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&(sample_rate.saturating_mul(2)).to_le_bytes());
    header[32..34].copy_from_slice(&2_u16.to_le_bytes());
    header[34..36].copy_from_slice(&16_u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&pcm_bytes.to_le_bytes());
    header
}

fn append_pcm_chunk(
    root: &std::path::Path,
    recording_id: &str,
    chunk: &[u8],
    sample_rate: u32,
) -> Result<()> {
    let mut metadata = read_recording_metadata(root, recording_id)?;
    let path = root.join(recording_id).join(&metadata.file_name);
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(chunk)?;
    let pcm_bytes = file
        .metadata()?
        .len()
        .saturating_sub(44)
        .min(u32::MAX as u64) as u32;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&wav_header(pcm_bytes, sample_rate))?;
    file.flush()?;
    metadata.bytes = pcm_bytes as u64 + 44;
    write_recording_metadata(root, &metadata)?;
    Ok(())
}

fn safe_extension(name: &str, mime_type: &str) -> &'static str {
    let lowercase = name.to_ascii_lowercase();
    if lowercase.ends_with(".wav") || mime_type.contains("wav") {
        "wav"
    } else if lowercase.ends_with(".mp3") || mime_type.contains("mpeg") {
        "mp3"
    } else if lowercase.ends_with(".m4a") || mime_type.contains("m4a") {
        "m4a"
    } else if lowercase.ends_with(".mp4") || mime_type.contains("mp4") {
        "mp4"
    } else if lowercase.ends_with(".ogg") || mime_type.contains("ogg") {
        "ogg"
    } else {
        "webm"
    }
}

#[tauri::command]
fn recording_begin(
    app_handle: AppHandle,
    source: String,
    name: String,
) -> Result<RecordingStartResponse, String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().to_string();
    let dir = root.join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("audio.wav"), wav_header(0, 24_000)).map_err(|e| e.to_string())?;
    let metadata = RecordingMetadata {
        id: id.clone(),
        created_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        name,
        source,
        mime_type: "audio/wav".to_string(),
        file_name: "audio.wav".to_string(),
        status: "recording".to_string(),
        bytes: 44,
        transcript: String::new(),
        error: String::new(),
    };
    write_recording_metadata(&root, &metadata).map_err(|e| e.to_string())?;
    prune_recordings(&root, RECORDING_RETENTION_COUNT).map_err(|e| e.to_string())?;
    Ok(RecordingStartResponse { id })
}

#[tauri::command]
fn recording_append_chunk(
    app_handle: AppHandle,
    recording_id: String,
    chunk_base64: String,
) -> Result<(), String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(chunk_base64)
        .map_err(|e| format!("Invalid audio chunk: {e}"))?;
    append_pcm_chunk(&root, &recording_id, &chunk, 24_000).map_err(|e| e.to_string())
}

#[tauri::command]
fn recording_import(
    app_handle: AppHandle,
    name: String,
    mime_type: String,
    file_base64: String,
) -> Result<RecordingStartResponse, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(file_base64)
        .map_err(|e| format!("Could not read imported audio: {e}"))?;
    if bytes.is_empty() {
        return Err("The selected audio file is empty".to_string());
    }
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().to_string();
    let extension = safe_extension(&name, &mime_type);
    let file_name = format!("audio.{extension}");
    let dir = root.join(&id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join(&file_name), &bytes).map_err(|e| e.to_string())?;
    let metadata = RecordingMetadata {
        id: id.clone(),
        created_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        name,
        source: "import".to_string(),
        mime_type,
        file_name,
        status: "saved".to_string(),
        bytes: bytes.len() as u64,
        transcript: String::new(),
        error: String::new(),
    };
    write_recording_metadata(&root, &metadata).map_err(|e| e.to_string())?;
    prune_recordings(&root, RECORDING_RETENTION_COUNT).map_err(|e| e.to_string())?;
    Ok(RecordingStartResponse { id })
}

#[tauri::command]
fn recording_finish(
    app_handle: AppHandle,
    recording_id: String,
    status: String,
    transcript: String,
    error: String,
) -> Result<(), String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    let mut metadata = read_recording_metadata(&root, &recording_id).map_err(|e| e.to_string())?;
    metadata.status = status;
    metadata.transcript = transcript;
    metadata.error = error;
    write_recording_metadata(&root, &metadata).map_err(|e| e.to_string())?;
    prune_recordings(&root, RECORDING_RETENTION_COUNT).map_err(|e| e.to_string())
}

#[tauri::command]
fn recording_list(app_handle: AppHandle) -> Result<Vec<RecordingMetadata>, String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    list_recording_metadata(&root).map_err(|e| e.to_string())
}

#[tauri::command]
fn recording_read(
    app_handle: AppHandle,
    recording_id: String,
) -> Result<RecordingAudioResponse, String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    let metadata = read_recording_metadata(&root, &recording_id).map_err(|e| e.to_string())?;
    let audio_path = root.join(recording_id).join(metadata.file_name);
    let bytes = fs::read(audio_path).map_err(|e| format!("Could not read saved recording: {e}"))?;
    if bytes.is_empty() {
        return Err("The saved recording is empty".to_string());
    }
    Ok(RecordingAudioResponse {
        mime_type: metadata.mime_type,
        file_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

#[tauri::command]
fn recording_open(app_handle: AppHandle, recording_id: String) -> Result<(), String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    let metadata = read_recording_metadata(&root, &recording_id).map_err(|e| e.to_string())?;
    let audio_path = root.join(recording_id).join(metadata.file_name);
    app_handle
        .opener()
        .open_path(audio_path.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn recordings_open_folder(app_handle: AppHandle) -> Result<(), String> {
    let root = recordings_root(&app_handle).map_err(|e| e.to_string())?;
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    app_handle
        .opener()
        .open_path(root.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

fn debug_log_line(source: &str, message: &str) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("[openwhisper][{now_ms}][{source}] {message}");
    eprintln!("{line}");
    append_debug_log_file(&line);
}

/// Finder-launched apps lose stderr, so keep a bounded on-disk copy for diagnosing
/// "the shortcut stopped working" reports. Failures to write are deliberately ignored.
fn append_debug_log_file(line: &str) {
    use std::io::Write;
    const MAX_BYTES: u64 = 2 * 1024 * 1024;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = PathBuf::from(home).join("Library/Logs/OpenWhisper");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("debug.log");
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, dir.join("debug.log.1"));
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

async fn with_worker_request(
    app_handle: &AppHandle,
    state: &State<'_, Arc<AppState>>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut guard = state.worker.lock().await;
    if guard.is_none() {
        let (openai_api_key, transcription_model) = {
            let settings = state
                .settings
                .lock()
                .map_err(|_| "Failed to lock settings".to_string())?;
            let key = if settings.openai_api_key.trim().is_empty() {
                None
            } else {
                Some(settings.openai_api_key.clone())
            };
            (key, settings.transcription_model.clone())
        };
        let worker = WorkerClient::spawn(app_handle, openai_api_key, transcription_model)
            .await
            .map_err(|e| e.to_string())?;
        *guard = Some(worker);
    }

    let worker = guard
        .as_mut()
        .ok_or_else(|| "Worker initialization failed".to_string())?;

    let result = worker.request(method, params).await;
    if let Err(error) = &result {
        if error.is_transport() {
            debug_log_line("worker", &format!("discarding worker after transport failure: {error}"));
            // Dropping the client kills the child; the next request spawns a fresh one.
            *guard = None;
        }
    }
    result.map_err(|e| e.to_string())
}

#[tauri::command]
async fn app_get_settings(state: State<'_, Arc<AppState>>) -> Result<FrontendSettings, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "Failed to lock settings".to_string())?
        .clone();
    frontend_settings_with_runtime(&state, &settings)
}

#[tauri::command]
async fn app_get_openai_api_key(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "Failed to lock settings".to_string())?
        .clone();
    Ok(settings.openai_api_key)
}

fn worker_settings_changed(previous: &AppSettings, updated: &AppSettings) -> bool {
    previous.openai_api_key != updated.openai_api_key
        || previous.transcription_model != updated.transcription_model
        || previous.transcription_prompt != updated.transcription_prompt
        || previous.refine_enabled != updated.refine_enabled
        || previous.refine_prompt != updated.refine_prompt
}

/// Whether `settings` hold the quill hotkey on.
#[cfg(target_os = "macos")]
fn quill_binding(settings: &AppSettings) -> bool {
    recording_shortcut::quill_binding(settings.quill_enabled, quill_command_error(settings).as_deref())
}

/// Whether a save turns the hook off, which closes a live revert window.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quill_turned_off(previous: &AppSettings, updated: &AppSettings) -> bool {
    previous.quill_enabled && !updated.quill_enabled
}

/// Whether a save makes a valid quill command invalid, which also closes a live revert window.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quill_command_broke(previous_error: Option<&str>, updated_error: Option<&str>) -> bool {
    previous_error.is_none() && updated_error.is_some()
}

#[tauri::command]
async fn app_save_settings(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
    openai_api_key: Option<String>,
    transcription_model: Option<String>,
    transcription_prompt: Option<String>,
    refine_enabled: Option<bool>,
    refine_prompt: Option<String>,
    show_idle_pill: Option<bool>,
    hold_to_talk: Option<bool>,
    recording_shortcut: Option<String>,
    quill_command: Option<String>,
    quill_default_mode: Option<String>,
    quill_dictation_cleanup: Option<bool>,
    quill_enabled: Option<bool>,
) -> Result<FrontendSettings, String> {
    let _update = state.settings_update.lock().await;
    let shortcut_requested = recording_shortcut.is_some();
    let previous = state
        .settings
        .lock()
        .map_err(|_| "Failed to lock settings".to_string())?
        .clone();
    let updated = {
        let mut current = previous.clone();
        if let Some(key) = openai_api_key {
            current.openai_api_key = key.trim().to_string();
        }
        if let Some(model) = transcription_model {
            if !is_supported_transcription_model(&model) {
                return Err(format!("Unsupported transcription model: {model}"));
            }
            current.transcription_model = model;
        }
        if let Some(prompt) = transcription_prompt {
            current.transcription_prompt = prompt;
        }
        if let Some(enabled) = refine_enabled {
            current.refine_enabled = enabled;
        }
        if let Some(prompt) = refine_prompt {
            current.refine_prompt = prompt;
        }
        if let Some(show) = show_idle_pill {
            current.show_idle_pill = show;
        }
        if let Some(enabled) = hold_to_talk {
            current.hold_to_talk = enabled;
        }
        if let Some(shortcut) = recording_shortcut {
            #[cfg(target_os = "macos")]
            recording_shortcut::RecordingShortcut::parse(&shortcut)?;
            #[cfg(not(target_os = "macos"))]
            if shortcut != "Control+Space" {
                return Err("Only Ctrl+Space is supported on this platform.".to_string());
            }
            current.recording_shortcut = shortcut;
        }
        apply_quill_command(&mut current, quill_command);
        apply_quill_settings(
            &mut current,
            quill_default_mode,
            quill_dictation_cleanup,
            quill_enabled,
        )?;
        current.hold_to_talk =
            hold_to_talk_for_shortcut(&current.recording_shortcut, current.hold_to_talk);
        current.refine_enabled =
            refinement_enabled_for_model(&current.transcription_model, current.refine_enabled);
        current
    };
    #[cfg(target_os = "macos")]
    recording_shortcut::save_with_quill_binding(
        &app_handle,
        quill_binding(&previous),
        quill_binding(&updated),
        || {
            if shortcut_requested {
                recording_shortcut::save_with_binding(&app_handle, &state, &previous, &updated)
            } else {
                save_settings(&app_handle, &updated).map_err(|e| e.to_string())
            }
        },
    )?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = shortcut_requested;
        save_settings(&app_handle, &updated).map_err(|e| e.to_string())?;
    }

    {
        let mut settings = state
            .settings
            .lock()
            .map_err(|_| "Failed to lock settings".to_string())?;
        *settings = updated.clone();
    }

    #[cfg(target_os = "windows")]
    text_inserter::set_hold_to_talk_enabled(updated.hold_to_talk);

    if let Some(tray) = app_handle.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(recording_shortcut_tooltip(&updated)));
    }

    if worker_settings_changed(&previous, &updated) {
        let mut worker = state.worker.lock().await;
        *worker = None;
    }

    #[cfg(target_os = "macos")]
    if quill_turned_off(&previous, &updated) {
        recording_shortcut::disarm_revert(None);
    }
    #[cfg(target_os = "macos")]
    if quill_command_broke(
        quill_command_error(&previous).as_deref(),
        quill_command_error(&updated).as_deref(),
    ) {
        recording_shortcut::disarm_revert(None);
    }
    if previous.quill_enabled != updated.quill_enabled {
        let quill_state = state.inner().clone();
        let enabled = updated.quill_enabled;
        tauri::async_runtime::spawn(async move {
            if !enabled {
                *quill_state.quill.lock().await = None;
                return;
            }
            let Ok(runtime) = QuillRuntime::from_state(&quill_state) else {
                return;
            };
            if let Err(error) =
                quill_client::spawn_into_slot(&quill_state.quill, &runtime.command, &runtime.env()).await
            {
                debug_log_line("quill", &quill_client::quill_event_line("enable_spawn_failed", error.code));
            }
        });
    }

    frontend_settings_with_runtime(&state, &updated)
}

/// What a quill command needs from the app: the configured command and the home directory.
/// `HOME` is read here, at the command entry point, and nowhere else.
struct QuillRuntime {
    command: String,
    home: Option<PathBuf>,
}

impl QuillRuntime {
    fn from_state(state: &AppState) -> Result<Self, QuillCommandError> {
        let command = state
            .settings
            .lock()
            .map_err(|_| QuillCommandError::transport("Failed to lock settings"))?
            .quill_command
            .clone();
        Ok(Self {
            command,
            home: std::env::var_os("HOME").map(PathBuf::from),
        })
    }

    fn env(&self) -> QuillEnv<'_> {
        QuillEnv {
            fs: &NativeFs,
            home: self.home.as_deref(),
            timeout: QUILL_REQUEST_TIMEOUT,
            spawn_timeout: QUILL_REQUEST_TIMEOUT,
        }
    }
}

#[cfg(target_os = "macos")]
type QuillFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, QuillCommandError>> + Send + 'a>>;

/// The one entry every quill command goes through. It builds the runtime from app state
/// (the configured command and `HOME`) and hands the quill slot, the command and the
/// environment to `f`. `quill_client::lease_quill` inside `f` does the rest: `busy` when
/// the slot is taken, revalidation of the command, a respawn after a change or a
/// transport error. It never touches `state.worker`.
#[cfg(target_os = "macos")]
async fn with_quill<T>(
    state: &AppState,
    f: impl for<'a> FnOnce(&'a Mutex<Option<QuillClient>>, &'a str, &'a QuillEnv<'a>) -> QuillFuture<'a, T>,
) -> Result<T, QuillCommandError> {
    if !quill_hook_enabled(state)? {
        return Err(quill_hook_off());
    }
    let runtime = QuillRuntime::from_state(state)?;
    let env = runtime.env();
    f(&state.quill, &runtime.command, &env).await
}

#[cfg(target_os = "macos")]
fn quill_hook_enabled(state: &AppState) -> Result<bool, QuillCommandError> {
    Ok(state
        .settings
        .lock()
        .map_err(|_| QuillCommandError::transport("Failed to lock settings"))?
        .quill_enabled)
}

#[cfg(target_os = "macos")]
fn quill_hook_off() -> QuillCommandError {
    QuillCommandError::new(
        quill_client::QuillErrorCode::Disabled,
        "Quill is turned off. Turn on Rewrite with quill in Settings.",
    )
}

#[cfg(not(target_os = "macos"))]
fn quill_unsupported() -> QuillCommandError {
    QuillCommandError::new(
        quill_client::QuillErrorCode::Disabled,
        "Quill rewriting is only available on macOS.",
    )
}

/// Rewrites the focused selection in place. `quill` is held from before the selection is
/// read until `outcome` is reported; the worker mutex is never involved.
#[tauri::command]
async fn quill_rewrite_selection(
    state: State<'_, Arc<AppState>>,
    mode: String,
    pressed_at_ms: u64,
) -> Result<QuillRewriteSelectionResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        let result = with_quill(&state, |slot, command, env| {
            Box::pin(async move {
                quill_client::run_rewrite_selection(
                    slot,
                    command,
                    env,
                    &mut quill_client::NativeHost,
                    &mode,
                    pressed_at_ms,
                )
                .await
            })
        })
        .await;
        if let Ok(result) = &result {
            // As in `quill_revert_arm`: a save that turned the hook off or broke the command
            // while the rewrite ran has already disarmed, so re-check under its lock.
            let _update = state.settings_update.lock().await;
            if quill_command_ready(&state) && quill_revert_arm_allowed(&state) {
                recording_shortcut::claim_revert(&result.rewrite_id);
                recording_shortcut::arm_revert()
            }
        }
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, mode, pressed_at_ms);
        Err(quill_unsupported())
    }
}

#[tauri::command]
async fn quill_rewrite_text(
    state: State<'_, Arc<AppState>>,
    text: String,
    mode: String,
    surface: String,
    source_app: Option<String>,
) -> Result<QuillRewriteTextResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        let args = quill_client::QuillRewriteTextArgs {
            text,
            mode,
            surface,
            source_app,
        };
        with_quill(&state, |slot, command, env| {
            Box::pin(quill_client::run_rewrite_text(
                slot,
                command,
                env,
                &quill_client::NativeHost,
                args,
            ))
        })
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, text, mode, surface, source_app);
        Err(quill_unsupported())
    }
}

#[tauri::command]
async fn quill_revert(
    state: State<'_, Arc<AppState>>,
    rewrite_id: String,
    delivered: Option<String>,
) -> Result<QuillRevertResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        let reverting = recording_shortcut::revert_generation_now();
        let owned = recording_shortcut::revert_window_is_for(&rewrite_id);
        let result = with_quill(&state, |slot, command, env| {
            Box::pin(async move {
                quill_client::run_revert(
                    slot,
                    command,
                    env,
                    &mut quill_client::NativeHost,
                    &rewrite_id,
                    delivered.as_deref(),
                )
                .await
            })
        })
        .await;
        recording_shortcut::after_revert(result, || {
            if owned {
                recording_shortcut::disarm_revert(reverting)
            }
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, rewrite_id, delivered);
        Err(quill_unsupported())
    }
}

#[tauri::command]
async fn quill_report_outcome(
    state: State<'_, Arc<AppState>>,
    rewrite_id: String,
    delivered: String,
    reverted_via: Option<String>,
) -> Result<QuillReportOutcomeResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        with_quill(&state, |slot, command, env| {
            Box::pin(async move {
                quill_client::run_report_outcome(
                    slot,
                    command,
                    env,
                    &rewrite_id,
                    &delivered,
                    reverted_via.as_deref(),
                )
                .await
            })
        })
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, rewrite_id, delivered, reverted_via);
        Err(quill_unsupported())
    }
}

#[tauri::command]
async fn quill_list_modes(
    state: State<'_, Arc<AppState>>,
) -> Result<QuillListModesResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        with_quill(&state, |slot, command, env| {
            Box::pin(quill_client::run_list_modes(slot, command, env))
        })
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &state;
        Err(quill_unsupported())
    }
}

/// Registers the picker's fourteen keys while the picker is open, all or none.
#[tauri::command]
fn quill_picker_open() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return recording_shortcut::quill_picker_open();
    #[cfg(not(target_os = "macos"))]
    Err("The quill picker is only available on macOS.".to_string())
}

/// Releases the picker keys. Idempotent.
#[tauri::command]
fn quill_picker_close() {
    #[cfg(target_os = "macos")]
    recording_shortcut::quill_picker_close();
}

/// Opens the revert window for a dictation rewrite once its delivery opened the slot, so a
/// rewrite that lands after the frontend guard or a failed delivery arms nothing.
/// Returns whether it armed, so the frontend opens no slot for a refused arm.
#[tauri::command]
async fn quill_revert_arm(state: State<'_, Arc<AppState>>, rewrite_id: String) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        // Every save holds this lock for its whole run, so no save can land between the
        // checks below and the arm, and a save's disarm cannot run before an arm it should undo.
        let _update = state.settings_update.lock().await;
        if !quill_command_ready(&state) {
            return Ok(false);
        }
        if quill_revert_arm_allowed(&state) {
            recording_shortcut::claim_revert(&rewrite_id);
            recording_shortcut::arm_revert();
            return Ok(true);
        }
        Ok(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, rewrite_id);
        Ok(false)
    }
}

/// Whether the configured quill command still validates.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quill_command_ready(state: &AppState) -> bool {
    state
        .settings
        .lock()
        .is_ok_and(|settings| quill_command_error(&settings).is_none())
}

/// Whether `quill_revert_arm` may arm the revert hotkey: only while the hook is on.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quill_revert_arm_allowed(state: &AppState) -> bool {
    state
        .settings
        .lock()
        .is_ok_and(|settings| settings.quill_enabled)
}

/// The clipboard as `quill_clipboard_hold` and `quill_clipboard_release` see it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
trait QuillClipboard {
    /// The plain text on the clipboard, `None` when it holds no text.
    fn text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: &str) -> bool;
}

#[cfg(target_os = "macos")]
struct NativeClipboard(arboard::Clipboard);

#[cfg(target_os = "macos")]
impl QuillClipboard for NativeClipboard {
    fn text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }

    fn set_text(&mut self, text: &str) -> bool {
        self.0.set_text(text).is_ok()
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const QUILL_CLIPBOARD_RESTORE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Saves the plain-text clipboard in `slot`, replacing what an earlier hold saved. A
/// clipboard without text, or none at all, saves nothing.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quill_clipboard_hold_with<C: QuillClipboard>(
    slot: &std::sync::Mutex<Option<String>>,
    clipboard: Option<&mut C>,
) -> Result<(), String> {
    let saved = clipboard.and_then(|clipboard| clipboard.text());
    *slot
        .lock()
        .map_err(|_| "Failed to lock the clipboard hold".to_string())? = saved;
    Ok(())
}

/// Empties `slot`; then, when `restore` is true, waits the restore delay and writes the
/// saved text back only if the clipboard still holds `pasted`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
async fn quill_clipboard_release_with<C, F, S, M>(
    slot: &std::sync::Mutex<Option<String>>,
    restore: bool,
    pasted: &str,
    sleep: S,
    clipboard: M,
) -> Result<(), String>
where
    C: QuillClipboard,
    F: std::future::Future<Output = ()>,
    S: FnOnce(std::time::Duration) -> F,
    M: FnOnce() -> Option<C>,
{
    let saved = slot
        .lock()
        .map_err(|_| "Failed to lock the clipboard hold".to_string())?
        .take();
    let Some(saved) = saved.filter(|_| restore) else {
        return Ok(());
    };
    sleep(QUILL_CLIPBOARD_RESTORE_DELAY).await;
    let Some(mut clipboard) = clipboard() else {
        return Ok(());
    };
    if clipboard.text().as_deref() == Some(pasted) {
        clipboard.set_text(&saved);
    }
    Ok(())
}

/// Saves the plain-text clipboard natively before a quill delivery. Returns nothing, so the
/// clipboard never reaches the webview.
#[tauri::command]
fn quill_clipboard_hold() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut clipboard = arboard::Clipboard::new().ok().map(NativeClipboard);
        quill_clipboard_hold_with(&recording_shortcut::QUILL_CLIPBOARD, clipboard.as_mut())
    }
    #[cfg(not(target_os = "macos"))]
    Ok(())
}

/// Ends the hold: restores the saved clipboard 300 ms after an inserted paste when the
/// clipboard still holds `pasted`, and always empties the slot.
#[tauri::command]
async fn quill_clipboard_release(restore: bool, pasted: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        quill_clipboard_release_with(
            &recording_shortcut::QUILL_CLIPBOARD,
            restore,
            &pasted,
            tokio::time::sleep,
            || arboard::Clipboard::new().ok().map(NativeClipboard),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (restore, pasted);
        Ok(())
    }
}

#[tauri::command]
async fn worker_ping(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    with_worker_request(&app_handle, &state, "ping", json!({})).await
}

#[tauri::command]
async fn worker_list_models(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    with_worker_request(&app_handle, &state, "list_models", json!({})).await
}

#[tauri::command]
async fn worker_start_session(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
    profile_id: String,
) -> Result<SessionStartResponse, String> {
    #[cfg(target_os = "macos")]
    recording_shortcut::quill_picker_close();
    let (prompt, refine_enabled, refine_prompt) = {
        let s = state
            .settings
            .lock()
            .map_err(|_| "Failed to lock settings".to_string())?;
        (
            s.transcription_prompt.clone(),
            session_refine_enabled(&s, quill_command_error(&s).as_deref()),
            s.refine_prompt.clone(),
        )
    };

    let response = with_worker_request(
        &app_handle,
        &state,
        "start_session",
        json!({
            "profile_id": profile_id,
            "prompt": prompt,
            "refine_enabled": refine_enabled,
            "refine_prompt": refine_prompt,
        }),
    )
    .await?;

    let session_id = response
        .get("session_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "Missing session_id from worker".to_string())?
        .to_string();

    Ok(SessionStartResponse { session_id })
}

#[tauri::command]
async fn worker_stop_session(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    with_worker_request(
        &app_handle,
        &state,
        "stop_session",
        json!({ "session_id": session_id }),
    )
    .await
}

#[tauri::command]
async fn worker_poll_session_events(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    with_worker_request(
        &app_handle,
        &state,
        "poll_session_events",
        json!({ "session_id": session_id }),
    )
    .await
}

#[tauri::command]
async fn worker_append_audio_chunk(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
    session_id: String,
    chunk_base64: String,
) -> Result<serde_json::Value, String> {
    with_worker_request(
        &app_handle,
        &state,
        "append_audio_chunk",
        json!({
            "session_id": session_id,
            "chunk_base64": chunk_base64
        }),
    )
    .await
}

#[tauri::command]
async fn worker_finalize_session_audio(
    app_handle: AppHandle,
    state: State<'_, Arc<AppState>>,
    session_id: String,
    mime_type: String,
) -> Result<serde_json::Value, String> {
    with_worker_request(
        &app_handle,
        &state,
        "finalize_session_audio",
        json!({
            "session_id": session_id,
            "mime_type": mime_type
        }),
    )
    .await
}

#[tauri::command]
fn debug_log(message: String) -> Result<(), String> {
    debug_log_line("frontend", &message);
    Ok(())
}

#[tauri::command]
fn get_foreground_app() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        text_inserter::foreground_app_name()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[tauri::command]
fn open_api_keys_page(app_handle: AppHandle) -> Result<(), String> {
    app_handle
        .opener()
        .open_url("https://platform.openai.com/api-keys", None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn paste_to_target(text: String) -> Result<PasteResult, String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(&text))
        .map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    return Ok(macos_text_inserter::paste_to_captured_target(&text));

    #[cfg(target_os = "windows")]
    {
        text_inserter::restore_and_paste();
        Ok(PasteResult {
            pasted: true,
            reason: None,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Ok(PasteResult {
        pasted: false,
        reason: Some("unsupported_target"),
    })
}

#[cfg(target_os = "macos")]
type PasteResult = macos_text_inserter::PasteOutcome;

#[cfg(not(target_os = "macos"))]
#[derive(Serialize)]
struct PasteResult {
    pasted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[tauri::command]
async fn request_paste_access(app_handle: AppHandle) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        app_handle
            .run_on_main_thread(move || {
                let _ = sender.send(macos_text_inserter::request_access());
            })
            .map_err(|error| error.to_string())?;
        receiver.await.map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app_handle;
        Ok(true)
    }
}

#[tauri::command]
fn copy_to_clipboard(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn capture_paste_target() {
    #[cfg(target_os = "windows")]
    text_inserter::save_foreground();

    #[cfg(target_os = "macos")]
    macos_text_inserter::capture_target();
}

#[tauri::command]
async fn replace_streamed_target(
    streamed_text: String,
    final_text: String,
) -> Result<PasteResult, String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(&final_text))
        .map_err(|e| e.to_string())?;

    #[cfg(target_os = "windows")]
    {
        text_inserter::restore_and_replace(streamed_text.chars().count());
        Ok(PasteResult {
            pasted: true,
            reason: None,
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = streamed_text;
        Ok(PasteResult {
            pasted: false,
            reason: Some("unsupported_target"),
        })
    }
}

#[tauri::command]
async fn overlay_apply_layout(
    window: Window,
    state: State<'_, Arc<AppState>>,
    width: f64,
    height: f64,
    anchor_offset_y: f64,
    visible: bool,
) -> Result<(), String> {
    let app_state = state.inner().clone();

    #[cfg(target_os = "windows")]
    {
        let _layout_guard = loop {
            if let Some(guard) = OverlayLayoutGuard::try_acquire(&app_state.overlay_layout_locked) {
                break guard;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        };
        let layout = OverlayLayoutSpec {
            width,
            height,
            anchor_offset_y,
        };
        *app_state
            .overlay_layout
            .lock()
            .map_err(|_| "Failed to lock overlay layout".to_string())? = Some(layout);
        let anchor = resolve_overlay_anchor(&app_state)?;

        if let Some(anchor) = anchor {
            let placement = overlay_placement(anchor, layout);
            let was_visible = window.is_visible().map_err(|e| e.to_string())?;
            let hwnd = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
            debug_log_line(
                "layout",
                &format!(
                    "commit logical={}x{} anchor=({}, {}) dpi={} physical={}x{} position=({}, {}) visible={}",
                    width,
                    height,
                    anchor.center_x,
                    anchor.bottom_y,
                    anchor.dpi,
                    placement.size.width,
                    placement.size.height,
                    placement.position.x,
                    placement.position.y,
                    was_visible
                ),
            );
            let visibility = match (was_visible, visible) {
                (true, true) => PlacementVisibility::Keep,
                (false, true) => PlacementVisibility::HideWithoutRedraw,
                (_, false) => PlacementVisibility::HideWithoutRedraw,
            };
            commit_native_placement(hwnd, placement, visibility)?;

            if visible && !was_visible {
                let mut stable_samples = 0u8;
                for _ in 0..12 {
                    tokio::time::sleep(std::time::Duration::from_millis(8)).await;
                    if native_placement_matches(hwnd, placement)? {
                        stable_samples = stable_samples.saturating_add(1);
                        if stable_samples >= 2 {
                            break;
                        }
                    } else {
                        stable_samples = 0;
                        commit_native_placement(
                            hwnd,
                            placement,
                            PlacementVisibility::HideWithoutRedraw,
                        )?;
                    }
                }
                if !native_placement_matches(hwnd, placement)? {
                    return Err("Initial overlay placement did not settle".to_string());
                }
                commit_native_placement(hwnd, placement, PlacementVisibility::Show)?;
            }
            // The recording pill can appear while the window is already up (an
            // idle→recording transition is a Keep, not a Show). Re-raise it to
            // the top of the topmost band on every visible layout commit so the
            // active listening pill is never occluded by other topmost windows.
            if visible {
                overlay_positioner::reassert_topmost(hwnd);
            }
            return Ok(());
        }
    }

    window
        .set_size(Size::Logical(LogicalSize::new(width, height)))
        .map_err(|e| e.to_string())?;
    if visible {
        window.show().map_err(|e| e.to_string())?;
    } else {
        window.hide().map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn settings_path(app_handle: &AppHandle) -> Result<PathBuf> {
    let mut dir = app_handle
        .path()
        .app_config_dir()
        .map_err(|e| anyhow!("Failed to resolve app config directory: {e}"))?;
    dir.push("settings.json");
    Ok(dir)
}

fn load_settings(app_handle: &AppHandle) -> Result<AppSettings> {
    let path = settings_path(app_handle)?;
    if !path.exists() {
        return Ok(AppSettings::default());
    }
    let raw = fs::read_to_string(&path)?;
    let settings: AppSettings = serde_json::from_str(&raw)?;
    Ok(settings)
}

fn save_settings(app_handle: &AppHandle, settings: &AppSettings) -> Result<()> {
    let path = settings_path(app_handle)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(settings)?;
    fs::write(path, payload)?;
    Ok(())
}

fn make_tray_icon() -> tauri::image::Image<'static> {
    let bytes = include_bytes!("../icons/32x32.png");
    tauri::image::Image::from_bytes(bytes)
        .expect("tray icon 32x32.png")
        .to_owned()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverlayRevealIntent {
    Passive,
    Interactive,
}

impl OverlayRevealIntent {
    fn should_focus(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

fn reveal_window(app: &AppHandle, intent: OverlayRevealIntent) {
    if matches!(intent, OverlayRevealIntent::Passive) {
        let show_idle_pill = app
            .state::<Arc<AppState>>()
            .settings
            .lock()
            .map(|settings| settings.show_idle_pill)
            .unwrap_or(true);
        if !show_idle_pill {
            debug_log_line("window", "passive reveal skipped while idle pill is hidden");
            return;
        }
    }
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "windows")]
        match position_main_window_before_reveal(app) {
            Ok(true) => {}
            Ok(false) => {
                debug_log_line("window", "reveal deferred until layout is committed");
                return;
            }
            Err(err) => {
                debug_log_line(
                    "window",
                    &format!("position_main_window_before_reveal failed: {err}"),
                );
                return;
            }
        }

        debug_log_line("window", "reveal_window");
        let _ = window.show();
        let _ = window.unminimize();
        #[cfg(target_os = "windows")]
        if let Ok(hwnd) = window.hwnd() {
            overlay_positioner::reassert_topmost(hwnd.0 as isize);
        }
        if intent.should_focus() {
            let _ = window.set_focus();
        }
        let _ = window.emit("overlay-revealed", intent.should_focus());
    }
}

fn reveal_main_window(app: &AppHandle) {
    reveal_window(app, OverlayRevealIntent::Interactive);
}

fn reveal_overlay_window(app: &AppHandle) {
    reveal_window(app, OverlayRevealIntent::Passive);
}

fn recording_shortcut_tooltip(settings: &AppSettings) -> String {
    let label = if cfg!(target_os = "macos") {
        settings
            .recording_shortcut
            .replace("Control+Alt+", "Control+Option+")
            .replace("KeyW", "W")
            .replace("RightControl", "Right Control")
    } else {
        "Ctrl+Space".to_string()
    };
    format!("OpenWhisper – {label} to talk")
}

fn setup_tray(app: &tauri::App) -> Result<()> {
    let show_i = MenuItem::with_id(app, "show", "Show Pill", true, None::<&str>)
        .map_err(|e| anyhow!("{e}"))?;
    let record_i = MenuItem::with_id(app, "record", "Toggle Recording", true, None::<&str>)
        .map_err(|e| anyhow!("{e}"))?;
    let sep = PredefinedMenuItem::separator(app).map_err(|e| anyhow!("{e}"))?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit OpenWhisper", true, None::<&str>)
        .map_err(|e| anyhow!("{e}"))?;

    let menu =
        Menu::with_items(app, &[&show_i, &record_i, &sep, &quit_i]).map_err(|e| anyhow!("{e}"))?;

    let settings = app
        .state::<Arc<AppState>>()
        .settings
        .lock()
        .map_err(|_| anyhow!("Failed to lock settings for tray"))?
        .clone();
    TrayIconBuilder::with_id("main")
        .icon(make_tray_icon())
        .tooltip(recording_shortcut_tooltip(&settings))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => reveal_main_window(app),
            "record" => {
                #[cfg(target_os = "windows")]
                text_inserter::save_foreground();
                reveal_main_window(app);
                let _ = app.emit("toggle-recording", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                reveal_main_window(&app);
            }
        })
        .build(app)
        .map_err(|e| anyhow!("{e}"))?;

    Ok(())
}

pub fn run() {
    let app_state = Arc::new(AppState {
        worker: Mutex::new(None),
        quill: Mutex::new(None),
        settings: std::sync::Mutex::new(AppSettings::default()),
        settings_update: Mutex::new(()),
        #[cfg(target_os = "macos")]
        recording_shortcut: std::sync::Mutex::new(recording_shortcut::Runtime::default()),
        overlay_anchor: std::sync::Mutex::new(None),
        overlay_layout: std::sync::Mutex::new(None),
        overlay_layout_locked: AtomicBool::new(false),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args(["--autostart"])
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }

            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    reveal_overlay_window(&window.app_handle());
                }
                WindowEvent::Focused(false) => {
                    if std::env::var_os("OPENWHISPER_UI_TEST").is_none() {
                        let _ = window.emit("overlay-lost-focus", ());
                    }
                }
                _ => {}
            }
        })
        .setup(|app| {
            #[allow(unused_mut)]
            let mut loaded = load_settings(app.handle()).unwrap_or_default();
            #[cfg(target_os = "macos")]
            if let Err(error) = recording_shortcut::install(app.handle(), &mut loaded) {
                eprintln!("[openwhisper][shortcut] {error}");
                app.state::<Arc<AppState>>().recording_shortcut.lock()
                    .map_err(|_| anyhow!("Failed to lock recording shortcut during setup"))?
                    .error = Some(error);
            }
            #[cfg(target_os = "windows")]
            text_inserter::set_hold_to_talk_enabled(loaded.hold_to_talk);
            let state = app.state::<Arc<AppState>>();
            {
                let mut settings = state
                    .settings
                    .lock()
                    .map_err(|_| anyhow!("Failed to lock settings during setup"))?;
                *settings = loaded;
            }
            let quill_enabled = state
                .settings
                .lock()
                .map_err(|_| anyhow!("Failed to lock settings during setup"))?
                .quill_enabled;

            // There is no enable setting yet, so nothing is enabled at setup and the client
            // starts on the first command; the setting replaces `false` later.
            let quill_validation = {
                let command = state
                    .settings
                    .lock()
                    .map_err(|_| anyhow!("Failed to lock settings during setup"))?
                    .quill_command
                    .clone();
                quill_client::validate_quill_command(&command, &NativeFs)
            };
            if quill_client::quill_should_spawn_at_setup(quill_enabled, &quill_validation) {
                let quill_state = state.inner().clone();
                tauri::async_runtime::spawn(async move {
                    let Ok(runtime) = QuillRuntime::from_state(&quill_state) else {
                        return;
                    };
                    if let Err(error) =
                        quill_client::spawn_into_slot(&quill_state.quill, &runtime.command, &runtime.env())
                            .await
                    {
                        debug_log_line("quill", &quill_client::quill_event_line("setup_spawn_failed", error.code));
                    }
                });
            }

            if let Err(e) = setup_tray(app) {
                eprintln!("Failed to create tray icon: {e}");
            }

            // The frontend's first layout commit performs the initial reveal.
            // Until then the window has no authoritative logical size or DPI
            // placement and must remain hidden.

            #[cfg(target_os = "windows")]
            {
                std::thread::spawn(|| text_inserter::install_keyboard_hook());

                let poll_handle = app.handle().clone();
                std::thread::spawn(move || {
                    let mut last_hook_log = std::time::Instant::now();
                    let mut last_hook_event = String::new();
                    let mut monitor_transition = MonitorTransitionDebouncer::default();
                    loop {
                        text_inserter::save_foreground();
                        if let Some(cursor_anchor) = cursor_overlay_anchor() {
                            let current_anchor = poll_handle
                                .state::<Arc<AppState>>()
                                .overlay_anchor
                                .lock()
                                .ok()
                                .and_then(|guard| *guard);

                            // Require five consecutive 20 ms observations before
                            // gathering and committing the destination geometry.
                            if let Some(target) =
                                monitor_transition.observe(current_anchor, cursor_anchor)
                            {
                                match sync_overlay_window_to_anchor(&poll_handle, target) {
                                    Ok(true) => monitor_transition.reset(),
                                    Ok(false) => {}
                                    Err(err) => debug_log_line(
                                        "window",
                                        &format!("monitor switch failed: {err}"),
                                    ),
                                }
                            }
                        }
                        let should_start = text_inserter::take_press_to_talk_start();
                        let should_stop = text_inserter::take_press_to_talk_stop();
                        let should_toggle = text_inserter::take_toggle_recording();
                        let debug_events = text_inserter::take_debug_events();
                        if debug_events != 0 {
                            let desc = text_inserter::describe_debug_events(debug_events);
                            let now = std::time::Instant::now();
                            let event_changed = desc != last_hook_event;
                            let throttle_elapsed = now.duration_since(last_hook_log).as_secs() >= 3;
                            if event_changed || throttle_elapsed {
                                debug_log_line(
                                    "hook",
                                    &format!(
                                        "events={} start_pending={} stop_pending={} toggle_pending={}",
                                        desc, should_start, should_stop, should_toggle
                                    ),
                                );
                                last_hook_log = now;
                                last_hook_event = desc;
                            }
                        }
                        if should_start {
                            debug_log_line("shortcut", "emit press-to-talk-start");
                            text_inserter::save_foreground();
                            reveal_overlay_window(&poll_handle);
                            let _ = poll_handle.emit("press-to-talk-start", ());
                        }
                        if should_stop {
                            debug_log_line("shortcut", "emit press-to-talk-stop");
                            let _ = poll_handle.emit("press-to-talk-stop", ());
                        }
                        if should_toggle {
                            debug_log_line("shortcut", "emit toggle-recording");
                            text_inserter::save_foreground();
                            reveal_overlay_window(&poll_handle);
                            let _ = poll_handle.emit("toggle-recording", ());
                        }

                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_get_settings,
            app_get_openai_api_key,
            app_save_settings,
            quill_rewrite_selection,
            quill_rewrite_text,
            quill_revert,
            quill_report_outcome,
            quill_list_modes,
            quill_picker_open,
            quill_picker_close,
            quill_clipboard_hold,
            quill_clipboard_release,
            quill_revert_arm,
            worker_ping,
            worker_list_models,
            worker_start_session,
            worker_stop_session,
            worker_poll_session_events,
            worker_append_audio_chunk,
            worker_finalize_session_audio,
            recording_begin,
            recording_append_chunk,
            recording_import,
            recording_finish,
            recording_list,
            recording_read,
            recording_open,
            recordings_open_folder,
            debug_log,
            get_foreground_app,
            open_api_keys_page,
            capture_paste_target,
            request_paste_access,
            copy_to_clipboard,
            paste_to_target,
            replace_streamed_target,
            overlay_apply_layout
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app, _event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Exit = _event {
                recording_shortcut::quill_picker_close();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_anchor(monitor_id: isize, center_x: i32, bottom_y: i32, dpi: u32) -> OverlayAnchor {
        OverlayAnchor {
            monitor_id,
            center_x,
            bottom_y,
            dpi,
        }
    }

    #[test]
    fn recording_store_keeps_ten_readable_wavs() {
        let root = std::env::temp_dir().join(format!("openwhisper-store-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        for index in 0..12_u128 {
            let id = format!("recording-{index}");
            let directory = root.join(&id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("audio.wav"), wav_header(0, 24_000)).unwrap();
            write_recording_metadata(
                &root,
                &RecordingMetadata {
                    id: id.clone(),
                    created_at_ms: index,
                    name: format!("Recording {index}"),
                    source: "microphone".to_string(),
                    mime_type: "audio/wav".to_string(),
                    file_name: "audio.wav".to_string(),
                    status: "saved".to_string(),
                    bytes: 44,
                    transcript: String::new(),
                    error: String::new(),
                },
            )
            .unwrap();
        }

        prune_recordings(&root, RECORDING_RETENTION_COUNT).unwrap();
        let retained = list_recording_metadata(&root).unwrap();
        assert_eq!(retained.len(), RECORDING_RETENTION_COUNT);
        assert_eq!(retained.first().unwrap().id, "recording-11");
        assert_eq!(retained.last().unwrap().id, "recording-2");
        for recording in retained {
            let path = root.join(recording.id).join(recording.file_name);
            assert_eq!(&fs::read(path).unwrap()[0..4], b"RIFF");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn appended_backup_is_immediately_a_valid_wav() {
        let root = std::env::temp_dir().join(format!("openwhisper-wav-test-{}", Uuid::new_v4()));
        let id = "active";
        fs::create_dir_all(root.join(id)).unwrap();
        fs::write(root.join(id).join("audio.wav"), wav_header(0, 24_000)).unwrap();
        write_recording_metadata(
            &root,
            &RecordingMetadata {
                id: id.to_string(),
                created_at_ms: 1,
                name: "Active".to_string(),
                source: "microphone".to_string(),
                mime_type: "audio/wav".to_string(),
                file_name: "audio.wav".to_string(),
                status: "recording".to_string(),
                bytes: 44,
                transcript: String::new(),
                error: String::new(),
            },
        )
        .unwrap();

        append_pcm_chunk(&root, id, &[1, 2, 3, 4], 24_000).unwrap();
        let wav = fs::read(root.join(id).join("audio.wav")).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
        assert_eq!(&wav[44..], &[1, 2, 3, 4]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlay_geometry_keeps_bottom_center_anchor_on_negative_monitor() {
        let anchor = test_anchor(1, -1280, 1430, 96);

        let collapsed = overlay_window_position_for_anchor(anchor, 100, 28);
        let expanded = overlay_window_position_for_anchor(anchor, 360, 520);

        assert_eq!(collapsed, PhysicalPosition::new(-1330, 1402));
        assert_eq!(expanded, PhysicalPosition::new(-1460, 910));
        assert_eq!(collapsed.x + 50, anchor.center_x);
        assert_eq!(expanded.x + 180, anchor.center_x);
        assert_eq!(collapsed.y + 28, anchor.bottom_y);
        assert_eq!(expanded.y + 520, anchor.bottom_y);
    }

    #[test]
    fn overlay_geometry_handles_monitor_above_primary() {
        let anchor = test_anchor(1, 960, -10, 96);
        assert_eq!(
            overlay_window_position_for_anchor(anchor, 188, 72),
            PhysicalPosition::new(866, -82)
        );
    }

    #[test]
    fn transcription_model_allowlist_matches_settings_ui() {
        assert!(is_supported_transcription_model("whisper-base-local"));
        assert!(is_supported_transcription_model("whisper-tiny-local"));
        assert!(is_supported_transcription_model("whisper-turbo-local"));
        assert!(is_supported_transcription_model("gpt-4o-mini-transcribe"));
        assert!(is_supported_transcription_model("gpt-4o-transcribe"));
        assert!(is_supported_transcription_model("gpt-realtime-transcribe"));
        assert!(!is_supported_transcription_model("gpt-5.4"));
    }

    #[test]
    fn new_settings_default_to_local_transcription_without_cloud_refinement() {
        for settings in [
            AppSettings::default(),
            serde_json::from_value(json!({})).unwrap(),
        ] {
            assert_eq!(settings.transcription_model, "whisper-base-local");
            assert!(settings.openai_api_key.is_empty());
            assert!(!settings.refine_enabled);
        }
    }

    #[test]
    fn local_models_disable_previously_enabled_cloud_refinement() {
        for model in [
            "whisper-base-local",
            "whisper-tiny-local",
            "whisper-turbo-local",
        ] {
            let settings = AppSettings {
                transcription_model: model.to_string(),
                refine_enabled: true,
                ..AppSettings::default()
            };
            assert!(!refinement_enabled_for_model(model, true));
            assert!(!frontend_settings_from(&settings).refine_enabled);
        }
    }

    #[test]
    fn cloud_models_keep_explicit_refinement_preferences() {
        for model in [
            "gpt-4o-mini-transcribe",
            "gpt-4o-transcribe",
            "gpt-realtime-transcribe",
        ] {
            assert!(refinement_enabled_for_model(model, true));
            assert!(!refinement_enabled_for_model(model, false));
        }
    }

    #[test]
    fn settings_without_idle_pill_preference_keep_showing_it() {
        let settings: AppSettings = serde_json::from_value(json!({})).unwrap();

        assert!(settings.show_idle_pill);
        assert!(frontend_settings_from(&settings).show_idle_pill);
    }

    #[test]
    fn settings_without_shortcut_preference_keep_hold_to_talk() {
        let settings: AppSettings = serde_json::from_value(json!({})).unwrap();

        assert!(settings.hold_to_talk);
        assert!(frontend_settings_from(&settings).hold_to_talk);
        assert_eq!(settings.recording_shortcut, default_recording_shortcut());
        assert_eq!(
            frontend_settings_from(&settings).recording_shortcut,
            settings.recording_shortcut
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn right_control_shortcut_always_uses_hold_mode() {
        assert!(hold_to_talk_for_shortcut("RightControl", false));
        assert!(hold_to_talk_for_shortcut("RightControl", true));
        assert!(!hold_to_talk_for_shortcut("Control+Alt+Space", false));
        let settings = AppSettings {
            recording_shortcut: "RightControl".to_string(),
            hold_to_talk: false,
            ..AppSettings::default()
        };
        assert!(frontend_settings_from(&settings).hold_to_talk);
        assert!(recording_shortcut_tooltip(&settings).contains("Right Control"));
    }

    #[test]
    fn settings_preserve_selected_shortcut_without_exposing_api_key() {
        let settings: AppSettings = serde_json::from_value(json!({
            "recordingShortcut": "Control+Alt+KeyW",
            "openaiApiKey": Uuid::new_v4().to_string()
        }))
        .unwrap();
        let frontend = serde_json::to_value(frontend_settings_from(&settings)).unwrap();
        assert_eq!(frontend["recordingShortcut"], "Control+Alt+KeyW");
        assert!(frontend["recordingShortcutError"].is_null());
        assert!(frontend.get("openaiApiKey").is_none());
        assert_eq!(
            serde_json::to_value(&settings).unwrap()["recordingShortcut"],
            "Control+Alt+KeyW"
        );
    }

    #[test]
    fn shortcut_and_pill_changes_keep_the_live_worker() {
        let previous = AppSettings::default();
        let updated = AppSettings {
            recording_shortcut: "Control+Alt+KeyW".to_string(),
            hold_to_talk: !previous.hold_to_talk,
            show_idle_pill: !previous.show_idle_pill,
            ..previous.clone()
        };
        assert!(!worker_settings_changed(&previous, &updated));
        for updated in [
            AppSettings {
                openai_api_key: Uuid::new_v4().to_string(),
                ..previous.clone()
            },
            AppSettings {
                transcription_model: "whisper-tiny-local".into(),
                ..previous.clone()
            },
            AppSettings {
                transcription_prompt: "test prompt".into(),
                ..previous.clone()
            },
            AppSettings {
                refine_enabled: !previous.refine_enabled,
                ..previous.clone()
            },
            AppSettings {
                refine_prompt: "test refinement".into(),
                ..previous.clone()
            },
        ] {
            assert!(worker_settings_changed(&previous, &updated));
        }
    }

    #[test]
    fn destination_dpi_alone_determines_physical_size_and_position() {
        let layout = OverlayLayoutSpec {
            width: 92.0,
            height: 28.0,
            anchor_offset_y: 2.0,
        };
        let at_125 = overlay_placement(test_anchor(1, 1000, 900, 120), layout);
        let at_200 = overlay_placement(test_anchor(2, 2000, 1700, 192), layout);

        assert_eq!(at_125.size, PhysicalSize::new(115, 35));
        assert_eq!(at_125.position, PhysicalPosition::new(943, 868));
        assert_eq!(at_200.size, PhysicalSize::new(184, 56));
        assert_eq!(at_200.position, PhysicalPosition::new(1908, 1648));
    }

    #[test]
    fn expanded_layout_grows_around_compact_anchor() {
        let anchor = test_anchor(1, 960, 1070, 96);
        let compact = overlay_window_position_for_anchor(anchor, 38, 14);
        let expanded = overlay_window_position_for_anchor_offset(anchor, 92, 26, 6);

        assert_eq!(compact.x + 19, expanded.x + 46);
        assert_eq!(compact.y + 7, expanded.y + 13);
    }

    #[test]
    fn secondary_monitor_anchor_applies_configured_offset() {
        assert_eq!(overlay_bottom_anchor(1800, 1704, true, 192), 1694);
        assert_eq!(overlay_bottom_anchor(0, -60, false, 120), -67);
        assert_eq!(overlay_bottom_anchor(1080, 1080, false, 144), 1070);
    }

    #[test]
    fn passive_overlay_reveals_never_take_keyboard_focus() {
        assert!(!OverlayRevealIntent::Passive.should_focus());
        assert!(OverlayRevealIntent::Interactive.should_focus());
    }

    #[test]
    fn repeated_entries_from_every_direction_commit_once_to_one_fixed_placement() {
        let center = test_anchor(1, 960, 1070, 96);
        let destinations = [
            test_anchor(2, -960, 1070, 120), // enter from the left
            test_anchor(3, 2880, 1430, 144), // enter from the right
            test_anchor(4, 960, -10, 192),   // enter from above
            test_anchor(5, 960, 3230, 168),  // enter from below
        ];
        let layout = OverlayLayoutSpec {
            width: 92.0,
            height: 28.0,
            anchor_offset_y: 2.0,
        };

        for destination in destinations {
            let expected = overlay_placement(destination, layout);
            let mut committed = Some(center);
            let mut transition = MonitorTransitionDebouncer::default();
            let mut destination_commits = Vec::new();

            for _ in 0..50 {
                // Approach noise from either side of the monitor boundary must
                // not move the already-visible pill.
                for observed in [destination, center, destination, center] {
                    assert_eq!(transition.observe(committed, observed), None);
                }

                let mut commits_this_entry = 0;
                for _ in 0..MONITOR_SWITCH_STABLE_SAMPLES {
                    if let Some(target) = transition.observe(committed, destination) {
                        commits_this_entry += 1;
                        destination_commits.push(overlay_placement(target, layout));
                        committed = Some(target);
                        transition.reset();
                    }
                }
                for _ in 0..20 {
                    assert_eq!(transition.observe(committed, destination), None);
                }
                assert_eq!(commits_this_entry, 1);

                for _ in 0..MONITOR_SWITCH_STABLE_SAMPLES {
                    if let Some(target) = transition.observe(committed, center) {
                        committed = Some(target);
                        transition.reset();
                    }
                }
                assert_eq!(committed, Some(center));
            }

            assert_eq!(destination_commits.len(), 50);
            assert!(destination_commits
                .iter()
                .all(|placement| *placement == expected));
        }
    }

    // ---- quill settings ---------------------------------------------------------------

    fn quill_test_state(settings: AppSettings) -> AppState {
        AppState {
            worker: Mutex::new(None),
            quill: Mutex::new(None),
            settings: std::sync::Mutex::new(settings),
            settings_update: Mutex::new(()),
            #[cfg(target_os = "macos")]
            recording_shortcut: std::sync::Mutex::new(recording_shortcut::Runtime::default()),
            overlay_anchor: std::sync::Mutex::new(None),
            overlay_layout: std::sync::Mutex::new(None),
            overlay_layout_locked: AtomicBool::new(false),
        }
    }

    fn quill_test_dir() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("openwhisper-quill-settings-{}", Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    fn quill_test_file(dir: &std::path::Path, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let file = dir.join("quill");
        fs::write(&file, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    #[test]
    fn quill_command_settings_missing_field_loads_the_default() {
        let settings: AppSettings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(settings.quill_command, default_quill_command());
        assert_eq!(AppSettings::default().quill_command, default_quill_command());

        assert_eq!(
            default_quill_command_for(Some(std::ffi::OsString::from("/Users/me"))),
            "/Users/me/.local/bin/quill"
        );
        assert_eq!(default_quill_command_for(None), "");
        assert_eq!(default_quill_command_for(Some(std::ffi::OsString::new())), "");
    }

    #[test]
    fn quill_command_settings_round_trip_keeps_the_value() {
        let settings = AppSettings {
            quill_command: "/opt/tools/quill".to_string(),
            ..AppSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["quillCommand"], "/opt/tools/quill");
        let loaded: AppSettings = serde_json::from_value(value).unwrap();
        assert_eq!(loaded.quill_command, "/opt/tools/quill");
        assert_eq!(
            serde_json::to_value(frontend_settings_from(&loaded)).unwrap()["quillCommand"],
            "/opt/tools/quill"
        );
    }

    #[test]
    fn quill_command_settings_failing_value_surfaces_the_validation_string() {
        let dir = quill_test_dir();
        let good = quill_test_file(&dir, 0o700);
        let state = quill_test_state(AppSettings::default());

        let ok = frontend_settings_with_runtime(
            &state,
            &AppSettings {
                quill_command: good.to_string_lossy().to_string(),
                ..AppSettings::default()
            },
        )
        .unwrap();
        assert_eq!(ok.quill_command_error, None);
        assert!(serde_json::to_value(&ok).unwrap()["quillCommandError"].is_null());

        let relative = frontend_settings_with_runtime(
            &state,
            &AppSettings {
                quill_command: "quill".to_string(),
                ..AppSettings::default()
            },
        )
        .unwrap();
        assert_eq!(
            relative.quill_command_error.as_deref(),
            Some("Quill command must be an absolute path.")
        );

        fs::set_permissions(&good, std::os::unix::fs::PermissionsExt::from_mode(0o770)).unwrap();
        let writable = frontend_settings_with_runtime(
            &state,
            &AppSettings {
                quill_command: good.to_string_lossy().to_string(),
                ..AppSettings::default()
            },
        )
        .unwrap();
        assert_eq!(
            writable.quill_command_error,
            Some(format!(
                "Quill command is group-writable: {}",
                fs::canonicalize(&good).unwrap().display()
            ))
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quill_command_settings_save_stores_the_trimmed_value_without_rejecting_it() {
        let mut settings = AppSettings::default();
        let before = settings.quill_command.clone();
        apply_quill_command(&mut settings, None);
        assert_eq!(settings.quill_command, before);
        apply_quill_command(&mut settings, Some("  /opt/quill  ".to_string()));
        assert_eq!(settings.quill_command, "/opt/quill");
        apply_quill_command(&mut settings, Some("not/absolute".to_string()));
        assert_eq!(settings.quill_command, "not/absolute", "a failing value is stored, not rejected");
        apply_quill_command(&mut settings, Some("   ".to_string()));
        assert_eq!(settings.quill_command, "");
    }

    #[test]
    fn quill_command_settings_change_alone_keeps_the_live_worker() {
        let previous = AppSettings::default();
        let updated = AppSettings {
            quill_command: "/somewhere/else/quill".to_string(),
            ..previous.clone()
        };
        assert!(!worker_settings_changed(&previous, &updated));
    }

    #[test]
    fn quill_command_settings_add_no_other_quill_setting() {
        let frontend = serde_json::to_value(frontend_settings_from(&AppSettings::default())).unwrap();
        let mut quill_keys: Vec<&str> = frontend
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| key.to_lowercase().contains("quill"))
            .map(String::as_str)
            .collect();
        quill_keys.sort_unstable();
        assert_eq!(quill_keys, ["quillCommand", "quillCommandError", "quillDefaultMode", "quillDictationCleanup", "quillEnabled"]);
        let stored = serde_json::to_value(AppSettings::default()).unwrap();
        let stored_keys: Vec<&str> = stored
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| key.to_lowercase().contains("quill"))
            .map(String::as_str)
            .collect();
        assert_eq!(stored_keys, ["quillCommand", "quillDefaultMode", "quillDictationCleanup", "quillEnabled"]);
    }

    // ---- quill hook surfaces --------------------------------------------------------------

    use std::{cell::RefCell, rc::Rc};

    #[cfg(target_os = "macos")]
    async fn quill_hook_off_run(enabled: bool) -> (Result<(), QuillCommandError>, bool, bool) {
        let dir = quill_test_dir();
        let command = quill_test_file(&dir, 0o700);
        let state = quill_test_state(AppSettings {
            quill_command: command.to_string_lossy().to_string(),
            quill_enabled: enabled,
            ..AppSettings::default()
        });
        let called = AtomicBool::new(false);
        let result = with_quill(&state, |_, _, _| {
            called.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        })
        .await;
        let slot_empty = state.quill.lock().await.is_none();
        fs::remove_dir_all(dir).unwrap();
        (result, called.load(Ordering::SeqCst), slot_empty)
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn quill_hook_off_with_quill_returns_disabled_before_the_closure() {
        let (result, called, slot_empty) = quill_hook_off_run(false).await;
        assert_eq!(result.unwrap_err().code, quill_client::QuillErrorCode::Disabled);
        assert!(!called, "nothing is leased or spawned while the hook is off");
        assert!(slot_empty);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn quill_hook_off_with_quill_runs_the_closure_when_enabled() {
        let (result, called, slot_empty) = quill_hook_off_run(true).await;
        assert!(result.is_ok());
        assert!(called);
        assert!(slot_empty, "the closure here spawns nothing");
    }

    const QUILL_TEST_MODELS: [&str; 6] = [
        "whisper-base-local",
        "whisper-tiny-local",
        "whisper-turbo-local",
        "gpt-4o-mini-transcribe",
        "gpt-4o-transcribe",
        "gpt-realtime-transcribe",
    ];

    #[test]
    fn quill_settings_missing_keys_load_the_defaults() {
        let settings: AppSettings = serde_json::from_str(
            r#"{"openaiApiKey":"","transcriptionModel":"gpt-4o-transcribe","refineEnabled":true,"recordingShortcut":"Control+Alt+Space"}"#,
        )
        .unwrap();
        assert!(!settings.quill_enabled);
        assert_eq!(settings.quill_default_mode, "task-claude");
        assert!(!settings.quill_dictation_cleanup);
        let defaults = AppSettings::default();
        assert!(!defaults.quill_enabled);
        assert_eq!(defaults.quill_default_mode, "task-claude");
        assert!(!defaults.quill_dictation_cleanup);
    }

    #[test]
    fn quill_settings_save_round_trips() {
        let settings = AppSettings {
            quill_enabled: true,
            quill_default_mode: "proofread".to_string(),
            quill_dictation_cleanup: true,
            ..AppSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["quillEnabled"], true);
        assert_eq!(value["quillDefaultMode"], "proofread");
        assert_eq!(value["quillDictationCleanup"], true);
        let loaded: AppSettings = serde_json::from_value(value).unwrap();
        assert!(loaded.quill_enabled);
        assert_eq!(loaded.quill_default_mode, "proofread");
        assert!(loaded.quill_dictation_cleanup);
        let frontend = serde_json::to_value(frontend_settings_from(&loaded)).unwrap();
        assert_eq!(frontend["quillEnabled"], true);
        assert_eq!(frontend["quillDefaultMode"], "proofread");
        assert_eq!(frontend["quillDictationCleanup"], true);
    }

    #[test]
    fn quill_settings_save_applies_the_three_options() {
        let mut settings = AppSettings::default();
        apply_quill_settings(&mut settings, None, None, None).unwrap();
        assert!(!settings.quill_enabled);
        assert_eq!(settings.quill_default_mode, "task-claude");
        apply_quill_settings(&mut settings, Some("dictation-cleanup".to_string()), Some(true), Some(true))
            .unwrap();
        assert!(settings.quill_enabled);
        assert!(settings.quill_dictation_cleanup);
        assert_eq!(settings.quill_default_mode, "dictation-cleanup");
    }

    #[test]
    fn quill_settings_refuse_a_bad_default_mode() {
        for bad in ["", "Task-claude", "task_claude", "task claude", "tâsk", &"a".repeat(65)] {
            let mut settings = AppSettings::default();
            assert!(
                apply_quill_settings(&mut settings, Some(bad.to_string()), Some(true), Some(true)).is_err(),
                "{bad:?} must be refused"
            );
            assert_eq!(settings.quill_default_mode, "task-claude", "a refused save changes nothing");
            assert!(!settings.quill_enabled, "a refused save changes nothing");
        }
        for good in ["a", "task-claude", "mode-2", &"z".repeat(64)] {
            let mut settings = AppSettings::default();
            apply_quill_settings(&mut settings, Some(good.to_string()), None, None).unwrap();
            assert_eq!(settings.quill_default_mode, good);
        }
    }

    #[test]
    fn quill_settings_model_streams_only_for_realtime() {
        for model in QUILL_TEST_MODELS {
            assert!(is_supported_transcription_model(model));
            assert_eq!(model_streams(model), model == "gpt-realtime-transcribe", "{model}");
        }
        assert!(!model_streams(""));
        assert!(!model_streams("gpt-realtime"));
    }

    #[test]
    fn quill_settings_cleanup_effective_needs_enabled_valid_and_cleanup() {
        for enabled in [false, true] {
            for valid in [false, true] {
                for cleanup in [false, true] {
                    let settings = AppSettings {
                        quill_enabled: enabled,
                        quill_dictation_cleanup: cleanup,
                        ..AppSettings::default()
                    };
                    let error = (!valid).then_some("Quill command must be an absolute path.");
                    assert_eq!(
                        cleanup_effective(&settings, error),
                        enabled && valid && cleanup,
                        "enabled={enabled} valid={valid} cleanup={cleanup}"
                    );
                }
            }
        }
    }

    #[test]
    fn quill_settings_refine_enabled_follows_the_settings_matrix() {
        // (quillEnabled, command valid, cleanup, model streams): None is "as today".
        let matrix: [(bool, Option<bool>, Option<bool>, Option<bool>, Option<bool>); 5] = [
            (false, None, None, None, None),
            (true, Some(false), None, None, None),
            (true, Some(true), Some(false), None, None),
            (true, Some(true), Some(true), Some(false), Some(false)),
            (true, Some(true), Some(true), Some(true), None),
        ];
        let any = |value: Option<bool>| value.map_or(vec![false, true], |value| vec![value]);
        let mut rows = 0;
        for (enabled, valid, cleanup, streams, sent) in matrix {
            for valid in any(valid) {
                for cleanup in any(cleanup) {
                    for model in QUILL_TEST_MODELS {
                        let model_is_streaming = model == "gpt-realtime-transcribe";
                        if streams.is_some_and(|streams| streams != model_is_streaming) {
                            continue;
                        }
                        for requested in [false, true] {
                            let settings = AppSettings {
                                transcription_model: model.to_string(),
                                refine_enabled: requested,
                                quill_enabled: enabled,
                                quill_dictation_cleanup: cleanup,
                                ..AppSettings::default()
                            };
                            let error = (!valid).then_some("Quill command must be an absolute path.");
                            let expected =
                                sent.unwrap_or_else(|| refinement_enabled_for_model(model, requested));
                            assert_eq!(
                                session_refine_enabled(&settings, error),
                                expected,
                                "enabled={enabled} valid={valid} cleanup={cleanup} model={model} requested={requested}"
                            );
                            rows += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(rows, 2 * 2 * 6 * 2 + 2 * 6 * 2 + 6 * 2 + 5 * 2 + 2);
    }

    #[test]
    fn quill_settings_quill_only_change_keeps_the_live_worker() {
        let previous = AppSettings::default();
        let updated = AppSettings {
            quill_enabled: true,
            quill_default_mode: "proofread".to_string(),
            quill_dictation_cleanup: true,
            ..previous.clone()
        };
        assert!(!worker_settings_changed(&previous, &updated));
    }

    #[derive(Default)]
    struct QuillTestClipboard {
        text: Option<String>,
        log: Rc<RefCell<Vec<String>>>,
    }

    impl QuillClipboard for QuillTestClipboard {
        fn text(&mut self) -> Option<String> {
            self.text.clone()
        }

        fn set_text(&mut self, text: &str) -> bool {
            self.log.borrow_mut().push(format!("set {text}"));
            self.text = Some(text.to_string());
            true
        }
    }

    fn quill_clipboard_slot(saved: Option<&str>) -> std::sync::Mutex<Option<String>> {
        let slot = std::sync::Mutex::new(None);
        let mut clipboard = QuillTestClipboard {
            text: saved.map(str::to_string),
            ..QuillTestClipboard::default()
        };
        quill_clipboard_hold_with(&slot, Some(&mut clipboard)).unwrap();
        slot
    }

    /// Releases with the clipboard holding `current`; returns the log and the final text.
    async fn quill_clipboard_run(
        slot: &std::sync::Mutex<Option<String>>,
        restore: bool,
        pasted: &str,
        current: Option<&str>,
    ) -> (Vec<String>, Option<String>) {
        let log = Rc::new(RefCell::new(vec![]));
        let clipboard = Rc::new(RefCell::new(None));
        let sleep_log = log.clone();
        let made = clipboard.clone();
        let make_log = log.clone();
        quill_clipboard_release_with(
            slot,
            restore,
            pasted,
            |delay| {
                sleep_log.borrow_mut().push(format!("sleep {}", delay.as_millis()));
                async {}
            },
            || {
                make_log.borrow_mut().push("read".to_string());
                Some(QuillClipboardProbe {
                    inner: QuillTestClipboard {
                        text: current.map(str::to_string),
                        log: make_log.clone(),
                    },
                    out: made,
                })
            },
        )
        .await
        .unwrap();
        let text = clipboard.borrow().clone();
        let log = log.borrow().clone();
        (log, text.or(current.map(str::to_string)))
    }

    struct QuillClipboardProbe {
        inner: QuillTestClipboard,
        out: Rc<RefCell<Option<String>>>,
    }

    impl QuillClipboard for QuillClipboardProbe {
        fn text(&mut self) -> Option<String> {
            self.inner.text()
        }

        fn set_text(&mut self, text: &str) -> bool {
            *self.out.borrow_mut() = Some(text.to_string());
            self.inner.set_text(text)
        }
    }

    #[tokio::test]
    async fn quill_clipboard_release_waits_300_ms_before_restoring() {
        let slot = quill_clipboard_slot(Some("original"));
        let (log, text) = quill_clipboard_run(&slot, true, "pasted", Some("pasted")).await;
        assert_eq!(log, vec!["sleep 300", "read", "set original"]);
        assert_eq!(text.as_deref(), Some("original"));
        assert!(slot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn quill_clipboard_restores_after_an_inserted_paste() {
        let slot = quill_clipboard_slot(Some("what the user had copied"));
        let (_, text) = quill_clipboard_run(&slot, true, "rewritten transcript", Some("rewritten transcript")).await;
        assert_eq!(text.as_deref(), Some("what the user had copied"));
        assert!(slot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn quill_clipboard_no_restore_when_restore_is_false() {
        let slot = quill_clipboard_slot(Some("original"));
        let (log, text) = quill_clipboard_run(&slot, false, "pasted", Some("pasted")).await;
        assert!(log.is_empty(), "nothing is read or written: {log:?}");
        assert_eq!(text.as_deref(), Some("pasted"));
        assert!(slot.lock().unwrap().is_none(), "release always empties the slot");
    }

    #[tokio::test]
    async fn quill_clipboard_no_restore_when_the_user_copied_since() {
        let slot = quill_clipboard_slot(Some("original"));
        let (log, text) = quill_clipboard_run(&slot, true, "pasted", Some("a newer copy")).await;
        assert!(!log.iter().any(|line| line.starts_with("set")), "{log:?}");
        assert_eq!(text.as_deref(), Some("a newer copy"));
        assert!(slot.lock().unwrap().is_none());
        let slot = quill_clipboard_slot(Some("original"));
        let (log, _) = quill_clipboard_run(&slot, true, "pasted", None).await;
        assert!(!log.iter().any(|line| line.starts_with("set")), "{log:?}");
    }

    #[tokio::test]
    async fn quill_clipboard_non_text_saves_nothing() {
        let slot = quill_clipboard_slot(None);
        assert!(slot.lock().unwrap().is_none());
        let (log, _) = quill_clipboard_run(&slot, true, "pasted", Some("pasted")).await;
        assert!(log.is_empty(), "{log:?}");
        let unavailable = std::sync::Mutex::new(Some("stale".to_string()));
        quill_clipboard_hold_with::<QuillTestClipboard>(&unavailable, None).unwrap();
        assert!(unavailable.lock().unwrap().is_none(), "no clipboard saves nothing");
    }

    #[tokio::test]
    async fn quill_clipboard_second_hold_replaces_the_first() {
        let slot = quill_clipboard_slot(Some("first"));
        let mut clipboard = QuillTestClipboard {
            text: Some("second".to_string()),
            ..QuillTestClipboard::default()
        };
        quill_clipboard_hold_with(&slot, Some(&mut clipboard)).unwrap();
        assert_eq!(slot.lock().unwrap().as_deref(), Some("second"));
        let (_, text) = quill_clipboard_run(&slot, true, "pasted", Some("pasted")).await;
        assert_eq!(text.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn quill_clipboard_release_always_empties_the_slot() {
        for (restore, current) in [(true, Some("pasted")), (true, Some("other")), (false, Some("pasted")), (true, None)] {
            let slot = quill_clipboard_slot(Some("original"));
            quill_clipboard_run(&slot, restore, "pasted", current).await;
            assert!(slot.lock().unwrap().is_none(), "restore={restore} current={current:?}");
        }
        let slot = quill_clipboard_slot(Some("original"));
        quill_clipboard_release_with::<QuillTestClipboard, _, _, _>(
            &slot,
            true,
            "pasted",
            |_| async {},
            || None,
        )
        .await
        .unwrap();
        assert!(slot.lock().unwrap().is_none(), "an unavailable clipboard still empties the slot");
    }

    #[test]
    fn quill_clipboard_commands_return_no_text() {
        fn returns_nothing<F: std::future::Future<Output = Result<(), String>>>(_: fn(bool, String) -> F) {}
        let _: fn() -> Result<(), String> = quill_clipboard_hold;
        returns_nothing(quill_clipboard_release);
    }

    #[test]
    fn quill_hook_off_revert_arm_is_refused() {
        let off = quill_test_state(AppSettings { quill_enabled: false, ..AppSettings::default() });
        assert!(!quill_revert_arm_allowed(&off), "armed the revert hotkey while the hook is off");
        let on = quill_test_state(AppSettings { quill_enabled: true, ..AppSettings::default() });
        assert!(quill_revert_arm_allowed(&on));
    }

    #[test]
    fn quill_settings_turning_the_hook_off_closes_the_revert_window() {
        let with = |enabled: bool| AppSettings { quill_enabled: enabled, ..AppSettings::default() };
        assert!(quill_turned_off(&with(true), &with(false)));
        assert!(!quill_turned_off(&with(true), &with(true)));
        assert!(!quill_turned_off(&with(false), &with(false)));
        assert!(!quill_turned_off(&with(false), &with(true)));
    }

    #[test]
    fn quill_settings_a_save_that_breaks_the_command_closes_the_revert_window() {
        assert!(quill_command_broke(None, Some("not executable")));
        assert!(!quill_command_broke(None, None));
        assert!(!quill_command_broke(Some("missing"), Some("not executable")));
        assert!(!quill_command_broke(Some("missing"), None));
    }
}
