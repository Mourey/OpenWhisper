use serde::Serialize;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Mutex, OnceLock};

pub(crate) const SYNTHETIC_PASTE_EVENT_TAG: i64 = 0x4f57_4850_4153_5445;
const COMMAND_FLAG: u64 = 1 << 20;
const HELD_MODIFIERS: u64 =
    (1 << 17) | (1 << 18) | (1 << 19) | COMMAND_FLAG | (1 << 22) | (1 << 23) | 0x207f;
const EVENT_USER_DATA: u32 = 42;
const V_KEY_CODE: u16 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PasteOutcome {
    pub pasted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl PasteOutcome {
    fn inserted() -> Self {
        Self {
            pasted: true,
            reason: None,
        }
    }

    fn fallback(reason: &'static str) -> Self {
        Self {
            pasted: false,
            reason: Some(reason),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InsertionSupport {
    SelectedText,
    Keyboard,
    Unsupported,
}

trait Platform {
    type Target;

    fn has_access(&mut self) -> bool;
    fn focused_target(&mut self) -> Option<Self::Target>;
    fn still_focused(&mut self, target: &Self::Target) -> bool;
    fn modifiers_held(&mut self) -> bool;
    fn support(&mut self, target: &Self::Target) -> InsertionSupport;
    fn set_selected_text(&mut self, target: &Self::Target, text: &str) -> bool;
    fn paste_shortcut(&mut self, target: &Self::Target) -> bool;
}

struct Captured<T> {
    target: Option<T>,
}

impl<T> Captured<T> {
    fn capture(&mut self, platform: &mut impl Platform<Target = T>) -> bool {
        self.target = None;
        if !platform.has_access() {
            return false;
        }
        self.target = platform.focused_target();
        self.target.is_some()
    }

    fn paste(&mut self, platform: &mut impl Platform<Target = T>, text: &str) -> PasteOutcome {
        let target = self.target.take();
        if !platform.has_access() {
            return PasteOutcome::fallback("accessibility_permission");
        }
        let Some(target) = target else {
            return PasteOutcome::fallback("no_target");
        };
        if !platform.still_focused(&target) {
            return PasteOutcome::fallback("target_changed");
        }
        if platform.modifiers_held() {
            return PasteOutcome::fallback("modifiers_held");
        }
        if text.is_empty() {
            return PasteOutcome::fallback("unsupported_target");
        }
        let support = platform.support(&target);
        if support == InsertionSupport::Unsupported {
            return PasteOutcome::fallback("unsupported_target");
        }
        if !platform.still_focused(&target) {
            return PasteOutcome::fallback("target_changed");
        }
        if platform.modifiers_held() {
            return PasteOutcome::fallback("modifiers_held");
        }
        let inserted = match support {
            InsertionSupport::SelectedText => platform.set_selected_text(&target, text),
            InsertionSupport::Keyboard => platform.paste_shortcut(&target),
            InsertionSupport::Unsupported => false,
        };
        if inserted {
            PasteOutcome::inserted()
        } else {
            PasteOutcome::fallback("unsupported_target")
        }
    }
}

type CfRef = *const c_void;

struct OwnedCf(CfRef);

impl OwnedCf {
    fn from_created(value: CfRef) -> Option<Self> {
        (!value.is_null()).then(|| Self(value))
    }

    fn string(text: &str) -> Option<Self> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let length = isize::try_from(utf16.len()).ok()?;
        Self::from_created(unsafe {
            CFStringCreateWithCharacters(ptr::null(), utf16.as_ptr(), length)
        })
    }

    fn equals_string(&self, text: &str) -> bool {
        (unsafe { CFGetTypeID(self.0) == CFStringGetTypeID() })
            && Self::string(text).is_some_and(|other| unsafe { CFEqual(self.0, other.0) != 0 })
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

struct NativeTarget {
    app: OwnedCf,
    field: OwnedCf,
    pid: i32,
}

// These owned AX proxies are immutable remote identities. A single mutex serializes
// their use and release; no CF pointer or mutable collection is shared with callers.
unsafe impl Send for NativeTarget {}

static TARGET: OnceLock<Mutex<Captured<NativeTarget>>> = OnceLock::new();

fn target_state() -> &'static Mutex<Captured<NativeTarget>> {
    TARGET.get_or_init(|| Mutex::new(Captured { target: None }))
}

pub fn capture_target() -> bool {
    target_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .capture(&mut NativePlatform)
}

pub fn paste_to_captured_target(text: &str) -> PasteOutcome {
    target_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .paste(&mut NativePlatform, text)
}

pub fn request_access() -> bool {
    unsafe {
        let key = kAXTrustedCheckOptionPrompt;
        let value = kCFBooleanTrue;
        let Some(options) = OwnedCf::from_created(CFDictionaryCreate(
            ptr::null(),
            &key,
            &value,
            1,
            ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
            ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
        )) else {
            return AXIsProcessTrusted() != 0;
        };
        AXIsProcessTrustedWithOptions(options.0) != 0
    }
}

struct NativePlatform;

impl NativePlatform {
    fn attribute(element: CfRef, name: &str) -> Option<OwnedCf> {
        let name = OwnedCf::string(name)?;
        let mut value = ptr::null();
        let result = unsafe { AXUIElementCopyAttributeValue(element, name.0, &mut value) };
        let value = OwnedCf::from_created(value);
        if result == 0 {
            value
        } else {
            None
        }
    }

    fn focused() -> Option<NativeTarget> {
        let system = OwnedCf::from_created(unsafe { AXUIElementCreateSystemWide() })?;
        let app = Self::attribute(system.0, "AXFocusedApplication")?;
        if unsafe { CFGetTypeID(app.0) != AXUIElementGetTypeID() } {
            return None;
        }
        let field = Self::attribute(app.0, "AXFocusedUIElement")?;
        if unsafe { CFGetTypeID(field.0) != AXUIElementGetTypeID() } {
            return None;
        }
        let mut pid = 0;
        let mut field_pid = 0;
        if unsafe {
            AXUIElementGetPid(app.0, &mut pid) != 0
                || AXUIElementGetPid(field.0, &mut field_pid) != 0
        } || pid <= 0
            || pid != field_pid
            || pid == std::process::id() as i32
        {
            return None;
        }
        Some(NativeTarget { app, field, pid })
    }
}

impl Platform for NativePlatform {
    type Target = NativeTarget;

    fn has_access(&mut self) -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    fn focused_target(&mut self) -> Option<NativeTarget> {
        Self::focused()
    }

    fn still_focused(&mut self, target: &NativeTarget) -> bool {
        Self::focused().is_some_and(|current| {
            current.pid == target.pid
                && unsafe {
                    CFEqual(current.app.0, target.app.0) != 0
                        && CFEqual(current.field.0, target.field.0) != 0
                }
        })
    }

    fn modifiers_held(&mut self) -> bool {
        modifiers_held(unsafe { CGEventSourceFlagsState(0) })
    }

    fn support(&mut self, target: &NativeTarget) -> InsertionSupport {
        if Self::attribute(target.field.0, "AXSubrole")
            .is_some_and(|value| value.equals_string("AXSecureTextField"))
        {
            return InsertionSupport::Unsupported;
        }
        if Self::attribute(target.field.0, "AXEnabled").is_some_and(|value| unsafe {
            CFGetTypeID(value.0) == CFBooleanGetTypeID() && CFBooleanGetValue(value.0) == 0
        }) {
            return InsertionSupport::Unsupported;
        }
        let Some(attribute) = OwnedCf::string("AXSelectedText") else {
            return InsertionSupport::Unsupported;
        };
        let mut settable = 0;
        let result =
            unsafe { AXUIElementIsAttributeSettable(target.field.0, attribute.0, &mut settable) };
        if result == 0 && settable != 0 {
            return InsertionSupport::SelectedText;
        }
        if result != 0 && result != -25205 && result != -25208 {
            return InsertionSupport::Unsupported;
        }
        if Self::attribute(target.field.0, "AXRole").is_some_and(|role| {
            ["AXTextField", "AXTextArea", "AXComboBox"]
                .into_iter()
                .any(|name| role.equals_string(name))
        }) {
            InsertionSupport::Keyboard
        } else {
            InsertionSupport::Unsupported
        }
    }

    fn set_selected_text(&mut self, target: &NativeTarget, text: &str) -> bool {
        let Some(attribute) = OwnedCf::string("AXSelectedText") else {
            return false;
        };
        let Some(text) = OwnedCf::string(text) else {
            return false;
        };
        unsafe { AXUIElementSetAttributeValue(target.field.0, attribute.0, text.0) == 0 }
    }

    fn paste_shortcut(&mut self, target: &NativeTarget) -> bool {
        let Some(down) = OwnedCf::from_created(unsafe {
            CGEventCreateKeyboardEvent(ptr::null(), V_KEY_CODE, true)
        }) else {
            return false;
        };
        let Some(up) = OwnedCf::from_created(unsafe {
            CGEventCreateKeyboardEvent(ptr::null(), V_KEY_CODE, false)
        }) else {
            return false;
        };
        for event in [&down, &up] {
            unsafe {
                CGEventSetFlags(event.0, COMMAND_FLAG);
                CGEventSetIntegerValueField(event.0, EVENT_USER_DATA, SYNTHETIC_PASTE_EVENT_TAG);
            }
        }
        if !self.has_access() || !self.still_focused(target) || self.modifiers_held() {
            return false;
        }
        unsafe {
            CGEventPostToPid(target.pid, down.0);
            CGEventPostToPid(target.pid, up.0);
        }
        true
    }
}

fn modifiers_held(flags: u64) -> bool {
    flags & HELD_MODIFIERS != 0
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CfRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CfRef;
    fn AXUIElementCreateSystemWide() -> CfRef;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementCopyAttributeValue(element: CfRef, attribute: CfRef, value: *mut CfRef) -> i32;
    fn AXUIElementIsAttributeSettable(element: CfRef, attribute: CfRef, settable: *mut u8) -> i32;
    fn AXUIElementSetAttributeValue(element: CfRef, attribute: CfRef, value: CfRef) -> i32;
    fn AXUIElementGetPid(element: CfRef, pid: *mut i32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFRelease(value: CfRef);
    fn CFEqual(left: CfRef, right: CfRef) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithCharacters(
        allocator: CfRef,
        characters: *const u16,
        length: isize,
    ) -> CfRef;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: CfRef) -> u8;
    static kCFBooleanTrue: CfRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(
        allocator: CfRef,
        keys: *const CfRef,
        values: *const CfRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventCreateKeyboardEvent(source: CfRef, key: u16, down: bool) -> CfRef;
    fn CGEventSetFlags(event: CfRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CfRef, field: u32, value: i64);
    fn CGEventPostToPid(pid: i32, event: CfRef);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct CfRange {
        location: isize,
        length: isize,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetLength(value: CfRef) -> isize;
        fn CFStringGetCharacters(value: CfRef, range: CfRange, buffer: *mut u16);
    }

    #[test]
    fn null_create_result_is_not_owned_or_released() {
        assert!(OwnedCf::from_created(ptr::null()).is_none());
    }

    #[test]
    fn native_string_preserves_unicode_and_embedded_null() {
        let text = "acción 👩🏽‍💻\n中文\0é";
        let value = OwnedCf::string(text).expect("CF string allocation");
        let length = unsafe { CFStringGetLength(value.0) };
        let mut units = vec![0_u16; usize::try_from(length).expect("CF string length")];
        unsafe {
            CFStringGetCharacters(
                value.0,
                CfRange {
                    location: 0,
                    length,
                },
                units.as_mut_ptr(),
            );
        }
        assert_eq!(String::from_utf16(&units).expect("valid UTF-16"), text);
    }

    #[test]
    fn outcome_serialization_matches_frontend_contract() {
        assert_eq!(
            serde_json::to_value(PasteOutcome::inserted()).unwrap(),
            serde_json::json!({ "pasted": true })
        );
        assert_eq!(
            serde_json::to_value(PasteOutcome::fallback("target_changed")).unwrap(),
            serde_json::json!({ "pasted": false, "reason": "target_changed" })
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeTarget {
        app: u32,
        field: u32,
    }

    struct FakePlatform {
        access: bool,
        focus: Option<FakeTarget>,
        flags: u64,
        support: InsertionSupport,
        accepted: bool,
        inserted: Vec<String>,
        shortcuts: Vec<FakeTarget>,
        change_focus_during_support: bool,
        hold_modifier_during_support: bool,
    }

    impl FakePlatform {
        fn new() -> Self {
            Self {
                access: true,
                focus: Some(FakeTarget { app: 1, field: 2 }),
                flags: 0,
                support: InsertionSupport::SelectedText,
                accepted: true,
                inserted: Vec::new(),
                shortcuts: Vec::new(),
                change_focus_during_support: false,
                hold_modifier_during_support: false,
            }
        }
    }

    impl Platform for FakePlatform {
        type Target = FakeTarget;

        fn has_access(&mut self) -> bool {
            self.access
        }

        fn focused_target(&mut self) -> Option<FakeTarget> {
            self.focus
        }

        fn still_focused(&mut self, target: &FakeTarget) -> bool {
            self.focus.as_ref() == Some(target)
        }

        fn modifiers_held(&mut self) -> bool {
            modifiers_held(self.flags)
        }

        fn support(&mut self, _: &FakeTarget) -> InsertionSupport {
            if self.change_focus_during_support {
                self.focus = None;
            }
            if self.hold_modifier_during_support {
                self.flags = COMMAND_FLAG;
            }
            self.support
        }

        fn set_selected_text(&mut self, _: &FakeTarget, text: &str) -> bool {
            self.inserted.push(text.to_owned());
            self.accepted
        }

        fn paste_shortcut(&mut self, target: &FakeTarget) -> bool {
            self.shortcuts.push(*target);
            self.accepted
        }
    }

    fn captured(platform: &mut FakePlatform) -> Captured<FakeTarget> {
        let mut state = Captured { target: None };
        assert!(state.capture(platform));
        state
    }

    #[test]
    fn permission_denial_never_captures_or_inserts() {
        let mut platform = FakePlatform::new();
        platform.access = false;
        let mut state = Captured { target: None };
        assert!(!state.capture(&mut platform));
        assert_eq!(
            state.paste(&mut platform, "hello"),
            PasteOutcome::fallback("accessibility_permission")
        );
        assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
    }

    #[test]
    fn missing_target_does_not_insert() {
        let mut platform = FakePlatform::new();
        platform.focus = None;
        let mut state = Captured { target: None };
        assert!(!state.capture(&mut platform));
        assert_eq!(
            state.paste(&mut platform, "hello"),
            PasteOutcome::fallback("no_target")
        );
        assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
    }

    #[test]
    fn changing_app_or_field_rejects_delivery() {
        for focus in [
            Some(FakeTarget { app: 3, field: 2 }),
            Some(FakeTarget { app: 1, field: 3 }),
            None,
        ] {
            let mut platform = FakePlatform::new();
            let mut state = captured(&mut platform);
            platform.focus = focus;
            assert_eq!(
                state.paste(&mut platform, "hello"),
                PasteOutcome::fallback("target_changed")
            );
            assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
        }
    }

    #[test]
    fn physical_modifiers_reject_delivery_including_right_control() {
        for flags in [
            1 << 17,
            1 << 18,
            1 << 19,
            COMMAND_FLAG,
            1 << 23,
            0x2000,
            0x1,
            0x8,
        ] {
            let mut platform = FakePlatform::new();
            let mut state = captured(&mut platform);
            platform.flags = flags;
            assert_eq!(
                state.paste(&mut platform, "hello"),
                PasteOutcome::fallback("modifiers_held")
            );
            assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
        }
        assert!(!modifiers_held(1 << 16));
    }

    #[test]
    fn preserves_unicode_and_replaces_only_once() {
        let mut platform = FakePlatform::new();
        let mut state = captured(&mut platform);
        let text = "Español: acción 👩🏽‍💻\n中文\0é";
        assert_eq!(state.paste(&mut platform, text), PasteOutcome::inserted());
        assert_eq!(platform.inserted, [text]);
        assert!(platform.shortcuts.is_empty());
        assert_eq!(
            state.paste(&mut platform, text),
            PasteOutcome::fallback("no_target")
        );
        assert_eq!(platform.inserted.len(), 1);
    }

    #[test]
    fn keyboard_fallback_delivers_only_to_the_captured_target() {
        let mut platform = FakePlatform::new();
        platform.support = InsertionSupport::Keyboard;
        let mut state = captured(&mut platform);
        assert_eq!(
            state.paste(&mut platform, "hello"),
            PasteOutcome::inserted()
        );
        assert_eq!(platform.shortcuts, [FakeTarget { app: 1, field: 2 }]);
        assert!(platform.inserted.is_empty());
    }

    #[test]
    fn rechecks_focus_and_modifiers_after_support_lookup() {
        for modifiers in [false, true] {
            let mut platform = FakePlatform::new();
            let mut state = captured(&mut platform);
            platform.change_focus_during_support = !modifiers;
            platform.hold_modifier_during_support = modifiers;
            let reason = if modifiers {
                "modifiers_held"
            } else {
                "target_changed"
            };
            assert_eq!(
                state.paste(&mut platform, "hello"),
                PasteOutcome::fallback(reason)
            );
            assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
        }
    }

    #[test]
    fn denied_recapture_clears_previous_target() {
        let mut platform = FakePlatform::new();
        let mut state = captured(&mut platform);
        platform.access = false;
        assert!(!state.capture(&mut platform));
        platform.access = true;
        assert_eq!(
            state.paste(&mut platform, "hello"),
            PasteOutcome::fallback("no_target")
        );
    }

    #[test]
    fn failed_selected_text_does_not_retry_with_a_second_insertion_method() {
        let mut platform = FakePlatform::new();
        let mut state = captured(&mut platform);
        platform.accepted = false;
        assert_eq!(
            state.paste(&mut platform, "hello"),
            PasteOutcome::fallback("unsupported_target")
        );
        assert_eq!(platform.inserted, ["hello"]);
        assert!(platform.shortcuts.is_empty());
        assert_eq!(
            state.paste(&mut platform, "hello"),
            PasteOutcome::fallback("no_target")
        );
    }

    #[test]
    fn unsupported_target_and_empty_text_have_no_insertion_side_effect() {
        for text in ["", "hello"] {
            let mut platform = FakePlatform::new();
            let mut state = captured(&mut platform);
            if !text.is_empty() {
                platform.support = InsertionSupport::Unsupported;
            }
            assert_eq!(
                state.paste(&mut platform, text),
                PasteOutcome::fallback("unsupported_target")
            );
            assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
        }
    }

    #[test]
    fn fresh_capture_supports_successive_recordings() {
        let mut platform = FakePlatform::new();
        let mut state = captured(&mut platform);
        assert!(state.paste(&mut platform, "first").pasted);
        platform.focus = Some(FakeTarget { app: 4, field: 5 });
        assert!(state.capture(&mut platform));
        assert!(state.paste(&mut platform, "second").pasted);
        assert_eq!(platform.inserted, ["first", "second"]);
    }
}
