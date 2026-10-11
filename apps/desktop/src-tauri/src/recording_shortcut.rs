use super::modifier_hold::{Action, Monitor};
use super::{reveal_overlay_window, save_settings, AppSettings, AppState};
use std::sync::Arc;
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
}
