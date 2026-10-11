use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::AppHandle;

const RIGHT_CONTROL_KEY: u16 = 62;
const RIGHT_CONTROL: u64 = 0x2000;
const OTHER_MODIFIERS: u64 = 0x1 | (1 << 17) | (1 << 19) | (1 << 20) | (1 << 23);
const PERMISSION_RETRY: Duration = Duration::from_secs(2);
pub(crate) const PERMISSION_ERROR: &str = "Right Control needs Input Monitoring access. Enable OpenWhisper in System Settings > Privacy & Security > Input Monitoring. It will retry automatically; Accessibility access may already allow it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Start,
    Stop,
    Cancel,
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Recording,
    Blocked,
}

#[derive(Default)]
struct Controller {
    phase: Phase,
    right_down: bool,
    other_keys_down: usize,
}

impl Controller {
    #[cfg(test)]
    fn event(&mut self, event_type: u32, key: u16, flags: u64) -> Option<Action> {
        self.event_with_repeat(event_type, key, flags, false)
    }

    fn event_from_source(
        &mut self,
        event_type: u32,
        key: u16,
        flags: u64,
        repeat: bool,
        source_tag: i64,
    ) -> Option<Action> {
        if source_tag == super::macos_text_inserter::SYNTHETIC_PASTE_EVENT_TAG {
            return None;
        }
        self.event_with_repeat(event_type, key, flags, repeat)
    }

    fn event_with_repeat(
        &mut self,
        event_type: u32,
        key: u16,
        flags: u64,
        repeat: bool,
    ) -> Option<Action> {
        match event_type {
            10 => {
                if !repeat {
                    self.other_keys_down = self.other_keys_down.saturating_add(1);
                }
                self.block()
            }
            11 => {
                self.other_keys_down = self.other_keys_down.saturating_sub(1);
                None
            }
            12 => {
                let was_down = self.right_down;
                self.right_down = flags & RIGHT_CONTROL != 0;
                if !self.right_down {
                    let action = matches!(self.phase, Phase::Recording).then_some(Action::Stop);
                    self.phase = Phase::Idle;
                    return action;
                }
                if flags & OTHER_MODIFIERS != 0 || key != RIGHT_CONTROL_KEY {
                    return self.block();
                }
                if !was_down {
                    if self.other_keys_down == 0 {
                        self.phase = Phase::Recording;
                        return Some(Action::Start);
                    }
                    self.phase = Phase::Blocked;
                }
                None
            }
            _ => None,
        }
    }

    fn block(&mut self) -> Option<Action> {
        let action = matches!(self.phase, Phase::Recording).then_some(Action::Cancel);
        if self.right_down {
            self.phase = Phase::Blocked;
        }
        action
    }

    fn reset(&mut self) -> Option<Action> {
        let action = matches!(self.phase, Phase::Recording).then_some(Action::Cancel);
        *self = Self::default();
        action
    }

    /// The tap can miss a key-up (secure input fields, app switchers, a tap that
    /// was disabled mid-press). A leaked count would classify every later Right
    /// Control press as a chord and leave the shortcut dead until relaunch, so
    /// each fresh press trusts the hardware state over the running count.
    fn sync_keys_down(&mut self, physical: usize) {
        self.other_keys_down = physical;
    }

    fn is_fresh_press(&self, event_type: u32, key: u16, flags: u64) -> bool {
        event_type == 12 && key == RIGHT_CONTROL_KEY && flags & RIGHT_CONTROL != 0 && !self.right_down
    }
}

pub(crate) struct Monitor {
    running: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub(crate) fn start(app: &AppHandle) -> Result<Self, String> {
        if !unsafe { CGPreflightListenEventAccess() } {
            app.run_on_main_thread(|| unsafe {
                if !CGPreflightListenEventAccess() {
                    CGRequestListenEventAccess();
                }
            })
            .map_err(|error| format!("Could not request Input Monitoring access: {error}"))?;
        }
        let running = Arc::new(AtomicBool::new(true));
        let held = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (sender, receiver) = mpsc::sync_channel(1);
        let task_running = running.clone();
        let task_held = held.clone();
        let task_error = error.clone();
        let task_app = app.clone();
        let thread = thread::Builder::new()
            .name("right-control-shortcut".to_string())
            .spawn(move || run_monitor(task_app, task_running, task_held, task_error, sender))
            .map_err(|error| format!("Could not start Right Control shortcut: {error}"))?;
        let monitor = Self {
            running,
            held,
            error,
            thread: Some(thread),
        };
        receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Right Control shortcut did not initialize.".to_string())??;
        Ok(monitor)
    }

    pub(crate) fn is_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|error| error.clone())
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

struct TapState {
    app: AppHandle,
    controller: Controller,
    held: Arc<AtomicBool>,
    tap: *mut c_void,
}

impl TapState {
    fn action(&mut self, action: Option<Action>) {
        self.held
            .store(self.controller.right_down, Ordering::Release);
        if let Some(action) = action {
            super::recording_shortcut::modifier_action(&self.app, action);
        }
    }

    fn reset(&mut self) {
        let action = self.controller.reset();
        self.action(action);
    }
}

fn update_error(app: &AppHandle, error: &Mutex<Option<String>>, next: Option<String>) {
    if let Ok(mut current) = error.lock() {
        if *current == next {
            return;
        }
        *current = next.clone();
    }
    if let Some(message) = &next {
        super::debug_log_line("shortcut", message);
    }
    super::recording_shortcut::modifier_status(app, next);
}

fn run_monitor(
    app: AppHandle,
    running: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    ready: mpsc::SyncSender<Result<(), String>>,
) {
    let mut ready = Some(ready);
    let mut state = Box::new(TapState {
        app,
        controller: Controller::default(),
        held,
        tap: ptr::null_mut(),
    });
    while running.load(Ordering::Acquire) {
        if !unsafe { CGPreflightListenEventAccess() } {
            update_error(&state.app, &error, Some(PERMISSION_ERROR.to_string()));
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(()));
            }
            thread::park_timeout(PERMISSION_RETRY);
            continue;
        }
        let source = unsafe {
            state.tap = CGEventTapCreate(
                1,
                0,
                1,
                (1 << 10) | (1 << 11) | (1 << 12),
                event_callback,
                (&mut *state as *mut TapState).cast(),
            );
            if state.tap.is_null() {
                ptr::null_mut()
            } else {
                CFMachPortCreateRunLoopSource(ptr::null(), state.tap, 0)
            }
        };
        if source.is_null() {
            if !state.tap.is_null() {
                unsafe { CFRelease(state.tap) };
                state.tap = ptr::null_mut();
            }
            let message = "Right Control could not start its keyboard listener. Check Input Monitoring access and try selecting the shortcut again.".to_string();
            update_error(&state.app, &error, Some(message.clone()));
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(message));
                return;
            }
            thread::park_timeout(PERMISSION_RETRY);
            continue;
        }
        unsafe {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CGEventTapEnable(state.tap, true);
        }
        super::debug_log_line("shortcut", "Right Control keyboard listener ready");
        update_error(&state.app, &error, None);
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let mut permission_check = Instant::now() + PERMISSION_RETRY;
        while running.load(Ordering::Acquire) {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.025, 0) };
            if !super::recording_shortcut::modifier_enabled(&state.app) {
                state.reset();
            }
            if Instant::now() >= permission_check {
                if !unsafe { CGPreflightListenEventAccess() } {
                    break;
                }
                permission_check = Instant::now() + PERMISSION_RETRY;
            }
        }
        state.reset();
        unsafe {
            CGEventTapEnable(state.tap, false);
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFMachPortInvalidate(state.tap);
            CFRelease(source);
            CFRelease(state.tap);
        }
        state.tap = ptr::null_mut();
    }
}

unsafe extern "C" fn event_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    info: *mut c_void,
) -> *mut c_void {
    if info.is_null() {
        return event;
    }
    let state = &mut *info.cast::<TapState>();
    let handled = catch_unwind(AssertUnwindSafe(|| {
        if event_type == 0xfffffffe || event_type == 0xffffffff {
            state.reset();
            CGEventTapEnable(state.tap, true);
        } else if !event.is_null() {
            if !super::recording_shortcut::modifier_enabled(&state.app) {
                state.reset();
            } else {
                let key = if event_type == 12 {
                    CGEventGetIntegerValueField(event, 9) as u16
                } else {
                    0
                };
                let repeat = event_type == 10 && CGEventGetIntegerValueField(event, 8) != 0;
                let flags = CGEventGetFlags(event);
                let fresh_press = state.controller.is_fresh_press(event_type, key, flags);
                if fresh_press {
                    state.controller.sync_keys_down(physical_keys_down());
                }
                let action = state.controller.event_from_source(
                    event_type,
                    key,
                    flags,
                    repeat,
                    CGEventGetIntegerValueField(event, 42),
                );
                if fresh_press && action.is_none() {
                    super::debug_log_line(
                        "shortcut",
                        "Right Control press ignored: another key or modifier is down",
                    );
                }
                state.action(action);
            }
        }
    }));
    if handled.is_err() {
        state.controller = Controller::default();
        state.held.store(false, Ordering::Release);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            super::recording_shortcut::modifier_action(&state.app, Action::Cancel);
        }));
    }
    event
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGEventTapCreate(
        location: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enabled: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CGEventGetFlags(event: *mut c_void) -> u64;
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
}

const HID_SYSTEM_STATE: i32 = 1;
const MODIFIER_KEYCODES: std::ops::RangeInclusive<u16> = 54..=63;

fn physical_keys_down() -> usize {
    (0u16..128)
        .filter(|key| !MODIFIER_KEYCODES.contains(key))
        .filter(|key| unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, *key) })
        .count()
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRelease(object: *const c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(controller: &mut Controller) -> Option<Action> {
        controller.event(12, RIGHT_CONTROL_KEY, RIGHT_CONTROL | (1 << 18))
    }

    #[test]
    fn isolated_right_control_starts_at_button_down() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
    }

    #[test]
    fn immediate_control_chord_cancels_startup_without_orphan_stop() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(controller.event(10, 8, RIGHT_CONTROL), Some(Action::Cancel));
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
    }

    #[test]
    fn release_stops_only_once() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(
            controller.event(12, RIGHT_CONTROL_KEY, 0),
            Some(Action::Stop)
        );
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
    }

    #[test]
    fn synthetic_paste_does_not_cancel_or_block_the_next_hold() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        for event_type in [10, 11] {
            assert_eq!(
                controller.event_from_source(
                    event_type,
                    9,
                    1 << 20,
                    false,
                    super::super::macos_text_inserter::SYNTHETIC_PASTE_EVENT_TAG,
                ),
                None
            );
        }
        assert_eq!(
            controller.event(12, RIGHT_CONTROL_KEY, 0),
            Some(Action::Stop)
        );
        assert_eq!(down(&mut controller), Some(Action::Start));
    }

    #[test]
    fn lost_key_up_does_not_block_the_shortcut_forever() {
        let mut controller = Controller::default();
        assert_eq!(controller.event(10, 8, 0), None);
        assert_eq!(down(&mut controller), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
        assert!(controller.is_fresh_press(12, RIGHT_CONTROL_KEY, RIGHT_CONTROL));
        controller.sync_keys_down(0);
        assert_eq!(down(&mut controller), Some(Action::Start));
    }

    #[test]
    fn a_key_really_held_still_blocks_the_press() {
        let mut controller = Controller::default();
        controller.sync_keys_down(1);
        assert_eq!(down(&mut controller), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
    }

    #[test]
    fn short_tap_and_control_chord_do_not_leave_orphan_recordings() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(
            controller.event(12, RIGHT_CONTROL_KEY, 0),
            Some(Action::Stop)
        );
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(controller.event(10, 8, RIGHT_CONTROL), Some(Action::Cancel));
        assert_eq!(controller.event(11, 8, RIGHT_CONTROL), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
    }

    #[test]
    fn control_chord_after_start_cancels_without_orphan_stop() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(controller.event(10, 8, RIGHT_CONTROL), Some(Action::Cancel));
        assert_eq!(
            controller.event_with_repeat(10, 8, RIGHT_CONTROL, true),
            None
        );
        assert_eq!(controller.event(11, 8, RIGHT_CONTROL), None);
        assert_eq!(down(&mut controller), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
        assert_eq!(down(&mut controller), Some(Action::Start));
    }

    #[test]
    fn left_control_alone_and_both_controls_do_not_start() {
        let mut controller = Controller::default();
        assert_eq!(controller.event(12, 59, 0x1 | (1 << 18)), None);
        assert_eq!(
            controller.event(12, RIGHT_CONTROL_KEY, RIGHT_CONTROL | 0x1 | (1 << 18)),
            None
        );
        assert_eq!(controller.event(12, 59, RIGHT_CONTROL | (1 << 18)), None);
        assert_eq!(down(&mut controller), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
    }

    #[test]
    fn other_modifiers_cancel_and_remain_blocked_until_release() {
        for (key, modifier) in [
            (59, 0x1),
            (56, 1 << 17),
            (58, 1 << 19),
            (55, 1 << 20),
            (63, 1 << 23),
        ] {
            let mut controller = Controller::default();
            assert_eq!(down(&mut controller), Some(Action::Start));
            assert_eq!(
                controller.event(12, key, RIGHT_CONTROL | modifier),
                Some(Action::Cancel)
            );
            assert_eq!(controller.event(12, key, RIGHT_CONTROL), None);
            assert_eq!(down(&mut controller), None);
            assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, modifier), None);
        }
    }

    #[test]
    fn raw_right_release_is_not_confused_by_held_left_control() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(
            controller.event(12, RIGHT_CONTROL_KEY, 0x1 | (1 << 18)),
            Some(Action::Stop)
        );
        assert!(!controller.right_down);
    }

    #[test]
    fn repeated_modifier_events_do_not_restart() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(down(&mut controller), None);
        assert_eq!(down(&mut controller), None);
        assert_eq!(
            controller.event(12, RIGHT_CONTROL_KEY, 0),
            Some(Action::Stop)
        );
    }

    #[test]
    fn preexisting_keys_and_mid_hold_listener_start_do_not_start() {
        let mut controller = Controller::default();
        assert_eq!(controller.event(10, 8, 0), None);
        assert_eq!(down(&mut controller), None);
        assert_eq!(controller.event(11, 8, RIGHT_CONTROL), None);
        assert_eq!(down(&mut controller), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(controller.reset(), Some(Action::Cancel));
        assert_eq!(controller.event(12, 56, RIGHT_CONTROL), None);
        assert_eq!(down(&mut controller), None);
    }

    #[test]
    fn disabling_listener_cancels_immediate_recording_and_resets_hold() {
        let mut controller = Controller::default();
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(controller.reset(), Some(Action::Cancel));
        assert_eq!(controller.reset(), None);
        assert_eq!(controller.event(12, RIGHT_CONTROL_KEY, 0), None);
        assert_eq!(down(&mut controller), Some(Action::Start));
        assert_eq!(controller.reset(), Some(Action::Cancel));
    }
}
