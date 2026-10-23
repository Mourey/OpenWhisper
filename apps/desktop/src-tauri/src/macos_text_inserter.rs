use serde::Serialize;
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const SYNTHETIC_PASTE_EVENT_TAG: i64 = 0x4f57_4850_4153_5445;
const COMMAND_FLAG: u64 = 1 << 20;
const HELD_MODIFIERS: u64 =
    (1 << 17) | (1 << 18) | (1 << 19) | COMMAND_FLAG | (1 << 22) | (1 << 23) | 0x207f;
const EVENT_USER_DATA: u32 = 42;
const V_KEY_CODE: u16 = 9;
const C_KEY_CODE: u16 = 8;
/// The AXValue type tag for a `CFRange`: a C enum value, not a linkable symbol.
const K_AX_VALUE_TYPE_CF_RANGE: u32 = 4;
/// Longest selection the quill hook accepts, in Unicode scalar values.
const MAX_SELECTION_SCALARS: usize = 8_000;
/// How long a posted Cmd+C may take to move the pasteboard change count.
const COPY_WAIT: Duration = Duration::from_millis(150);
const COPY_POLL: Duration = Duration::from_millis(5);
/// Delay before the clipboard is put back after a Cmd+V insertion.
const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(300);
/// How long the quill slot stays usable for a revert, measured from its capture.
const QUILL_SLOT_TTL: Duration = Duration::from_secs(30);

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

    // The quill hook reads and replaces a selection and restores the clipboard.
    fn is_secure_field(&mut self, target: &Self::Target) -> bool;
    /// The application's name (its `AXTitle`), sent to quill as `source_app`.
    fn source_app(&mut self, target: &Self::Target) -> Option<String>;
    /// `AXSelectedText`; `None` when the attribute cannot be read.
    fn selected_text(&mut self, target: &Self::Target) -> Option<String>;
    /// Posts a tagged Cmd+C to the target's process.
    fn copy_shortcut(&mut self, target: &Self::Target) -> bool;
    fn pasteboard_change_count(&mut self) -> i64;
    /// The plain text on the general pasteboard, if any.
    fn pasteboard_text(&mut self) -> Option<String>;
    fn set_pasteboard_text(&mut self, text: &str) -> bool;
    fn pasteboard_has_items(&mut self) -> bool;
    /// `AXSelectedTextRange`, in UTF-16 code units.
    fn selected_text_range(&mut self, target: &Self::Target) -> Option<CfRange>;
    fn set_selected_text_range(&mut self, target: &Self::Target, range: CfRange) -> bool;
}

/// A `CFRange`: UTF-16 code unit offsets, as accessibility reports them.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CfRange {
    location: isize,
    length: isize,
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

    /// The contents of a CFString; `None` for any other CF type or invalid UTF-16.
    fn text(&self) -> Option<String> {
        if unsafe { CFGetTypeID(self.0) != CFStringGetTypeID() } {
            return None;
        }
        let length = unsafe { CFStringGetLength(self.0) };
        let mut units = vec![0_u16; usize::try_from(length).ok()?];
        unsafe {
            CFStringGetCharacters(
                self.0,
                CfRange {
                    location: 0,
                    length,
                },
                units.as_mut_ptr(),
            );
        }
        String::from_utf16(&units).ok()
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

/// The target of the last quill hotkey rewrite, kept apart from the dictation `TARGET` so a
/// recording started inside the revert window cannot replace it.
struct QuillSlot<T> {
    target: T,
    /// The application's `AXTitle`, sent as `source_app`.
    source_app: Option<String>,
    anchor: Option<Anchor>,
    rewrite_id: Option<String>,
    rewritten_text: Option<String>,
    delivered: Option<Delivery>,
    captured_at: Instant,
    /// The user's clipboard was overwritten and could not be put back.
    clipboard_replaced: bool,
}

/// Where the inserted rewrite starts and how long it is, in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    start: isize,
    len: isize,
}

/// How a rewrite reached the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Inserted,
    Copied,
}

/// What `replace_selection` did. The restore of the clipboard is still pending when it
/// returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceReport {
    pub paste: PasteOutcome,
    /// An anchor was recorded, so a revert can act in place.
    pub anchored: bool,
    pub clipboard_replaced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertAtAnchor {
    /// The original was written over the rewrite.
    Inserted,
    /// Nothing was touched; the caller copies the original instead.
    Copied,
}

/// The saved clipboard text, put back once the target has consumed a Cmd+V.
#[derive(Debug)]
struct PendingRestore {
    saved: String,
    /// The change count right after the rewrite was set; any other value means someone
    /// else wrote to the clipboard since.
    count_after_set: i64,
}

impl PendingRestore {
    fn apply(&self, platform: &mut impl Platform) -> bool {
        platform.pasteboard_change_count() == self.count_after_set
            && platform.set_pasteboard_text(&self.saved)
    }
}

fn wait_for_change(platform: &mut impl Platform, before: i64) -> bool {
    let deadline = Instant::now() + COPY_WAIT;
    loop {
        if platform.pasteboard_change_count() != before {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(COPY_POLL);
    }
}

/// Posts Cmd+C, waits for the pasteboard to change, reads the text and puts the previous
/// plain text back unless somebody else wrote in the meantime.
fn copy_selection<P: Platform>(
    slot: &mut QuillSlot<P::Target>,
    platform: &mut P,
) -> Result<String, &'static str> {
    if platform.modifiers_held() {
        return Err("modifiers_held");
    }
    let previous = platform.pasteboard_text();
    let had_items = platform.pasteboard_has_items();
    let before = platform.pasteboard_change_count();
    if !platform.copy_shortcut(&slot.target) || !wait_for_change(platform, before) {
        return Err("no_selection");
    }
    let after = platform.pasteboard_change_count();
    let copied = platform.pasteboard_text();
    match previous {
        Some(previous) => {
            if platform.pasteboard_change_count() == after {
                platform.set_pasteboard_text(&previous);
            }
        }
        None => slot.clipboard_replaced = had_items,
    }
    copied.ok_or("no_selection")
}

fn read_into_slot<P: Platform>(
    slot: &mut QuillSlot<P::Target>,
    platform: &mut P,
) -> Result<String, &'static str> {
    if platform.is_secure_field(&slot.target) {
        return Err("secure_field");
    }
    let text = match platform.selected_text(&slot.target) {
        Some(text) => text,
        None => copy_selection(slot, platform)?,
    };
    if text.is_empty() {
        Err("no_selection")
    } else if text.chars().count() > MAX_SELECTION_SCALARS {
        Err("too_long")
    } else {
        Ok(text)
    }
}

/// Captures the focused element into `slot` (replacing any previous capture) and returns
/// the selected text, or the reason it cannot be read.
fn slot_read_selection<P: Platform>(
    slot: &mut Option<QuillSlot<P::Target>>,
    platform: &mut P,
    now: Instant,
) -> Result<String, &'static str> {
    *slot = None;
    if !platform.has_access() {
        return Err("accessibility_permission");
    }
    let target = platform.focused_target().ok_or("no_selection")?;
    let source_app = platform.source_app(&target);
    let mut captured = QuillSlot {
        target,
        source_app,
        anchor: None,
        rewrite_id: None,
        rewritten_text: None,
        delivered: None,
        captured_at: now,
        clipboard_replaced: false,
    };
    let result = read_into_slot(&mut captured, platform);
    *slot = Some(captured);
    result
}

fn refused(reason: &'static str, clipboard_replaced: bool) -> (ReplaceReport, Option<PendingRestore>) {
    let report = ReplaceReport {
        paste: PasteOutcome::fallback(reason),
        anchored: false,
        clipboard_replaced,
    };
    (report, None)
}

/// Writes `text` over the slot's selection without consuming the slot, with the same
/// checks as `Captured::paste`. The AX path never touches the pasteboard and records the
/// insertion anchor; the Cmd+V path sets the pasteboard, posts the shortcut and hands back
/// the clipboard restore for the caller to run later.
fn slot_replace_selection<P: Platform>(
    slot: &mut Option<QuillSlot<P::Target>>,
    platform: &mut P,
    text: &str,
) -> (ReplaceReport, Option<PendingRestore>) {
    if !platform.has_access() {
        let flag = slot.as_ref().is_some_and(|slot| slot.clipboard_replaced);
        return refused("accessibility_permission", flag);
    }
    let Some(slot) = slot.as_mut() else {
        return refused("no_target", false);
    };
    slot.anchor = None;
    let flag = slot.clipboard_replaced;
    if !platform.still_focused(&slot.target) {
        return refused("target_changed", flag);
    }
    if platform.modifiers_held() {
        return refused("modifiers_held", flag);
    }
    if text.is_empty() {
        return refused("unsupported_target", flag);
    }
    let support = platform.support(&slot.target);
    if support == InsertionSupport::Unsupported {
        return refused("unsupported_target", flag);
    }
    if !platform.still_focused(&slot.target) {
        return refused("target_changed", flag);
    }
    if platform.modifiers_held() {
        return refused("modifiers_held", flag);
    }

    let mut restore = None;
    let inserted = match support {
        InsertionSupport::SelectedText => {
            let inserted = platform.set_selected_text(&slot.target, text);
            if inserted {
                slot.anchor = anchor_after_insertion(platform, &slot.target, text);
            }
            inserted
        }
        InsertionSupport::Keyboard => {
            let saved = platform.pasteboard_text();
            if platform.set_pasteboard_text(text) {
                let count_after_set = platform.pasteboard_change_count();
                match saved {
                    Some(saved) => {
                        restore = Some(PendingRestore {
                            saved,
                            count_after_set,
                        });
                    }
                    None => slot.clipboard_replaced = true,
                }
                platform.paste_shortcut(&slot.target)
            } else {
                false
            }
        }
        InsertionSupport::Unsupported => false,
    };
    if !inserted {
        // Nothing was pasted, so leave the clipboard as the user had it.
        if let Some(pending) = restore.take() {
            pending.apply(platform);
        }
        return refused("unsupported_target", slot.clipboard_replaced);
    }
    let report = ReplaceReport {
        paste: PasteOutcome::inserted(),
        anchored: slot.anchor.is_some(),
        clipboard_replaced: slot.clipboard_replaced,
    };
    (report, restore)
}

/// The anchor start is the range end after the insertion minus the inserted length, which
/// is right whether the caret collapsed after the text or the application kept it selected.
fn anchor_after_insertion<P: Platform>(
    platform: &mut P,
    target: &P::Target,
    text: &str,
) -> Option<Anchor> {
    let range = platform.selected_text_range(target)?;
    let len = isize::try_from(text.encode_utf16().count()).ok()?;
    let start = range.location.checked_add(range.length)?.checked_sub(len)?;
    (start >= 0).then_some(Anchor { start, len })
}

/// Clears a slot older than 30 s, measured from its capture.
fn expire_aged_slot<T>(slot: &mut Option<QuillSlot<T>>, now: Instant) {
    if slot
        .as_ref()
        .is_some_and(|slot| now.saturating_duration_since(slot.captured_at) >= QUILL_SLOT_TTL)
    {
        *slot = None;
    }
}

/// How the slot's own rewrite reached the user; `None` for any other rewrite id or once the
/// slot has aged out (which clears it).
fn slot_delivery<T>(slot: &mut Option<QuillSlot<T>>, rewrite_id: &str, now: Instant) -> Option<Delivery> {
    expire_aged_slot(slot, now);
    slot.as_ref()
        .filter(|slot| slot.rewrite_id.as_deref() == Some(rewrite_id))
        .and_then(|slot| slot.delivered)
}

fn slot_record_rewrite<T>(
    slot: &mut Option<QuillSlot<T>>,
    rewrite_id: &str,
    rewritten_text: &str,
    delivered: Delivery,
) {
    if let Some(slot) = slot.as_mut() {
        slot.rewrite_id = Some(rewrite_id.to_owned());
        slot.rewritten_text = Some(rewritten_text.to_owned());
        slot.delivered = Some(delivered);
    }
}

/// Selects the recorded anchor and writes `original` over it, but only for the slot's own
/// rewrite, within 30 s of its capture, with focus unchanged and the anchored text still
/// equal to the rewrite. Anything else leaves the document alone.
fn slot_revert<P: Platform>(
    slot: &mut Option<QuillSlot<P::Target>>,
    platform: &mut P,
    rewrite_id: &str,
    original: &str,
    now: Instant,
) -> RevertAtAnchor {
    expire_aged_slot(slot, now);
    let Some(slot) = slot.as_mut() else {
        return RevertAtAnchor::Copied;
    };
    if slot.rewrite_id.as_deref() != Some(rewrite_id) {
        return RevertAtAnchor::Copied;
    }
    let (Some(anchor), Some(rewritten)) = (slot.anchor, slot.rewritten_text.clone()) else {
        return RevertAtAnchor::Copied;
    };
    if !platform.has_access() || !platform.still_focused(&slot.target) {
        return RevertAtAnchor::Copied;
    }
    let range = CfRange {
        location: anchor.start,
        length: anchor.len,
    };
    if !platform.set_selected_text_range(&slot.target, range)
        || platform.selected_text(&slot.target).as_deref() != Some(rewritten.as_str())
        || !platform.set_selected_text(&slot.target, original)
    {
        return RevertAtAnchor::Copied;
    }
    slot.anchor = None;
    slot.rewritten_text = None;
    RevertAtAnchor::Inserted
}

static QUILL_TARGET: OnceLock<Mutex<Option<QuillSlot<NativeTarget>>>> = OnceLock::new();

fn quill_state() -> std::sync::MutexGuard<'static, Option<QuillSlot<NativeTarget>>> {
    QUILL_TARGET
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Captures the focused element into the quill slot and returns its selected text, or one
/// of `accessibility_permission`, `no_selection`, `secure_field`, `modifiers_held`,
/// `too_long`.
pub fn quill_read_selection() -> Result<String, &'static str> {
    slot_read_selection(&mut quill_state(), &mut NativePlatform, Instant::now())
}

pub fn read_selected_text() -> Result<String, &'static str> {
    quill_read_selection()
}

/// The application name captured with the quill slot.
pub fn quill_source_app() -> Option<String> {
    quill_state().as_ref().and_then(|slot| slot.source_app.clone())
}

/// Replaces the quill slot's selection with `text`. A Cmd+V insertion returns before the
/// clipboard is restored, 300 ms later on a detached thread.
pub fn replace_selection(text: &str) -> ReplaceReport {
    let (report, restore) = slot_replace_selection(&mut quill_state(), &mut NativePlatform, text);
    if let Some(restore) = restore {
        let _ = std::thread::Builder::new()
            .name("quill-clipboard-restore".into())
            .spawn(move || {
                std::thread::sleep(CLIPBOARD_RESTORE_DELAY);
                restore.apply(&mut NativePlatform);
            });
    }
    report
}

/// How the quill slot's rewrite was delivered, if `rewrite_id` is that rewrite.
pub fn quill_slot_delivery(rewrite_id: &str) -> Option<Delivery> {
    slot_delivery(&mut quill_state(), rewrite_id, Instant::now())
}

/// Records the rewrite on the quill slot so a later revert can find it.
pub fn quill_record_rewrite(rewrite_id: &str, rewritten_text: &str, delivered: Delivery) {
    slot_record_rewrite(&mut quill_state(), rewrite_id, rewritten_text, delivered);
}

pub fn revert_at_anchor(rewrite_id: &str, original: &str) -> RevertAtAnchor {
    slot_revert(
        &mut quill_state(),
        &mut NativePlatform,
        rewrite_id,
        original,
        Instant::now(),
    )
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
        self.post_command_key(target, V_KEY_CODE)
    }

    fn copy_shortcut(&mut self, target: &NativeTarget) -> bool {
        self.post_command_key(target, C_KEY_CODE)
    }

    fn is_secure_field(&mut self, target: &NativeTarget) -> bool {
        Self::attribute(target.field.0, "AXSubrole")
            .is_some_and(|value| value.equals_string("AXSecureTextField"))
    }

    fn source_app(&mut self, target: &NativeTarget) -> Option<String> {
        Self::attribute(target.app.0, "AXTitle")?.text()
    }

    fn selected_text(&mut self, target: &NativeTarget) -> Option<String> {
        Self::attribute(target.field.0, "AXSelectedText")?.text()
    }

    fn pasteboard_change_count(&mut self) -> i64 {
        pasteboard::change_count()
    }

    fn pasteboard_text(&mut self) -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }

    fn set_pasteboard_text(&mut self, text: &str) -> bool {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text.to_owned()))
            .is_ok()
    }

    fn pasteboard_has_items(&mut self) -> bool {
        pasteboard::has_items()
    }

    fn selected_text_range(&mut self, target: &NativeTarget) -> Option<CfRange> {
        let value = Self::attribute(target.field.0, "AXSelectedTextRange")?;
        if unsafe { CFGetTypeID(value.0) != AXValueGetTypeID() } {
            return None;
        }
        let mut range = CfRange {
            location: 0,
            length: 0,
        };
        let read = unsafe {
            AXValueGetValue(
                value.0,
                K_AX_VALUE_TYPE_CF_RANGE,
                ptr::addr_of_mut!(range).cast::<c_void>(),
            )
        };
        (read != 0).then_some(range)
    }

    fn set_selected_text_range(&mut self, target: &NativeTarget, range: CfRange) -> bool {
        let Some(attribute) = OwnedCf::string("AXSelectedTextRange") else {
            return false;
        };
        let Some(value) = OwnedCf::from_created(unsafe {
            AXValueCreate(
                K_AX_VALUE_TYPE_CF_RANGE,
                ptr::addr_of!(range).cast::<c_void>(),
            )
        }) else {
            return false;
        };
        unsafe { AXUIElementSetAttributeValue(target.field.0, attribute.0, value.0) == 0 }
    }
}

impl NativePlatform {
    /// Posts a tagged Cmd+<key> to the target's process after re-checking access, focus
    /// and modifiers.
    fn post_command_key(&mut self, target: &NativeTarget, key_code: u16) -> bool {
        let Some(down) = OwnedCf::from_created(unsafe {
            CGEventCreateKeyboardEvent(ptr::null(), key_code, true)
        }) else {
            return false;
        };
        let Some(up) = OwnedCf::from_created(unsafe {
            CGEventCreateKeyboardEvent(ptr::null(), key_code, false)
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
    fn AXValueGetTypeID() -> usize;
    fn AXValueCreate(value_type: u32, value: *const c_void) -> CfRef;
    fn AXValueGetValue(value: CfRef, value_type: u32, out: *mut c_void) -> u8;
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
    fn CFStringGetLength(value: CfRef) -> isize;
    fn CFStringGetCharacters(value: CfRef, range: CfRange, buffer: *mut u16);
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

/// The general pasteboard's change and item counts, through `objc_msgSend`. Every send is
/// cast to the exact signature of its selector and runs inside an autorelease pool, since
/// these calls happen on threads that have none.
mod pasteboard {
    use super::*;

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    // NSPasteboard lives in AppKit, which must be loaded for the class lookup to succeed.
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    unsafe fn selector(name: &'static [u8]) -> Sel {
        debug_assert_eq!(name.last(), Some(&0));
        sel_registerName(name.as_ptr().cast())
    }

    /// `-[receiver selector]` returning an object.
    unsafe fn send_object(receiver: Id, selector: Sel) -> Id {
        let send: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        send(receiver, selector)
    }

    /// `-[receiver selector]` returning an `NSInteger`.
    unsafe fn send_integer(receiver: Id, selector: Sel) -> isize {
        let send: unsafe extern "C" fn(Id, Sel) -> isize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        send(receiver, selector)
    }

    /// `-[receiver selector]` returning an `NSUInteger`.
    unsafe fn send_unsigned(receiver: Id, selector: Sel) -> usize {
        let send: unsafe extern "C" fn(Id, Sel) -> usize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        send(receiver, selector)
    }

    unsafe fn general_pasteboard() -> Option<Id> {
        let class = objc_getClass(b"NSPasteboard\0".as_ptr().cast());
        if class.is_null() {
            return None;
        }
        let pasteboard = send_object(class, selector(b"generalPasteboard\0"));
        (!pasteboard.is_null()).then_some(pasteboard)
    }

    /// `-[NSPasteboard changeCount]`; 0 when the pasteboard is unavailable.
    pub(super) fn change_count() -> i64 {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let count = general_pasteboard()
                .map_or(0, |pasteboard| send_integer(pasteboard, selector(b"changeCount\0")));
            objc_autoreleasePoolPop(pool);
            i64::try_from(count).unwrap_or(0)
        }
    }

    /// Whether `-[NSPasteboard pasteboardItems]` holds anything, text or not.
    pub(super) fn has_items() -> bool {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let has_items = general_pasteboard().is_some_and(|pasteboard| {
                let items = send_object(pasteboard, selector(b"pasteboardItems\0"));
                !items.is_null() && send_unsigned(items, selector(b"count\0")) > 0
            });
            objc_autoreleasePoolPop(pool);
            has_items
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        secure: bool,
        /// What `AXSelectedText` reads; `None` is an unreadable attribute.
        selected: Option<String>,
        selected_calls: usize,
        app_title: Option<String>,
        /// The plain text a posted Cmd+C puts on the pasteboard; `None` copies nothing.
        copy_result: Option<String>,
        copy_accepted: bool,
        copies: Vec<FakeTarget>,
        pasteboard_text: Option<String>,
        pasteboard_items: bool,
        change_count: i64,
        /// Every pasteboard method called, so AX-only paths can assert zero.
        pasteboard_calls: usize,
        pasteboard_writes: Vec<String>,
        /// Another writer moves the change count while the text is read.
        other_writer_on_read: bool,
        /// What `AXSelectedTextRange` reads.
        range: Option<CfRange>,
        range_sets: Vec<CfRange>,
        range_accepted: bool,
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
                secure: false,
                selected: Some("selected text".to_owned()),
                selected_calls: 0,
                app_title: Some("Notes".to_owned()),
                copy_result: None,
                copy_accepted: true,
                copies: Vec::new(),
                pasteboard_text: Some("old clipboard".to_owned()),
                pasteboard_items: true,
                change_count: 100,
                pasteboard_calls: 0,
                pasteboard_writes: Vec::new(),
                other_writer_on_read: false,
                range: None,
                range_sets: Vec::new(),
                range_accepted: true,
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

        fn is_secure_field(&mut self, _: &FakeTarget) -> bool {
            self.secure
        }

        fn source_app(&mut self, _: &FakeTarget) -> Option<String> {
            self.app_title.clone()
        }

        fn selected_text(&mut self, _: &FakeTarget) -> Option<String> {
            self.selected_calls += 1;
            self.selected.clone()
        }

        fn copy_shortcut(&mut self, target: &FakeTarget) -> bool {
            self.copies.push(*target);
            if !self.copy_accepted {
                return false;
            }
            if let Some(text) = self.copy_result.clone() {
                self.pasteboard_text = Some(text);
                self.pasteboard_items = true;
                self.change_count += 1;
            }
            true
        }

        fn pasteboard_change_count(&mut self) -> i64 {
            self.pasteboard_calls += 1;
            self.change_count
        }

        fn pasteboard_text(&mut self) -> Option<String> {
            self.pasteboard_calls += 1;
            let text = self.pasteboard_text.clone();
            if self.other_writer_on_read {
                self.change_count += 1;
            }
            text
        }

        fn set_pasteboard_text(&mut self, text: &str) -> bool {
            self.pasteboard_calls += 1;
            self.pasteboard_writes.push(text.to_owned());
            self.pasteboard_text = Some(text.to_owned());
            self.pasteboard_items = true;
            self.change_count += 1;
            true
        }

        fn pasteboard_has_items(&mut self) -> bool {
            self.pasteboard_calls += 1;
            self.pasteboard_items
        }

        fn selected_text_range(&mut self, _: &FakeTarget) -> Option<CfRange> {
            self.range
        }

        fn set_selected_text_range(&mut self, _: &FakeTarget, range: CfRange) -> bool {
            self.range_sets.push(range);
            self.range_accepted
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

    // ---- quill selection read, replace and revert ----------------------------------

    type FakeSlot = Option<QuillSlot<FakeTarget>>;

    fn read(slot: &mut FakeSlot, platform: &mut FakePlatform) -> Result<String, &'static str> {
        slot_read_selection(slot, platform, Instant::now())
    }

    fn slot_ref(slot: &FakeSlot) -> &QuillSlot<FakeTarget> {
        slot.as_ref().expect("the quill slot is filled")
    }

    #[test]
    fn read_selected_text_ax_path_returns_the_selection_without_a_copy() {
        let mut platform = FakePlatform::new();
        platform.selected = Some("hello 👩🏽‍💻".to_owned());
        let mut slot = None;
        assert_eq!(read(&mut slot, &mut platform), Ok("hello 👩🏽‍💻".to_owned()));
        assert!(platform.copies.is_empty());
        assert_eq!(platform.pasteboard_calls, 0);
        assert_eq!(slot_ref(&slot).target, FakeTarget { app: 1, field: 2 });
        assert_eq!(slot_ref(&slot).source_app.as_deref(), Some("Notes"));
        assert!(!slot_ref(&slot).clipboard_replaced);
    }

    #[test]
    fn read_selected_text_unreadable_attribute_takes_the_fallback_and_restores() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("copied selection".to_owned());
        let mut slot = None;
        assert_eq!(read(&mut slot, &mut platform), Ok("copied selection".to_owned()));
        assert_eq!(platform.copies, [FakeTarget { app: 1, field: 2 }]);
        assert_eq!(platform.pasteboard_writes, ["old clipboard"]);
        assert_eq!(platform.pasteboard_text.as_deref(), Some("old clipboard"));
        assert!(!slot_ref(&slot).clipboard_replaced);
    }

    #[test]
    fn read_selected_text_restore_is_skipped_when_another_writer_moved_the_count() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("copied selection".to_owned());
        platform.other_writer_on_read = true;
        let mut slot = None;
        assert_eq!(read(&mut slot, &mut platform), Ok("copied selection".to_owned()));
        assert!(platform.pasteboard_writes.is_empty());
    }

    #[test]
    fn read_selected_text_secure_field_returns_before_any_read() {
        let mut platform = FakePlatform::new();
        platform.secure = true;
        let mut slot = None;
        assert_eq!(read(&mut slot, &mut platform), Err("secure_field"));
        assert_eq!(platform.selected_calls, 0);
        assert!(platform.copies.is_empty());
        assert_eq!(platform.pasteboard_calls, 0);
    }

    #[test]
    fn read_selected_text_empty_or_absent_selection_is_no_selection() {
        let mut platform = FakePlatform::new();
        platform.selected = Some(String::new());
        assert_eq!(read(&mut None, &mut platform), Err("no_selection"));
        assert!(platform.copies.is_empty(), "a readable empty selection is not retried by copying");

        // Unreadable attribute and the copy moves nothing: nothing is selected.
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = None;
        assert_eq!(read(&mut None, &mut platform), Err("no_selection"));
        assert_eq!(platform.copies.len(), 1);
        assert!(platform.pasteboard_writes.is_empty());

        // The copy moved the count but left no plain text.
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some(String::new());
        assert_eq!(read(&mut None, &mut platform), Err("no_selection"));

        let mut platform = FakePlatform::new();
        platform.focus = None;
        assert_eq!(read(&mut None, &mut platform), Err("no_selection"));
    }

    #[test]
    fn read_selected_text_exactly_8000_scalars_is_accepted_and_8001_is_too_long() {
        // 4-byte, 2-UTF-16-unit scalars: the limit counts Unicode scalar values.
        let mut platform = FakePlatform::new();
        platform.selected = Some("😀".repeat(8000));
        assert_eq!(read(&mut None, &mut platform).map(|text| text.chars().count()), Ok(8000));

        platform.selected = Some("😀".repeat(8001));
        assert_eq!(read(&mut None, &mut platform), Err("too_long"));
    }

    #[test]
    fn read_selected_text_too_long_copy_still_restores_the_clipboard() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("x".repeat(8001));
        assert_eq!(read(&mut None, &mut platform), Err("too_long"));
        assert_eq!(platform.pasteboard_writes, ["old clipboard"]);
    }

    #[test]
    fn read_selected_text_non_text_clipboard_sets_clipboard_replaced_and_writes_nothing() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("copied selection".to_owned());
        platform.pasteboard_text = None;
        platform.pasteboard_items = true;
        let mut slot = None;
        assert_eq!(read(&mut slot, &mut platform), Ok("copied selection".to_owned()));
        assert!(platform.pasteboard_writes.is_empty());
        assert!(slot_ref(&slot).clipboard_replaced);
    }

    #[test]
    fn read_selected_text_empty_clipboard_restores_nothing_and_is_not_flagged() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("copied selection".to_owned());
        platform.pasteboard_text = None;
        platform.pasteboard_items = false;
        let mut slot = None;
        assert_eq!(read(&mut slot, &mut platform), Ok("copied selection".to_owned()));
        assert!(platform.pasteboard_writes.is_empty());
        assert!(!slot_ref(&slot).clipboard_replaced);
    }

    #[test]
    fn read_selected_text_copy_shortcut_is_never_posted_with_modifiers_held() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("copied selection".to_owned());
        platform.flags = COMMAND_FLAG;
        assert_eq!(read(&mut None, &mut platform), Err("modifiers_held"));
        assert!(platform.copies.is_empty());
        assert_eq!(platform.pasteboard_calls, 0);

        // The AX path posts nothing, so held modifiers do not matter there.
        let mut platform = FakePlatform::new();
        platform.flags = COMMAND_FLAG;
        assert_eq!(read(&mut None, &mut platform), Ok("selected text".to_owned()));
    }

    #[test]
    fn read_selected_text_failed_copy_post_is_no_selection() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_accepted = false;
        assert_eq!(read(&mut None, &mut platform), Err("no_selection"));
    }

    #[test]
    fn read_selected_text_without_accessibility_access_is_refused() {
        let mut platform = FakePlatform::new();
        platform.access = false;
        assert_eq!(read(&mut None, &mut platform), Err("accessibility_permission"));
        assert_eq!(platform.selected_calls, 0);
    }

    #[test]
    fn read_selected_text_each_capture_replaces_the_previous_slot() {
        let mut platform = FakePlatform::new();
        let mut slot = None;
        read(&mut slot, &mut platform).unwrap();
        slot.as_mut().unwrap().rewrite_id = Some("old".to_owned());
        platform.focus = Some(FakeTarget { app: 7, field: 8 });
        platform.app_title = Some("Mail".to_owned());
        read(&mut slot, &mut platform).unwrap();
        assert_eq!(slot_ref(&slot).target, FakeTarget { app: 7, field: 8 });
        assert_eq!(slot_ref(&slot).source_app.as_deref(), Some("Mail"));
        assert_eq!(slot_ref(&slot).rewrite_id, None);

        // A refused capture leaves no stale slot behind either.
        platform.access = false;
        assert!(read(&mut slot, &mut platform).is_err());
        assert!(slot.is_none());
    }

    #[test]
    fn read_selected_text_dictation_capture_after_a_quill_read_leaves_quill_target_intact() {
        let mut platform = FakePlatform::new();
        let mut quill: FakeSlot = None;
        read(&mut quill, &mut platform).unwrap();

        // The dictation slot is a separate object: capturing into it, even a different
        // field, never touches the quill slot.
        platform.focus = Some(FakeTarget { app: 9, field: 9 });
        let mut dictation = Captured { target: None };
        assert!(dictation.capture(&mut platform));
        assert_eq!(dictation.target, Some(FakeTarget { app: 9, field: 9 }));
        assert_eq!(slot_ref(&quill).target, FakeTarget { app: 1, field: 2 });
        assert!(dictation.paste(&mut platform, "dictated").pasted);
        assert_eq!(slot_ref(&quill).target, FakeTarget { app: 1, field: 2 });
    }

    #[test]
    fn read_selected_text_native_change_count_is_non_negative_and_non_decreasing() {
        let first = NativePlatform.pasteboard_change_count();
        let second = NativePlatform.pasteboard_change_count();
        assert!(first >= 0, "{first}");
        assert!(second >= first, "{first} then {second}");
    }

    #[test]
    fn read_selected_text_native_item_count_read_writes_nothing() {
        let before = NativePlatform.pasteboard_change_count();
        let _ = NativePlatform.pasteboard_has_items();
        assert_eq!(NativePlatform.pasteboard_change_count(), before);
    }

    fn slot_with_selection(platform: &mut FakePlatform) -> FakeSlot {
        let mut slot = None;
        read(&mut slot, platform).expect("a selection");
        slot
    }

    #[test]
    fn replace_selection_ax_path_makes_no_pasteboard_call_and_anchors_a_collapsed_caret() {
        let mut platform = FakePlatform::new();
        let mut slot = slot_with_selection(&mut platform);
        // "héllo👩" is 7 UTF-16 code units; the caret ends up after it, at 17.
        platform.range = Some(CfRange { location: 17, length: 0 });
        let (report, restore) = slot_replace_selection(&mut slot, &mut platform, "héllo👩");
        assert_eq!(report.paste, PasteOutcome::inserted());
        assert!(report.anchored && !report.clipboard_replaced);
        assert!(restore.is_none());
        assert_eq!(platform.inserted, ["héllo👩"]);
        assert_eq!(platform.pasteboard_calls, 0);
        assert!(platform.shortcuts.is_empty());
        assert_eq!(slot_ref(&slot).anchor, Some(Anchor { start: 10, len: 7 }));
    }

    #[test]
    fn replace_selection_ax_path_anchors_a_retained_selection() {
        let mut platform = FakePlatform::new();
        let mut slot = slot_with_selection(&mut platform);
        platform.range = Some(CfRange { location: 10, length: 7 });
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "héllo👩");
        assert!(report.anchored);
        assert_eq!(slot_ref(&slot).anchor, Some(Anchor { start: 10, len: 7 }));
    }

    #[test]
    fn replace_selection_ax_path_with_an_unreadable_range_records_no_anchor() {
        let mut platform = FakePlatform::new();
        let mut slot = slot_with_selection(&mut platform);
        platform.range = None;
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "text");
        assert_eq!(report.paste, PasteOutcome::inserted());
        assert!(!report.anchored);
        assert_eq!(slot_ref(&slot).anchor, None);

        // A range that would put the start before the document is not an anchor either.
        platform.range = Some(CfRange { location: 1, length: 0 });
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "text");
        assert!(!report.anchored);
    }

    #[test]
    fn replace_selection_does_not_consume_the_slot() {
        let mut platform = FakePlatform::new();
        let mut slot = slot_with_selection(&mut platform);
        assert!(slot_replace_selection(&mut slot, &mut platform, "one").0.paste.pasted);
        assert!(slot_replace_selection(&mut slot, &mut platform, "two").0.paste.pasted);
        assert_eq!(platform.inserted, ["one", "two"]);
        assert!(slot.is_some());
    }

    #[test]
    fn replace_selection_keyboard_path_sets_posts_and_restores_without_an_anchor() {
        let mut platform = FakePlatform::new();
        platform.support = InsertionSupport::Keyboard;
        platform.range = Some(CfRange { location: 9, length: 0 });
        let mut slot = slot_with_selection(&mut platform);
        let (report, restore) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
        assert_eq!(report.paste, PasteOutcome::inserted());
        assert!(!report.anchored && !report.clipboard_replaced);
        assert_eq!(platform.pasteboard_writes, ["rewrite"]);
        assert_eq!(platform.shortcuts, [FakeTarget { app: 1, field: 2 }]);
        assert!(platform.inserted.is_empty());
        assert_eq!(slot_ref(&slot).anchor, None);

        // The inserter returned before the restore: it only hands one back.
        assert_eq!(platform.pasteboard_text.as_deref(), Some("rewrite"));
        assert!(restore.expect("a pending restore").apply(&mut platform));
        assert_eq!(platform.pasteboard_writes, ["rewrite", "old clipboard"]);
        assert_eq!(platform.pasteboard_text.as_deref(), Some("old clipboard"));
    }

    #[test]
    fn replace_selection_keyboard_path_skips_the_restore_when_the_count_moved() {
        let mut platform = FakePlatform::new();
        platform.support = InsertionSupport::Keyboard;
        let mut slot = slot_with_selection(&mut platform);
        let (_, restore) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
        platform.change_count += 1; // the user copied something in the meantime
        assert!(!restore.expect("a pending restore").apply(&mut platform));
        assert_eq!(platform.pasteboard_writes, ["rewrite"]);
    }

    #[test]
    fn replace_selection_non_text_clipboard_sets_clipboard_replaced_and_restores_nothing() {
        let mut platform = FakePlatform::new();
        platform.support = InsertionSupport::Keyboard;
        platform.pasteboard_text = None;
        platform.pasteboard_items = true;
        let mut slot = slot_with_selection(&mut platform);
        let (report, restore) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
        assert_eq!(report.paste, PasteOutcome::inserted());
        assert!(report.clipboard_replaced);
        assert!(restore.is_none());
        assert!(slot_ref(&slot).clipboard_replaced);
    }

    #[test]
    fn replace_selection_reports_a_clipboard_flag_set_by_the_read() {
        let mut platform = FakePlatform::new();
        platform.selected = None;
        platform.copy_result = Some("copied".to_owned());
        platform.pasteboard_text = None;
        let mut slot = slot_with_selection(&mut platform);
        platform.selected = Some("copied".to_owned());
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
        assert!(report.clipboard_replaced, "the flag covers the whole flow");
    }

    #[test]
    fn replace_selection_target_change_returns_a_reason_with_no_insertion() {
        for (expected, change) in [
            ("target_changed", 0),
            ("modifiers_held", 1),
            ("accessibility_permission", 2),
        ] {
            let mut platform = FakePlatform::new();
            let mut slot = slot_with_selection(&mut platform);
            match change {
                0 => platform.focus = Some(FakeTarget { app: 3, field: 2 }),
                1 => platform.flags = COMMAND_FLAG,
                _ => platform.access = false,
            }
            let calls = platform.pasteboard_calls;
            let (report, restore) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
            assert_eq!(report.paste, PasteOutcome::fallback(expected));
            assert!(!report.anchored && restore.is_none());
            assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
            assert_eq!(platform.pasteboard_calls, calls);
            assert!(slot.is_some(), "a refusal does not consume the slot");
        }
    }

    #[test]
    fn replace_selection_without_a_slot_or_with_unsupported_target_inserts_nothing() {
        let mut platform = FakePlatform::new();
        let (report, _) = slot_replace_selection(&mut None, &mut platform, "rewrite");
        assert_eq!(report.paste, PasteOutcome::fallback("no_target"));

        let mut slot = slot_with_selection(&mut platform);
        platform.support = InsertionSupport::Unsupported;
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
        assert_eq!(report.paste, PasteOutcome::fallback("unsupported_target"));
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "");
        assert_eq!(report.paste, PasteOutcome::fallback("unsupported_target"));
        assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
    }

    #[test]
    fn replace_selection_rechecks_focus_and_modifiers_after_the_support_lookup() {
        for modifiers in [false, true] {
            let mut platform = FakePlatform::new();
            let mut slot = slot_with_selection(&mut platform);
            platform.change_focus_during_support = !modifiers;
            platform.hold_modifier_during_support = modifiers;
            let reason = if modifiers { "modifiers_held" } else { "target_changed" };
            let (report, _) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
            assert_eq!(report.paste, PasteOutcome::fallback(reason));
            assert!(platform.inserted.is_empty() && platform.shortcuts.is_empty());
        }
    }

    /// A slot holding rewrite `rw-1` ("rewrite") inserted over "selected text" by the AX
    /// path, with the anchor at 10, ready to be reverted at `base`.
    fn reverted_fixture() -> (FakePlatform, FakeSlot, Instant) {
        let mut platform = FakePlatform::new();
        let base = Instant::now();
        let mut slot = None;
        slot_read_selection(&mut slot, &mut platform, base).unwrap();
        platform.range = Some(CfRange { location: 17, length: 0 });
        let (report, _) = slot_replace_selection(&mut slot, &mut platform, "rewrite");
        assert!(report.anchored);
        slot_record_rewrite(&mut slot, "rw-1", "rewrite", Delivery::Inserted);
        // Selecting the anchored range reads back the rewrite.
        platform.selected = Some("rewrite".to_owned());
        platform.inserted.clear();
        (platform, slot, base)
    }

    #[test]
    fn quill_revert_at_anchor_writes_the_original_when_the_selection_equals_the_rewrite() {
        let (mut platform, mut slot, base) = reverted_fixture();
        let outcome = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Inserted);
        assert_eq!(platform.range_sets, [CfRange { location: 10, length: 7 }]);
        assert_eq!(platform.inserted, ["the original"]);
        assert_eq!(platform.pasteboard_calls, 0);
        // The anchor is spent: reverting again copies instead of writing twice.
        let again = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(again, RevertAtAnchor::Copied);
        assert_eq!(platform.inserted.len(), 1);
    }

    #[test]
    fn quill_revert_at_anchor_mismatch_returns_copied_with_no_write() {
        let (mut platform, mut slot, base) = reverted_fixture();
        platform.selected = Some("rewrite, edited since".to_owned());
        let outcome = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty());
    }

    #[test]
    fn quill_revert_at_anchor_missing_anchor_returns_copied_with_no_write() {
        let (mut platform, mut slot, base) = reverted_fixture();
        slot.as_mut().unwrap().anchor = None;
        let outcome = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty() && platform.range_sets.is_empty());
    }

    #[test]
    fn quill_revert_at_anchor_focus_change_returns_copied_with_no_write() {
        let (mut platform, mut slot, base) = reverted_fixture();
        platform.focus = Some(FakeTarget { app: 1, field: 3 });
        let outcome = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty() && platform.range_sets.is_empty());
    }

    #[test]
    fn quill_revert_at_anchor_another_rewrite_id_returns_copied_and_keeps_the_slot() {
        let (mut platform, mut slot, base) = reverted_fixture();
        let outcome = slot_revert(&mut slot, &mut platform, "rw-2", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty() && platform.range_sets.is_empty());
        assert!(slot.is_some());
    }

    #[test]
    fn quill_revert_at_anchor_expires_a_slot_after_30_seconds() {
        let (mut platform, mut slot, base) = reverted_fixture();
        let outcome = slot_revert(
            &mut slot,
            &mut platform,
            "rw-1",
            "the original",
            base + Duration::from_secs(31),
        );
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty() && platform.range_sets.is_empty());
        assert!(slot.is_none(), "an aged slot is cleared on access");

        let (mut platform, mut slot, base) = reverted_fixture();
        let outcome = slot_revert(
            &mut slot,
            &mut platform,
            "rw-1",
            "the original",
            base + Duration::from_secs(29),
        );
        assert_eq!(outcome, RevertAtAnchor::Inserted);
    }

    #[test]
    fn quill_revert_at_anchor_refused_range_or_write_returns_copied() {
        let (mut platform, mut slot, base) = reverted_fixture();
        platform.range_accepted = false;
        let outcome = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty());

        let (mut platform, mut slot, base) = reverted_fixture();
        platform.accepted = false;
        let outcome = slot_revert(&mut slot, &mut platform, "rw-1", "the original", base);
        assert_eq!(outcome, RevertAtAnchor::Copied);
    }

    #[test]
    fn quill_revert_at_anchor_slot_delivery_is_known_only_for_the_slots_own_rewrite() {
        let (_, mut slot, base) = reverted_fixture();
        assert_eq!(slot_delivery(&mut slot, "rw-1", base), Some(Delivery::Inserted));
        assert_eq!(slot_delivery(&mut slot, "rw-2", base), None);
        slot.as_mut().unwrap().delivered = None;
        assert_eq!(slot_delivery(&mut slot, "rw-1", base), None);
        assert_eq!(slot_delivery::<FakeTarget>(&mut None, "rw-1", base), None);
    }

    #[test]
    fn quill_revert_at_anchor_slot_delivery_expires_with_the_slot_after_30_seconds() {
        let (_, mut slot, base) = reverted_fixture();
        assert_eq!(
            slot_delivery(&mut slot, "rw-1", base + Duration::from_secs(29)),
            Some(Delivery::Inserted)
        );
        assert!(slot.is_some());
        assert_eq!(slot_delivery(&mut slot, "rw-1", base + Duration::from_secs(31)), None);
        assert!(slot.is_none(), "an aged slot is cleared on access");
    }

    #[test]
    fn quill_revert_at_anchor_without_a_slot_returns_copied() {
        let mut platform = FakePlatform::new();
        let outcome = slot_revert(&mut None, &mut platform, "rw-1", "the original", Instant::now());
        assert_eq!(outcome, RevertAtAnchor::Copied);
        assert!(platform.inserted.is_empty());
    }
}
