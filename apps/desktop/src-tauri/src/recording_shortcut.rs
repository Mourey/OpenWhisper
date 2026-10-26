use super::modifier_hold::{Action, Monitor};
use super::{reveal_overlay_window, save_settings, AppSettings, AppState};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordingShortcut {
    Space,
    W,
    RightControl,
}

impl RecordingShortcut {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "Control+Alt+Space" => Ok(Self::Space),
            "Control+Alt+KeyW" => Ok(Self::W),
            "RightControl" => Ok(Self::RightControl),
            _ => Err("Choose Right Control, Control+Option+Space or Control+Option+W.".to_string()),
        }
    }

    pub(crate) fn setting(self) -> &'static str {
        match self {
            Self::Space => "Control+Alt+Space",
            Self::W => "Control+Alt+KeyW",
            Self::RightControl => "RightControl",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Space => "Control+Option+Space",
            Self::W => "Control+Option+W",
            Self::RightControl => "Right Control",
        }
    }

    fn native(self) -> Option<Shortcut> {
        Some(Shortcut::new(
            Some(Modifiers::CONTROL | Modifiers::ALT),
            match self {
                Self::Space => Code::Space,
                Self::W => Code::KeyW,
                Self::RightControl => return None,
            },
        ))
    }

    fn alternate(self) -> Option<Self> {
        match self {
            Self::Space => Some(Self::W),
            Self::W => Some(Self::Space),
            Self::RightControl => None,
        }
    }
}

fn register(app: &AppHandle, shortcut: RecordingShortcut) -> Result<(), String> {
    if let Some(native) = shortcut.native() {
        return app
            .global_shortcut()
            .register(native)
            .map_err(|error| error.to_string());
    }
    let monitor = Monitor::start(app)?;
    let state = app.state::<Arc<AppState>>();
    let mut runtime = state
        .recording_shortcut
        .lock()
        .map_err(|_| "Failed to lock recording shortcut".to_string())?;
    let previous = runtime.monitor.replace(monitor);
    drop(runtime);
    drop(previous);
    Ok(())
}

fn unregister(app: &AppHandle, shortcut: RecordingShortcut) -> Result<(), String> {
    if let Some(native) = shortcut.native() {
        return app
            .global_shortcut()
            .unregister(native)
            .map_err(|error| error.to_string());
    }
    let state = app.state::<Arc<AppState>>();
    let monitor = state
        .recording_shortcut
        .lock()
        .map_err(|_| "Failed to lock recording shortcut".to_string())?
        .monitor
        .take();
    drop(monitor);
    Ok(())
}

#[derive(Default)]
struct Gesture {
    held_mode: Option<bool>,
}

impl Gesture {
    fn event(&mut self, pressed: bool, hold_to_talk: bool) -> Option<&'static str> {
        if pressed {
            if self.held_mode.is_some() {
                return None;
            }
            self.held_mode = Some(hold_to_talk);
            Some(if hold_to_talk {
                "press-to-talk-start"
            } else {
                "toggle-recording"
            })
        } else if self.held_mode.take() == Some(true) {
            Some("press-to-talk-stop")
        } else {
            None
        }
    }
}

#[derive(Default)]
pub(crate) struct Runtime {
    active: Option<RecordingShortcut>,
    gesture: Gesture,
    changing: bool,
    monitor: Option<Monitor>,
    pub(crate) error: Option<String>,
}

pub(crate) fn install(app: &AppHandle, settings: &mut AppSettings) -> Result<(), String> {
    app.plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_handler(|app, shortcut, event| {
                if quill_handle(app, shortcut.id(), event.state == ShortcutState::Pressed) {
                    return;
                }
                let state = app.state::<Arc<AppState>>();
                let hold_to_talk = match state.settings.lock() {
                    Ok(settings) => settings.hold_to_talk,
                    Err(_) => return,
                };
                let action = {
                    let Ok(mut runtime) = state.recording_shortcut.lock() else {
                        return;
                    };
                    if runtime.changing
                        || runtime
                            .active
                            .and_then(RecordingShortcut::native)
                            .map(|active| active.id())
                            != Some(shortcut.id())
                    {
                        return;
                    }
                    runtime
                        .gesture
                        .event(event.state == ShortcutState::Pressed, hold_to_talk)
                };
                if let Some(action) = action {
                    if action != "press-to-talk-stop" {
                        reveal_overlay_window(app);
                    }
                    let _ = app.emit(action, ());
                }
            })
            .build(),
    )
    .map_err(|error| error.to_string())?;
    quill_install(app);
    quill_startup(
        app,
        quill_binding(
            settings.quill_enabled,
            super::quill_command_error(settings).as_deref(),
        ),
    );
    let preferred =
        RecordingShortcut::parse(&settings.recording_shortcut).unwrap_or(RecordingShortcut::Space);
    settings.recording_shortcut = preferred.setting().to_string();
    if preferred == RecordingShortcut::RightControl {
        settings.hold_to_talk = true;
    }
    let result = register_initial(preferred, |shortcut| register(app, shortcut));
    let state = app.state::<Arc<AppState>>();
    let mut runtime = state
        .recording_shortcut
        .lock()
        .map_err(|_| "Failed to lock recording shortcut".to_string())?;
    match result {
        Ok(active) => {
            runtime.active = Some(active);
            runtime.error = runtime.monitor.as_ref().and_then(Monitor::error);
            settings.recording_shortcut = active.setting().to_string();
            if let Some(error) = &runtime.error {
                super::debug_log_line("shortcut", &error.to_string());
            } else {
                super::debug_log_line("shortcut", &format!("registered {}", active.label()));
            }
        }
        Err(error) => runtime.error = Some(error),
    }
    Ok(())
}

fn register_initial(
    preferred: RecordingShortcut,
    mut register: impl FnMut(RecordingShortcut) -> Result<(), String>,
) -> Result<RecordingShortcut, String> {
    let error = match register(preferred) {
        Ok(()) => return Ok(preferred),
        Err(error) => error,
    };
    let Some(alternate) = preferred.alternate() else {
        return Err(error);
    };
    register(alternate).map_err(|error| {
        format!(
            "Neither {} nor {} could be registered. Free one in the other app, then select it in Settings. {error}",
            preferred.label(), alternate.label()
        )
    })?;
    Ok(alternate)
}

fn change_binding(
    previous: Option<RecordingShortcut>,
    next: RecordingShortcut,
    mut register: impl FnMut(RecordingShortcut) -> Result<(), String>,
    mut unregister: impl FnMut(RecordingShortcut) -> Result<(), String>,
    mut persist: impl FnMut(bool) -> Result<(), String>,
) -> Result<(), String> {
    if previous == Some(next) {
        return persist(true);
    }
    register(next).map_err(|error| {
        if next == RecordingShortcut::RightControl {
            return format!("Right Control could not be enabled. {error}");
        }
        format!(
            "{} could not be registered; it may be in use. Choose the other shortcut. {error}",
            next.label()
        )
    })?;
    if let Err(error) = persist(true) {
        let cleanup = unregister(next);
        return Err(format!(
            "Could not save shortcut: {error}. {}",
            cleanup.err().unwrap_or_default()
        ));
    }
    if let Some(previous) = previous {
        if let Err(error) = unregister(previous) {
            let rollback = persist(false);
            let cleanup = unregister(next);
            return Err(format!(
                "Could not release previous shortcut: {error}. {} {}",
                rollback.err().unwrap_or_default(),
                cleanup.err().unwrap_or_default()
            ));
        }
    }
    Ok(())
}

pub(crate) fn save_with_binding(
    app: &AppHandle,
    state: &AppState,
    previous_settings: &AppSettings,
    updated: &AppSettings,
) -> Result<(), String> {
    if app
        .try_state::<tauri_plugin_global_shortcut::GlobalShortcut<tauri::Wry>>()
        .is_none()
    {
        return Err(
            "Shortcut support did not start. Restart OpenWhisper and try again.".to_string(),
        );
    }
    let next = RecordingShortcut::parse(&updated.recording_shortcut)?;
    let previous = {
        let mut runtime = state
            .recording_shortcut
            .lock()
            .map_err(|_| "Failed to lock recording shortcut".to_string())?;
        if runtime.gesture.held_mode.is_some()
            || runtime.monitor.as_ref().is_some_and(Monitor::is_held)
        {
            return Err("Release the recording shortcut before changing Settings.".to_string());
        }
        runtime.changing = true;
        runtime.active
    };
    let result = change_binding(
        previous,
        next,
        |shortcut| register(app, shortcut),
        |shortcut| unregister(app, shortcut),
        |use_updated| {
            save_settings(
                app,
                if use_updated {
                    updated
                } else {
                    previous_settings
                },
            )
            .map_err(|e| e.to_string())
        },
    );
    let mut runtime = state
        .recording_shortcut
        .lock()
        .map_err(|_| "Failed to lock recording shortcut".to_string())?;
    runtime.changing = false;
    if result.is_ok() {
        runtime.active = Some(next);
        runtime.error = runtime.monitor.as_ref().and_then(Monitor::error);
    }
    result
}

pub(crate) fn modifier_enabled(app: &AppHandle) -> bool {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return false;
    };
    let Ok(runtime) = state.recording_shortcut.lock() else {
        return false;
    };
    runtime.active == Some(RecordingShortcut::RightControl) && !runtime.changing
}

pub(crate) fn modifier_action(app: &AppHandle, action: Action) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let event = {
        let Ok(mut runtime) = state.recording_shortcut.lock() else {
            return;
        };
        if runtime.active != Some(RecordingShortcut::RightControl) || runtime.changing {
            return;
        }
        match action {
            Action::Start => runtime.gesture.event(true, true),
            Action::Stop => runtime.gesture.event(false, true),
            Action::Cancel => {
                (runtime.gesture.held_mode.take() == Some(true)).then_some("press-to-talk-cancel")
            }
        }
    };
    if let Some(event) = event {
        if action == Action::Start {
            reveal_overlay_window(app);
        }
        let _ = app.emit(event, ());
    }
}

pub(crate) fn modifier_status(app: &AppHandle, error: Option<String>) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let changed = {
        let Ok(mut runtime) = state.recording_shortcut.lock() else {
            return;
        };
        if runtime.active != Some(RecordingShortcut::RightControl)
            || runtime.changing
            || runtime.error == error
        {
            return;
        }
        runtime.error = error;
        true
    };
    if changed {
        let _ = app.emit("recording-shortcut-status-changed", ());
    }
}

// ---- quill shortcuts --------------------------------------------------------------------
//
// The quill hotkey, the revert window and the picker keys share the plugin handler with the
// recording shortcut. The handler routes quill ids first and only emits: the plugin holds
// its shortcut mutex while calling the handler and `unregister` takes the same mutex, so
// every release runs in a command or on `tauri::async_runtime::spawn`. Quill state has its
// own mutex, never held while a shortcut is registered or released, so a quill press never
// waits on the recording runtime or on a registration in flight.

pub(crate) const QUILL_HOTKEY_ERROR: &str =
    "Ctrl+Opt+R is used by another app. Pick a different quill shortcut in Settings.";
const QUILL_REVERT_WINDOW: Duration = Duration::from_secs(30);
const QUILL_PICKER_IDLE: Duration = Duration::from_secs(10);

static QUILL: Mutex<Quill> = Mutex::new(Quill::new());
static QUILL_APP: OnceLock<AppHandle> = OnceLock::new();
/// The plain-text clipboard saved by `quill_clipboard_hold` until `quill_clipboard_release`.
pub(crate) static QUILL_CLIPBOARD: Mutex<Option<String>> = Mutex::new(None);

fn quill_rewrite_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyR)
}

fn quill_revert_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyZ)
}

fn quill_picker_keys() -> [(Shortcut, &'static str); 14] {
    [
        (Code::ArrowUp, "up"),
        (Code::ArrowDown, "down"),
        (Code::Enter, "enter"),
        (Code::Escape, "escape"),
        (Code::Digit1, "1"),
        (Code::Digit2, "2"),
        (Code::Digit3, "3"),
        (Code::Digit4, "4"),
        (Code::Digit5, "5"),
        (Code::Digit6, "6"),
        (Code::Digit7, "7"),
        (Code::Digit8, "8"),
        (Code::Digit9, "9"),
        (Code::Digit0, "0"),
    ]
    .map(|(code, payload)| (Shortcut::new(None, code), payload))
}

/// The quill hotkey is registered only while the hook is on and its command validated.
pub(crate) fn quill_binding(enabled: bool, command_error: Option<&str>) -> bool {
    enabled && command_error.is_none()
}

/// Moves the quill hotkey from `registered` to `on` around `persist`, in `change_binding`'s
/// order: register, persist, release. Returns whether the hotkey is registered afterwards.
fn change_quill_binding(
    registered: bool,
    on: bool,
    mut register: impl FnMut(Shortcut) -> Result<(), String>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), String>,
    mut persist: impl FnMut() -> Result<(), String>,
) -> Result<bool, String> {
    if registered == on {
        return persist().map(|()| on);
    }
    if on {
        if register(quill_rewrite_shortcut()).is_err() {
            return Err(QUILL_HOTKEY_ERROR.to_string());
        }
        if let Err(error) = persist() {
            let cleanup = unregister(quill_rewrite_shortcut());
            return Err(format!(
                "Could not save settings: {error}. {}",
                cleanup.err().unwrap_or_default()
            ));
        }
        return Ok(true);
    }
    persist()?;
    let _ = unregister(quill_rewrite_shortcut());
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerState {
    Closed,
    Opening,
    Open { deadline: Instant },
    /// Enter, Escape or a digit was pressed; the keys are released off the handler.
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevertState {
    Off,
    Arming,
    Armed,
}

pub(crate) struct Quill {
    rewrite: bool,
    picker: PickerState,
    picker_generation: u64,
    revert: RevertState,
    revert_generation: u64,
    revert_deadline: Option<Instant>,
    /// The rewrite whose revert the window serves, so a revert of another rewrite leaves it.
    revert_rewrite: Option<String>,
}

/// Work the handler or a command hands to a spawned task.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Deferred {
    ClosePicker { generation: u64 },
    PickerTimer { generation: u64, deadline: Instant },
    RevertTimer { generation: u64, deadline: Instant },
}

#[derive(Debug, PartialEq, Eq)]
enum Routed {
    /// Not a quill id: the recording path handles it.
    Recording,
    /// A quill id with nothing to emit.
    Ignored,
    Emit {
        event: &'static str,
        payload: Option<&'static str>,
        reveal: bool,
        deferred: Option<Deferred>,
    },
}

impl Quill {
    const fn new() -> Self {
        Self {
            rewrite: false,
            picker: PickerState::Closed,
            picker_generation: 0,
            revert: RevertState::Off,
            revert_generation: 0,
            revert_deadline: None,
            revert_rewrite: None,
        }
    }

    /// The handler's routing step. It takes no register or unregister closure, so the
    /// handler path cannot release a shortcut.
    fn route(&mut self, id: u32, pressed: bool, now: Instant) -> Routed {
        let emit = |event, payload, reveal, deferred| Routed::Emit {
            event,
            payload,
            reveal,
            deferred,
        };
        if id == quill_rewrite_shortcut().id() {
            return if pressed {
                emit("quill-rewrite", None, true, None)
            } else {
                Routed::Ignored
            };
        }
        if id == quill_revert_shortcut().id() {
            return if pressed {
                emit("quill-revert", None, false, None)
            } else {
                Routed::Ignored
            };
        }
        let Some((_, payload)) = quill_picker_keys()
            .into_iter()
            .find(|(shortcut, _)| shortcut.id() == id)
        else {
            return Routed::Recording;
        };
        if !matches!(self.picker, PickerState::Open { .. }) {
            return if self.picker == PickerState::Closed {
                Routed::Recording
            } else {
                Routed::Ignored
            };
        }
        if !pressed {
            return Routed::Ignored;
        }
        self.picker_generation += 1;
        let generation = self.picker_generation;
        let deferred = if matches!(payload, "up" | "down") {
            let deadline = now + QUILL_PICKER_IDLE;
            self.picker = PickerState::Open { deadline };
            Deferred::PickerTimer {
                generation,
                deadline,
            }
        } else {
            self.picker = PickerState::Closing;
            Deferred::ClosePicker { generation }
        };
        emit("quill-picker-key", Some(payload), false, Some(deferred))
    }
}

fn release_picker_keys(mut unregister: impl FnMut(Shortcut) -> Result<(), String>) -> usize {
    for (shortcut, _) in quill_picker_keys() {
        let _ = unregister(shortcut);
    }
    quill_picker_keys().len()
}

/// Registers the fourteen picker keys, all or none. Opening an open picker is a no-op.
fn picker_open(
    quill: &Mutex<Quill>,
    now: Instant,
    mut register: impl FnMut(Shortcut) -> Result<(), String>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), String>,
) -> Result<Option<Deferred>, String> {
    {
        let mut state = quill.lock().map_err(|_| "Failed to lock quill shortcuts".to_string())?;
        if state.picker != PickerState::Closed {
            return Ok(None);
        }
        state.picker = PickerState::Opening;
    }
    let mut registered = vec![];
    for (shortcut, _) in quill_picker_keys() {
        if let Err(error) = register(shortcut) {
            for shortcut in registered.into_iter().rev() {
                let _ = unregister(shortcut);
            }
            if let Ok(mut state) = quill.lock() {
                state.picker = PickerState::Closed;
            }
            return Err(format!("The quill picker keys could not be registered. {error}"));
        }
        registered.push(shortcut);
    }
    let mut state = quill.lock().map_err(|_| "Failed to lock quill shortcuts".to_string())?;
    if state.picker != PickerState::Opening {
        // Closed while registering.
        drop(state);
        release_picker_keys(unregister);
        return Ok(None);
    }
    state.picker_generation += 1;
    let deadline = now + QUILL_PICKER_IDLE;
    state.picker = PickerState::Open { deadline };
    Ok(Some(Deferred::PickerTimer {
        generation: state.picker_generation,
        deadline,
    }))
}

/// Releases the picker keys if the picker is open. Idempotent; returns how many it released.
fn picker_close(
    quill: &Mutex<Quill>,
    unregister: impl FnMut(Shortcut) -> Result<(), String>,
) -> usize {
    {
        let Ok(mut state) = quill.lock() else {
            return 0;
        };
        match state.picker {
            PickerState::Closed => return 0,
            PickerState::Opening => {
                // `picker_open` releases what it registered once it sees the close.
                state.picker = PickerState::Closed;
                return 0;
            }
            PickerState::Open { .. } | PickerState::Closing => {
                state.picker = PickerState::Closed;
                state.picker_generation += 1;
            }
        }
    }
    release_picker_keys(unregister)
}

/// Opens or extends the 30 s revert window, registering the revert hotkey only if absent.
fn revert_arm(
    quill: &Mutex<Quill>,
    now: Instant,
    mut register: impl FnMut(Shortcut) -> Result<(), String>,
) -> Result<Deferred, String> {
    let arm = |state: &mut Quill| {
        state.revert_generation += 1;
        let deadline = now + QUILL_REVERT_WINDOW;
        state.revert_deadline = Some(deadline);
        Deferred::RevertTimer {
            generation: state.revert_generation,
            deadline,
        }
    };
    {
        let mut state = quill.lock().map_err(|_| "Failed to lock quill shortcuts".to_string())?;
        if state.revert != RevertState::Off {
            return Ok(arm(&mut state));
        }
        // A new window: a late revert that read the generation before this must not close it.
        state.revert_generation += 1;
        state.revert = RevertState::Arming;
    }
    let registered = register(quill_revert_shortcut());
    let mut state = quill.lock().map_err(|_| "Failed to lock quill shortcuts".to_string())?;
    if let Err(error) = registered {
        state.revert = RevertState::Off;
        state.revert_deadline = None;
        // The claim stays, so the slot keeps its button with no hotkey (R17.1); the next claim
        // or a disarm replaces it.
        return Err(error);
    }
    if state.revert != RevertState::Arming {
        // Disarmed while registering: the window still opens and its timer releases it.
        state.revert = RevertState::Armed;
        return Ok(Deferred::RevertTimer {
            generation: state.revert_generation,
            deadline: now,
        });
    }
    state.revert = RevertState::Armed;
    Ok(arm(&mut state))
}

/// Closes the revert window at once. Returns whether the hotkey was released.
#[cfg(test)]
fn revert_disarm(
    quill: &Mutex<Quill>,
    unregister: impl FnMut(Shortcut) -> Result<(), String>,
) -> bool {
    revert_disarm_since(quill, None, unregister)
}

/// The revert window's generation, which every claim, arm, disarm and expiry moves.
fn revert_generation(quill: &Mutex<Quill>) -> Option<u64> {
    quill.lock().ok().map(|state| state.revert_generation)
}

/// Gives the revert window to `rewrite_id` before it is armed for that rewrite.
fn revert_claim(quill: &Mutex<Quill>, rewrite_id: &str) {
    if let Ok(mut state) = quill.lock() {
        state.revert_rewrite = Some(rewrite_id.to_string());
        state.revert_generation += 1;
    }
}

/// Whether the revert window was last claimed for `rewrite_id` and not released since. It
/// does not check the window's state: `revert_disarm_since` returns false when it is `Off`.
fn revert_owned_by(quill: &Mutex<Quill>, rewrite_id: &str) -> bool {
    quill
        .lock()
        .is_ok_and(|state| state.revert_rewrite.as_deref() == Some(rewrite_id))
}

/// Closes the revert window unless it was armed or disarmed after `generation`, so a late
/// revert of an older rewrite leaves a newer rewrite's window armed. `None` always closes.
fn revert_disarm_since(
    quill: &Mutex<Quill>,
    generation: Option<u64>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), String>,
) -> bool {
    {
        let Ok(mut state) = quill.lock() else {
            return false;
        };
        if generation.is_some_and(|generation| generation != state.revert_generation) {
            return false;
        }
        state.revert_generation += 1;
        state.revert_deadline = None;
        state.revert_rewrite = None;
        match state.revert {
            RevertState::Off => return false,
            RevertState::Arming => {
                state.revert = RevertState::Off;
                return false;
            }
            RevertState::Armed => state.revert = RevertState::Off,
        }
    }
    let _ = unregister(quill_revert_shortcut());
    true
}

/// Runs a deferred close or timer at `now`. Stale generations and early timers release
/// nothing. Returns how many shortcuts it released.
fn run_deferred(
    quill: &Mutex<Quill>,
    deferred: Deferred,
    now: Instant,
    mut unregister: impl FnMut(Shortcut) -> Result<(), String>,
) -> usize {
    match deferred {
        Deferred::ClosePicker { generation } | Deferred::PickerTimer { generation, .. } => {
            {
                let Ok(mut state) = quill.lock() else {
                    return 0;
                };
                if state.picker_generation != generation {
                    return 0;
                }
                match state.picker {
                    PickerState::Closing => {}
                    PickerState::Open { deadline } if now >= deadline => {}
                    _ => return 0,
                }
                state.picker = PickerState::Closed;
                state.picker_generation += 1;
            }
            release_picker_keys(unregister)
        }
        Deferred::RevertTimer {
            generation,
            deadline,
        } => {
            {
                let Ok(mut state) = quill.lock() else {
                    return 0;
                };
                if state.revert_generation != generation
                    || state.revert != RevertState::Armed
                    || now < deadline
                {
                    return 0;
                }
                state.revert = RevertState::Off;
                state.revert_deadline = None;
                state.revert_rewrite = None;
                state.revert_generation += 1;
            }
            let _ = unregister(quill_revert_shortcut());
            1
        }
    }
}

fn quill_register_native(app: &AppHandle, shortcut: Shortcut) -> Result<(), String> {
    app.global_shortcut()
        .register(shortcut)
        .map_err(|error| error.to_string())
}

fn quill_unregister_native(app: &AppHandle, shortcut: Shortcut) -> Result<(), String> {
    app.global_shortcut()
        .unregister(shortcut)
        .map_err(|error| {
            super::debug_log_line("quill", &format!("shortcut release failed: {error}"));
            error.to_string()
        })
}

/// Runs `deferred` on the async runtime, after its deadline for a timer.
fn spawn_deferred(app: AppHandle, deferred: Deferred) {
    tauri::async_runtime::spawn(async move {
        if let Deferred::PickerTimer { deadline, .. } | Deferred::RevertTimer { deadline, .. } =
            &deferred
        {
            tokio::time::sleep_until(tokio::time::Instant::from_std(*deadline)).await;
        }
        run_deferred(&QUILL, deferred, Instant::now(), |shortcut| {
            quill_unregister_native(&app, shortcut)
        });
    });
}

/// The handler's first step. True when `id` is a quill id, which then never reaches the
/// recording path.
fn quill_handle(app: &AppHandle, id: u32, pressed: bool) -> bool {
    let routed = match QUILL.lock() {
        Ok(mut quill) => quill.route(id, pressed, Instant::now()),
        Err(_) => return false,
    };
    let (event, payload, reveal, deferred) = match routed {
        Routed::Recording => return false,
        Routed::Ignored => return true,
        Routed::Emit {
            event,
            payload,
            reveal,
            deferred,
        } => (event, payload, reveal, deferred),
    };
    if reveal {
        reveal_overlay_window(app);
    }
    let _ = match payload {
        Some(payload) => app.emit(event, payload),
        None => app.emit(event, ()),
    };
    if let Some(deferred) = deferred {
        spawn_deferred(app.clone(), deferred);
    }
    true
}

/// Captures the app handle that commands and spawned tasks register and release with.
fn quill_install(app: &AppHandle) {
    let _ = QUILL_APP.set(app.clone());
}

/// Registers the quill hotkey at startup when `on`. A failure is logged and the hotkey
/// stays off.
fn quill_startup(app: &AppHandle, on: bool) {
    if !on {
        super::debug_log_line("quill", "hotkey off");
        return;
    }
    match quill_register_native(app, quill_rewrite_shortcut()) {
        Ok(()) => {
            if let Ok(mut quill) = QUILL.lock() {
                quill.rewrite = true;
            }
            super::debug_log_line("quill", "hotkey registered Control+Option+R");
        }
        Err(error) => {
            super::debug_log_line("quill", &format!("hotkey registration failed at startup: {error}"))
        }
    }
}

/// Saves settings through `persist` while moving the quill hotkey from `was_on` to `on`.
/// When the binding does not change, the hotkey is left as it is, so a hotkey that failed at
/// startup does not block an unrelated save.
pub(crate) fn save_with_quill_binding(
    app: &AppHandle,
    was_on: bool,
    on: bool,
    persist: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let registered = QUILL
        .lock()
        .map_err(|_| "Failed to lock quill shortcuts".to_string())?
        .rewrite;
    let target = if was_on == on { registered } else { on };
    let now_registered = change_quill_binding(
        registered,
        target,
        |shortcut| {
            quill_register_native(app, shortcut).inspect_err(|error| {
                super::debug_log_line("quill", &format!("hotkey registration failed: {error}"))
            })
        },
        |shortcut| quill_unregister_native(app, shortcut),
        persist,
    )?;
    QUILL
        .lock()
        .map_err(|_| "Failed to lock quill shortcuts".to_string())?
        .rewrite = now_registered;
    Ok(())
}

/// Registers the picker keys for `quill_picker_open`.
pub(crate) fn quill_picker_open() -> Result<(), String> {
    let app = QUILL_APP
        .get()
        .ok_or_else(|| "Shortcut support did not start. Restart OpenWhisper and try again.".to_string())?;
    let deferred = picker_open(
        &QUILL,
        Instant::now(),
        |shortcut| quill_register_native(app, shortcut),
        |shortcut| quill_unregister_native(app, shortcut),
    )
    .inspect_err(|_| super::debug_log_line("quill", "picker keys registration failed"))?;
    if let Some(deferred) = deferred {
        spawn_deferred(app.clone(), deferred);
    }
    Ok(())
}

/// Releases the picker keys: `quill_picker_close`, a recording start and app exit.
pub(crate) fn quill_picker_close() {
    if let Some(app) = QUILL_APP.get() {
        picker_close(&QUILL, |shortcut| quill_unregister_native(app, shortcut));
    }
}

/// Opens or extends the revert window after a successful rewrite. Runs in a command.
pub(crate) fn arm_revert() {
    let Some(app) = QUILL_APP.get() else {
        return;
    };
    match revert_arm(&QUILL, Instant::now(), |shortcut| {
        quill_register_native(app, shortcut)
    }) {
        Ok(deferred) => spawn_deferred(app.clone(), deferred),
        Err(error) => {
            super::debug_log_line("quill", &format!("revert hotkey registration failed: {error}"))
        }
    }
}

/// The revert window's generation as `quill_revert` starts, for `disarm_revert`.
pub(crate) fn revert_generation_now() -> Option<u64> {
    revert_generation(&QUILL)
}

/// Gives the revert window to `rewrite_id`; call it just before `arm_revert`.
pub(crate) fn claim_revert(rewrite_id: &str) {
    revert_claim(&QUILL, rewrite_id);
}

/// Whether the revert window still serves `rewrite_id`, including after a failed hotkey
/// registration. Read it after `revert_generation_now`, so a claim between the two reads
/// shows as another owner.
pub(crate) fn revert_window_is_for(rewrite_id: &str) -> bool {
    revert_owned_by(&QUILL, rewrite_id)
}

/// Closes the revert window after `quill_revert`, unless a newer rewrite armed it after
/// `since`. Runs in a command.
pub(crate) fn disarm_revert(since: Option<u64>) {
    if let Some(app) = QUILL_APP.get() {
        revert_disarm_since(&QUILL, since, |shortcut| quill_unregister_native(app, shortcut));
    }
}

/// Runs `disarm` after a revert whatever its result, so a failed revert leaves no hotkey.
pub(crate) fn after_revert<R>(result: R, disarm: impl FnOnce()) -> R {
    disarm();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn only_supported_shortcuts_are_accepted() {
        for shortcut in [RecordingShortcut::Space, RecordingShortcut::W] {
            assert_eq!(
                RecordingShortcut::parse(shortcut.setting()).unwrap(),
                shortcut
            );
            assert_eq!(
                shortcut.native().unwrap().mods,
                Modifiers::CONTROL | Modifiers::ALT
            );
        }
        assert!(RecordingShortcut::parse("Control+Space").is_err());
        assert!(RecordingShortcut::parse("Command+Q").is_err());
        assert_ne!(
            RecordingShortcut::Space.native().unwrap().id(),
            RecordingShortcut::W.native().unwrap().id()
        );
        assert_eq!(
            RecordingShortcut::parse("RightControl").unwrap(),
            RecordingShortcut::RightControl
        );
        assert!(RecordingShortcut::RightControl.native().is_none());
    }

    #[test]
    fn hold_emits_one_start_and_one_stop_even_if_mode_changes() {
        let mut gesture = Gesture::default();
        assert_eq!(gesture.event(false, true), None);
        assert_eq!(gesture.event(true, true), Some("press-to-talk-start"));
        assert_eq!(gesture.event(true, true), None);
        assert_eq!(gesture.event(false, false), Some("press-to-talk-stop"));
        assert_eq!(gesture.event(false, true), None);
    }

    #[test]
    fn toggle_ignores_repeat_and_release() {
        let mut gesture = Gesture::default();
        assert_eq!(gesture.event(true, false), Some("toggle-recording"));
        assert_eq!(gesture.event(true, false), None);
        assert_eq!(gesture.event(false, true), None);
        assert_eq!(gesture.event(true, false), Some("toggle-recording"));
    }

    #[test]
    fn startup_uses_alternate_when_preferred_is_occupied() {
        let mut attempts = vec![];
        let result = register_initial(RecordingShortcut::Space, |shortcut| {
            attempts.push(shortcut);
            if shortcut == RecordingShortcut::Space {
                Err("occupied".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap(), RecordingShortcut::W);
        assert_eq!(
            attempts,
            vec![RecordingShortcut::Space, RecordingShortcut::W]
        );
        assert!(register_initial(RecordingShortcut::W, |_| Err("occupied".into())).is_err());
    }

    #[test]
    fn modifier_shortcut_never_falls_back_to_a_combo() {
        let attempts = RefCell::new(vec![]);
        let error = register_initial(RecordingShortcut::RightControl, |shortcut| {
            attempts.borrow_mut().push(shortcut);
            Err("Input Monitoring is not enabled".to_string())
        })
        .unwrap_err();
        assert!(error.contains("Input Monitoring"));
        assert_eq!(*attempts.borrow(), vec![RecordingShortcut::RightControl]);
    }

    #[test]
    fn failed_modifier_registration_keeps_previous_combo() {
        let result = change_binding(
            Some(RecordingShortcut::W),
            RecordingShortcut::RightControl,
            |_| Err("listener unavailable".to_string()),
            |_| panic!("must retain old registration"),
            |_| panic!("must not save unavailable shortcut"),
        );
        assert!(result.unwrap_err().contains("listener unavailable"));
    }

    #[test]
    fn occupied_new_shortcut_keeps_old_registration_and_settings() {
        let result = change_binding(
            Some(RecordingShortcut::Space),
            RecordingShortcut::W,
            |_| Err("occupied".into()),
            |_| panic!("must retain old registration"),
            |_| panic!("must not save occupied shortcut"),
        );
        assert!(result.unwrap_err().contains("in use"));
    }

    #[test]
    fn saving_failure_releases_only_new_shortcut() {
        let released = RefCell::new(vec![]);
        let result = change_binding(
            Some(RecordingShortcut::Space),
            RecordingShortcut::W,
            |_| Ok(()),
            |shortcut| {
                released.borrow_mut().push(shortcut);
                Ok(())
            },
            |_| Err("disk full".into()),
        );
        assert!(result.unwrap_err().contains("disk full"));
        assert_eq!(*released.borrow(), vec![RecordingShortcut::W]);
    }

    #[test]
    fn switching_registers_and_saves_before_releasing_old_shortcut() {
        let actions = RefCell::new(vec![]);
        change_binding(
            Some(RecordingShortcut::Space),
            RecordingShortcut::W,
            |shortcut| {
                actions.borrow_mut().push(format!("register {shortcut:?}"));
                Ok(())
            },
            |shortcut| {
                actions.borrow_mut().push(format!("release {shortcut:?}"));
                Ok(())
            },
            |updated| {
                actions.borrow_mut().push(format!("save {updated}"));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            *actions.borrow(),
            vec!["register W", "save true", "release Space"]
        );
    }

    #[test]
    fn unchanged_shortcut_does_not_register_twice() {
        change_binding(
            Some(RecordingShortcut::W),
            RecordingShortcut::W,
            |_| panic!("already registered"),
            |_| panic!("keep current registration"),
            |updated| {
                assert!(updated);
                Ok(())
            },
        )
        .unwrap();
    }

    #[test]
    fn old_release_failure_rolls_back_saved_preference() {
        let actions = RefCell::new(vec![]);
        let result = change_binding(
            Some(RecordingShortcut::Space),
            RecordingShortcut::W,
            |_| Ok(()),
            |shortcut| {
                actions.borrow_mut().push(format!("release {shortcut:?}"));
                if shortcut == RecordingShortcut::Space {
                    Err("unregister failed".into())
                } else {
                    Ok(())
                }
            },
            |updated| {
                actions.borrow_mut().push(format!("save {updated}"));
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            *actions.borrow(),
            vec!["save true", "release Space", "save false", "release W"]
        );
    }

    // ---- quill shortcuts ----------------------------------------------------------------

    fn quill_expected_picker_ids() -> Vec<u32> {
        [
            Code::ArrowUp,
            Code::ArrowDown,
            Code::Enter,
            Code::Escape,
            Code::Digit1,
            Code::Digit2,
            Code::Digit3,
            Code::Digit4,
            Code::Digit5,
            Code::Digit6,
            Code::Digit7,
            Code::Digit8,
            Code::Digit9,
            Code::Digit0,
        ]
        .into_iter()
        .map(|code| Shortcut::new(None, code).id())
        .collect()
    }

    fn quill_rewrite_id() -> u32 {
        Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyR).id()
    }

    fn quill_revert_id() -> u32 {
        Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyZ).id()
    }

    fn quill_open_picker(quill: &Mutex<Quill>, now: Instant) -> Deferred {
        picker_open(quill, now, |_| Ok(()), |_| panic!("open must not release"))
            .unwrap()
            .expect("first open starts the inactivity timer")
    }

    fn quill_released(quill: &Mutex<Quill>, deferred: Deferred, now: Instant) -> Vec<u32> {
        let released = RefCell::new(vec![]);
        run_deferred(quill, deferred, now, |shortcut| {
            released.borrow_mut().push(shortcut.id());
            Ok(())
        });
        released.into_inner()
    }

    #[test]
    fn quill_shortcut_binding_is_on_only_when_enabled_and_valid() {
        assert!(quill_binding(true, None));
        assert!(!quill_binding(true, Some("not executable")));
        assert!(!quill_binding(false, None));
        assert!(!quill_binding(false, Some("not executable")));
    }

    #[test]
    fn quill_shortcut_hotkeys_are_control_option_r_and_z() {
        assert_eq!(quill_rewrite_shortcut().id(), quill_rewrite_id());
        assert_eq!(quill_revert_shortcut().id(), quill_revert_id());
        assert_eq!(QUILL_REVERT_WINDOW, Duration::from_secs(30));
        assert_eq!(QUILL_PICKER_IDLE, Duration::from_secs(10));
        for recording in [RecordingShortcut::Space, RecordingShortcut::W] {
            let id = recording.native().unwrap().id();
            assert_ne!(id, quill_rewrite_id());
            assert_ne!(id, quill_revert_id());
        }
    }

    #[test]
    fn quill_shortcut_route_emits_rewrite_on_pressed_only_without_app_locks() {
        // Routing must run while the recording runtime and settings are locked elsewhere.
        let recording = std::sync::Mutex::new(Runtime::default());
        let settings = std::sync::Mutex::new(AppSettings::default());
        let _recording = recording.lock().unwrap();
        let _settings = settings.lock().unwrap();
        let mut quill = Quill::new();
        let now = Instant::now();
        assert_eq!(
            quill.route(quill_rewrite_id(), true, now),
            Routed::Emit {
                event: "quill-rewrite",
                payload: None,
                reveal: true,
                deferred: None,
            }
        );
        assert_eq!(quill.route(quill_rewrite_id(), false, now), Routed::Ignored);
    }

    #[test]
    fn quill_shortcut_recording_ids_fall_through() {
        let mut quill = Quill::new();
        let now = Instant::now();
        for recording in [RecordingShortcut::Space, RecordingShortcut::W] {
            let id = recording.native().unwrap().id();
            assert_eq!(quill.route(id, true, now), Routed::Recording);
            assert_eq!(quill.route(id, false, now), Routed::Recording);
        }
        // Picker keys are not quill keys while the picker is closed.
        for id in quill_expected_picker_ids() {
            assert_eq!(quill.route(id, true, now), Routed::Recording);
        }
    }

    #[test]
    fn quill_shortcut_failed_register_persists_nothing() {
        let result = change_quill_binding(
            false,
            true,
            |_| Err("occupied".into()),
            |_| panic!("nothing to release"),
            || panic!("must not save quillEnabled when the hotkey is taken"),
        );
        assert_eq!(result.unwrap_err(), QUILL_HOTKEY_ERROR);
        assert_eq!(
            QUILL_HOTKEY_ERROR,
            "Ctrl+Opt+R is used by another app. Pick a different quill shortcut in Settings."
        );
    }

    #[test]
    fn quill_shortcut_disable_releases_the_id_once() {
        let released = RefCell::new(vec![]);
        let registered = change_quill_binding(
            true,
            false,
            |_| panic!("disable registers nothing"),
            |shortcut| {
                released.borrow_mut().push(shortcut.id());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert!(!registered);
        assert_eq!(*released.borrow(), vec![quill_rewrite_id()]);
    }

    #[test]
    fn quill_shortcut_registration_follows_change_binding_order() {
        let actions = RefCell::new(vec![]);
        let log = |transition: (bool, bool), persist_ok: bool| {
            actions.borrow_mut().clear();
            let result = change_quill_binding(
                transition.0,
                transition.1,
                |shortcut| {
                    assert_eq!(shortcut.id(), quill_rewrite_id());
                    actions.borrow_mut().push("register");
                    Ok(())
                },
                |shortcut| {
                    assert_eq!(shortcut.id(), quill_rewrite_id());
                    actions.borrow_mut().push("release");
                    Ok(())
                },
                || {
                    actions.borrow_mut().push("save");
                    if persist_ok {
                        Ok(())
                    } else {
                        Err("disk full".into())
                    }
                },
            );
            (result, actions.borrow().clone())
        };
        assert_eq!(log((false, true), true), (Ok(true), vec!["register", "save"]));
        assert_eq!(log((true, false), true), (Ok(false), vec!["save", "release"]));
        assert_eq!(log((true, true), true), (Ok(true), vec!["save"]));
        assert_eq!(log((false, false), true), (Ok(false), vec!["save"]));
        let (result, actions) = log((false, true), false);
        assert!(result.unwrap_err().contains("disk full"));
        assert_eq!(actions, vec!["register", "save", "release"]);
        let (result, actions) = log((true, false), false);
        assert!(result.unwrap_err().contains("disk full"));
        assert_eq!(actions, vec!["save"], "a failed save keeps the hotkey registered");
    }

    #[test]
    fn quill_picker_shortcuts_open_registers_fourteen_distinct_ids() {
        let quill = Mutex::new(Quill::new());
        let registered = RefCell::new(vec![]);
        let deferred = picker_open(
            &quill,
            Instant::now(),
            |shortcut| {
                assert!(shortcut.mods.is_empty(), "picker keys carry no modifier");
                registered.borrow_mut().push(shortcut.id());
                Ok(())
            },
            |_| panic!("a successful open releases nothing"),
        )
        .unwrap();
        assert!(deferred.is_some());
        let registered = registered.into_inner();
        assert_eq!(registered, quill_expected_picker_ids());
        let mut distinct = registered.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 14);
    }

    #[test]
    fn quill_picker_shortcuts_close_releases_exactly_those_and_is_idempotent() {
        let quill = Mutex::new(Quill::new());
        quill_open_picker(&quill, Instant::now());
        let released = RefCell::new(vec![]);
        let count = picker_close(&quill, |shortcut| {
            released.borrow_mut().push(shortcut.id());
            Ok(())
        });
        assert_eq!(count, 14);
        assert_eq!(*released.borrow(), quill_expected_picker_ids());
        assert_eq!(picker_close(&quill, |_| panic!("already closed")), 0);
        assert_eq!(picker_close(&quill, |_| panic!("already closed")), 0);
    }

    #[test]
    fn quill_picker_shortcuts_failure_on_ninth_releases_first_eight() {
        let quill = Mutex::new(Quill::new());
        let attempts = RefCell::new(0);
        let released = RefCell::new(vec![]);
        let result = picker_open(
            &quill,
            Instant::now(),
            |_| {
                *attempts.borrow_mut() += 1;
                if *attempts.borrow() == 9 {
                    Err("occupied".into())
                } else {
                    Ok(())
                }
            },
            |shortcut| {
                released.borrow_mut().push(shortcut.id());
                Ok(())
            },
        );
        assert!(result.is_err());
        let mut released = released.into_inner();
        released.sort_unstable();
        let mut first_eight = quill_expected_picker_ids()[..8].to_vec();
        first_eight.sort_unstable();
        assert_eq!(released, first_eight);
        // All or none: the picker is closed, so its keys fall through and a close is a no-op.
        assert_eq!(picker_close(&quill, |_| panic!("nothing is registered")), 0);
        assert_eq!(
            quill.lock().unwrap().route(quill_expected_picker_ids()[0], true, Instant::now()),
            Routed::Recording
        );
    }

    #[test]
    fn quill_picker_shortcuts_second_open_registers_nothing() {
        let quill = Mutex::new(Quill::new());
        quill_open_picker(&quill, Instant::now());
        let again = picker_open(
            &quill,
            Instant::now(),
            |_| panic!("already open"),
            |_| panic!("already open"),
        );
        assert_eq!(again, Ok(None));
    }

    #[test]
    fn quill_picker_shortcuts_handler_never_unregisters_and_defers_release() {
        let payloads = [
            "up", "down", "enter", "escape", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0",
        ];
        for (id, payload) in quill_expected_picker_ids().into_iter().zip(payloads) {
            let quill = Mutex::new(Quill::new());
            let start = Instant::now();
            quill_open_picker(&quill, start);
            // `route` takes no unregister closure: the handler path cannot release.
            let routed = quill.lock().unwrap().route(id, true, start);
            let Routed::Emit {
                event,
                payload: Some(emitted),
                reveal,
                deferred: Some(deferred),
            } = routed
            else {
                panic!("picker key {payload} must emit and defer, got {routed:?}");
            };
            assert_eq!(event, "quill-picker-key");
            assert_eq!(emitted, payload);
            assert!(!reveal);
            // The deferred release frees all fourteen once, whenever it runs.
            let late = start + QUILL_PICKER_IDLE + QUILL_PICKER_IDLE;
            assert_eq!(quill_released(&quill, deferred.clone(), late), quill_expected_picker_ids());
            assert!(quill_released(&quill, deferred, late).is_empty());
            assert_eq!(picker_close(&quill, |_| panic!("already released")), 0);
        }
    }

    #[test]
    fn quill_picker_shortcuts_close_after_ten_seconds_without_a_key() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let timer = quill_open_picker(&quill, start);
        assert!(quill_released(&quill, timer.clone(), start + Duration::from_secs(9)).is_empty());
        assert_eq!(
            quill_released(&quill, timer.clone(), start + QUILL_PICKER_IDLE),
            quill_expected_picker_ids()
        );
        assert!(quill_released(&quill, timer, start + QUILL_PICKER_IDLE).is_empty());
    }

    #[test]
    fn quill_picker_shortcuts_key_at_nine_seconds_defers_the_close() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let first = quill_open_picker(&quill, start);
        let at_nine = start + Duration::from_secs(9);
        let Routed::Emit {
            deferred: Some(second),
            ..
        } = quill.lock().unwrap().route(quill_expected_picker_ids()[1], true, at_nine)
        else {
            panic!("down must emit and restart the timer");
        };
        assert!(quill_released(&quill, first, start + QUILL_PICKER_IDLE).is_empty());
        assert!(quill_released(&quill, second.clone(), start + Duration::from_secs(18)).is_empty());
        assert_eq!(
            quill_released(&quill, second, at_nine + QUILL_PICKER_IDLE),
            quill_expected_picker_ids()
        );
    }

    #[test]
    fn quill_picker_shortcuts_stale_generation_timer_releases_nothing() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let stale = quill_open_picker(&quill, start);
        picker_close(&quill, |_| Ok(()));
        let fresh = quill_open_picker(&quill, start + Duration::from_secs(5));
        assert!(quill_released(&quill, stale, start + Duration::from_secs(11)).is_empty());
        assert_eq!(
            quill_released(&quill, fresh, start + Duration::from_secs(15)),
            quill_expected_picker_ids()
        );
    }

    #[test]
    fn quill_picker_shortcuts_released_emits_nothing() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let timer = quill_open_picker(&quill, start);
        for id in quill_expected_picker_ids() {
            assert_eq!(quill.lock().unwrap().route(id, false, start), Routed::Ignored);
        }
        // Released neither closed the picker nor restarted its timer.
        assert_eq!(
            quill_released(&quill, timer, start + QUILL_PICKER_IDLE),
            quill_expected_picker_ids()
        );
    }

    #[test]
    fn quill_revert_hotkey_arm_registers_once() {
        let quill = Mutex::new(Quill::new());
        let registered = RefCell::new(vec![]);
        let start = Instant::now();
        let register = |shortcut: Shortcut| {
            registered.borrow_mut().push(shortcut.id());
            Ok(())
        };
        revert_arm(&quill, start, register).unwrap();
        revert_arm(&quill, start + Duration::from_secs(1), register).unwrap();
        assert_eq!(*registered.borrow(), vec![quill_revert_id()]);
    }

    #[test]
    fn quill_revert_hotkey_rearm_inside_the_window_moves_the_deadline() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let first = revert_arm(&quill, start, |_| Ok(())).unwrap();
        let later = start + Duration::from_secs(20);
        let second = revert_arm(&quill, later, |_| panic!("already registered")).unwrap();
        assert!(quill_released(&quill, first, start + QUILL_REVERT_WINDOW).is_empty());
        assert!(quill_released(&quill, second.clone(), start + QUILL_REVERT_WINDOW).is_empty());
        assert_eq!(
            quill_released(&quill, second, later + QUILL_REVERT_WINDOW),
            vec![quill_revert_id()]
        );
    }

    #[test]
    fn quill_revert_hotkey_expiry_releases_once() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let timer = revert_arm(&quill, start, |_| Ok(())).unwrap();
        let end = start + QUILL_REVERT_WINDOW;
        assert_eq!(quill_released(&quill, timer.clone(), end), vec![quill_revert_id()]);
        assert!(quill_released(&quill, timer, end).is_empty());
        assert!(!revert_disarm(&quill, |_| panic!("already released")));
    }

    #[test]
    fn quill_revert_hotkey_stale_timer_releases_nothing() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let stale = revert_arm(&quill, start, |_| Ok(())).unwrap();
        assert!(revert_disarm(&quill, |_| Ok(())));
        let registered = RefCell::new(0);
        let fresh = revert_arm(&quill, start + Duration::from_secs(10), |_| {
            *registered.borrow_mut() += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(*registered.borrow(), 1, "a disarmed window registers again");
        assert!(quill_released(&quill, stale, start + QUILL_REVERT_WINDOW).is_empty());
        assert_eq!(
            quill_released(&quill, fresh, start + Duration::from_secs(40)),
            vec![quill_revert_id()]
        );
    }

    #[test]
    fn quill_revert_hotkey_disarm_releases() {
        let quill = Mutex::new(Quill::new());
        revert_arm(&quill, Instant::now(), |_| Ok(())).unwrap();
        let released = RefCell::new(vec![]);
        assert!(revert_disarm(&quill, |shortcut| {
            released.borrow_mut().push(shortcut.id());
            Ok(())
        }));
        assert_eq!(*released.borrow(), vec![quill_revert_id()]);
        assert!(!revert_disarm(&quill, |_| panic!("already released")));
    }

    #[test]
    fn quill_revert_hotkey_failed_registration_opens_no_window() {
        let quill = Mutex::new(Quill::new());
        assert!(revert_arm(&quill, Instant::now(), |_| Err("occupied".into())).is_err());
        assert!(!revert_disarm(&quill, |_| panic!("nothing is registered")));
    }

    #[test]
    fn quill_revert_hotkey_failed_registration_keeps_the_claim_for_the_slot() {
        let quill = Mutex::new(Quill::new());
        revert_claim(&quill, "rewrite-x");
        assert!(revert_arm(&quill, Instant::now(), |_| Err("occupied".into())).is_err());
        assert!(revert_owned_by(&quill, "rewrite-x"), "a failed registration dropped the slot's claim");
        assert!(!revert_disarm(&quill, |_| panic!("nothing is registered")));
        assert!(!revert_owned_by(&quill, "rewrite-x"), "a disarm left the claim");
    }

    #[test]
    fn quill_revert_hotkey_handler_never_unregisters_and_released_emits_nothing() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        revert_arm(&quill, start, |_| Ok(())).unwrap();
        // `route` takes no unregister closure: the handler path cannot release.
        assert_eq!(
            quill.lock().unwrap().route(quill_revert_id(), true, start),
            Routed::Emit {
                event: "quill-revert",
                payload: None,
                reveal: false,
                deferred: None,
            }
        );
        assert_eq!(quill.lock().unwrap().route(quill_revert_id(), false, start), Routed::Ignored);
        let released = RefCell::new(0);
        assert!(revert_disarm(&quill, |_| {
            *released.borrow_mut() += 1;
            Ok(())
        }));
        assert_eq!(*released.borrow(), 1, "the window was still armed after both presses");
    }

    #[test]
    fn quill_revert_hotkey_after_revert_disarms_on_success_and_on_failure() {
        let disarmed = RefCell::new(0);
        let ok: Result<&str, &str> = after_revert(Ok("inserted"), || *disarmed.borrow_mut() += 1);
        assert_eq!(ok, Ok("inserted"));
        assert_eq!(*disarmed.borrow(), 1);
        let failed: Result<&str, &str> = after_revert(Err("target_changed"), || *disarmed.borrow_mut() += 1);
        assert_eq!(failed, Err("target_changed"));
        assert_eq!(*disarmed.borrow(), 2, "a failed revert left the revert hotkey armed");
    }

    #[test]
    fn quill_revert_hotkey_late_revert_leaves_a_newer_window_armed() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        revert_arm(&quill, start, |_| Ok(())).unwrap();
        let reverting = revert_generation(&quill);
        revert_arm(&quill, start + Duration::from_secs(5), |_| Ok(())).unwrap();
        assert!(!revert_disarm_since(&quill, reverting, |_| panic!("released a newer rewrite's hotkey")));
        let released = RefCell::new(vec![]);
        assert!(revert_disarm_since(&quill, revert_generation(&quill), |shortcut| {
            released.borrow_mut().push(shortcut.id());
            Ok(())
        }));
        assert_eq!(*released.borrow(), vec![quill_revert_id()]);
    }

    #[test]
    fn quill_revert_hotkey_revert_with_no_newer_rewrite_disarms() {
        let quill = Mutex::new(Quill::new());
        revert_arm(&quill, Instant::now(), |_| Ok(())).unwrap();
        let reverting = revert_generation(&quill);
        let released = RefCell::new(0);
        assert!(revert_disarm_since(&quill, reverting, |_| {
            *released.borrow_mut() += 1;
            Ok(())
        }));
        assert_eq!(*released.borrow(), 1);
        assert!(!revert_disarm_since(&quill, None, |_| panic!("already released")));
    }

    #[test]
    fn quill_revert_hotkey_arming_and_expiry_move_the_generation() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let before = revert_generation(&quill);
        revert_arm(&quill, start, |_| {
            assert_ne!(revert_generation(&quill), before, "arming did not move the generation");
            Ok(())
        })
        .map(|timer| {
            let armed = revert_generation(&quill);
            quill_released(&quill, timer, start + QUILL_REVERT_WINDOW);
            assert_ne!(revert_generation(&quill), armed, "expiry did not move the generation");
        })
        .unwrap();
    }

    #[test]
    fn quill_revert_hotkey_late_revert_across_expiry_leaves_a_registering_window() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        let old = revert_arm(&quill, start, |_| Ok(())).unwrap();
        let reverting = revert_generation(&quill);
        let expired = start + QUILL_REVERT_WINDOW;
        assert_eq!(quill_released(&quill, old, expired), vec![quill_revert_id()]);
        let late_disarm = RefCell::new(None);
        let fresh = revert_arm(&quill, expired, |_| {
            *late_disarm.borrow_mut() =
                Some(revert_disarm_since(&quill, reverting, |_| panic!("released while registering")));
            Ok(())
        })
        .unwrap();
        assert_eq!(*late_disarm.borrow(), Some(false));
        assert!(quill_released(&quill, fresh.clone(), expired).is_empty(), "the new window closed at once");
        assert_eq!(quill_released(&quill, fresh, expired + QUILL_REVERT_WINDOW), vec![quill_revert_id()]);
    }

    #[test]
    fn quill_revert_hotkey_window_belongs_to_the_latest_claimed_rewrite() {
        let quill = Mutex::new(Quill::new());
        let start = Instant::now();
        revert_claim(&quill, "rewrite-x");
        revert_arm(&quill, start, |_| Ok(())).unwrap();
        assert!(revert_owned_by(&quill, "rewrite-x"));
        let reverting_x = revert_generation(&quill);
        revert_claim(&quill, "rewrite-y");
        assert!(!revert_owned_by(&quill, "rewrite-x"), "an older rewrite still owns the window");
        assert!(revert_owned_by(&quill, "rewrite-y"));
        assert_ne!(revert_generation(&quill), reverting_x, "claiming did not move the generation");
        revert_arm(&quill, start + Duration::from_secs(1), |_| panic!("already registered")).unwrap();
        assert!(revert_owned_by(&quill, "rewrite-y"));
        assert!(revert_disarm(&quill, |_| Ok(())));
        assert!(!revert_owned_by(&quill, "rewrite-y"), "a closed window has no owner");
    }
}
