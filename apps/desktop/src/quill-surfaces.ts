// Pure logic of the quill hook surfaces: the press window, the picker, the dictation
// branch, the revert slot and the notice table. It imports nothing from Tauri and touches
// no DOM, so vitest runs it in node; main.ts wires it to events, markup and `invoke`.

export const QUILL_PRESS_WINDOW_MS = 600;
export const QUILL_PICKER_IDLE_MS = 10_000;
export const QUILL_REVERT_WINDOW_MS = 30_000;
export const QUILL_DICTATION_GUARD_MS = 8000;

export const QUILL_STILL_REWRITING = "Still rewriting the last selection.";
export const QUILL_REVERT_COPIED = "Original copied. Press Command+V to restore.";
export const QUILL_CLIPBOARD_REPLACED = "Your clipboard held an image or file; it was replaced by the selection.";
export const QUILL_LIVE_REWRITING = "Rewriting transcript";
export const QUILL_MODE_HINT = "Turn on Quill to choose a mode";

/** The eight engine codes of the vendored protocol, then the nine client codes. */
export const QUILL_ERROR_CODES = [
  "not_found",
  "bad_request",
  "internal_error",
  "timeout",
  "upstream_error",
  "no_key",
  "unknown_mode",
  "revert_expired",
  "busy",
  "transport",
  "no_selection",
  "secure_field",
  "too_long",
  "accessibility_permission",
  "modifiers_held",
  "target_changed",
  "disabled",
] as const;

export type QuillErrorCode = (typeof QUILL_ERROR_CODES)[number];
export type QuillCommandError = { code: QuillErrorCode; message: string };

const SERVICE_UNAVAILABLE = "Rewrite service unavailable. Check the quill command in Settings.";

/** One notice per code. The code selects the text; `message` never does. */
export const QUILL_NOTICES: Record<QuillErrorCode, string> = {
  not_found: SERVICE_UNAVAILABLE,
  bad_request: SERVICE_UNAVAILABLE,
  internal_error: SERVICE_UNAVAILABLE,
  timeout: "Rewrite timed out. The text was not changed.",
  upstream_error: SERVICE_UNAVAILABLE,
  no_key: "No OpenRouter key found. Add one in Keychain as quill-openrouter.",
  unknown_mode: SERVICE_UNAVAILABLE,
  revert_expired: SERVICE_UNAVAILABLE,
  busy: "Finishing the dictation rewrite first. Try again in a moment.",
  transport: SERVICE_UNAVAILABLE,
  no_selection: "Select some text first.",
  secure_field: "Can't rewrite in a password field.",
  too_long: "Selection is over 8,000 characters. Select less.",
  accessibility_permission: "Enable Accessibility for automatic insertion.",
  modifiers_held: "A shortcut key is still held.",
  target_changed: "Copied the rewrite. Press Command+V to paste it.",
  disabled: SERVICE_UNAVAILABLE,
};

export type QuillNoticeRole = "alert" | "status";
export type QuillNotify = (text: string, role: QuillNoticeRole) => void;
export type QuillInvoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
export type QuillDelivered = "inserted" | "copied" | "failed";

/** Failures stay 6 s, successes 3 s. */
export function quillNoticeDurationMs(role: QuillNoticeRole) {
  return role === "alert" ? 6000 : 3000;
}

/** A diagnostic line that names an event and a code and nothing else. */
export function quillLogLine(event: string, code: string) {
  return `quill ${event} code=${code}`;
}

/** Reads a rejection as `{code, message}`; anything else is `transport`. */
export function toQuillCommandError(error: unknown): QuillCommandError {
  if (typeof error === "object" && error !== null) {
    const { code, message } = error as { code?: unknown; message?: unknown };
    if (
      typeof code === "string" &&
      typeof message === "string" &&
      (QUILL_ERROR_CODES as readonly string[]).includes(code)
    ) {
      return { code: code as QuillErrorCode, message };
    }
  }
  return { code: "transport", message: "The quill command returned an unexpected error." };
}

export function quillNotice(error: unknown) {
  return QUILL_NOTICES[toQuillCommandError(error).code];
}

export function escapeHtml(value: string) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

type Timers = {
  setTimeout: (fn: () => void, ms: number) => unknown;
  clearTimeout: (id: unknown) => void;
  now: () => number;
};

// Looked up at call time, so vitest's fake timers apply.
const defaultTimers: Timers = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (id) => clearTimeout(id as ReturnType<typeof setTimeout>),
  now: () => Date.now(),
};

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------
export type QuillMode = {
  id: string;
  name: string;
  description: string;
  form: "task" | "system" | "skill" | "dictation";
  targetModel: "claude" | "gpt" | "generic" | "any";
  default: boolean;
};

const FORM_ORDER = ["task", "system", "skill", "dictation"];
const TARGET_ORDER = ["claude", "gpt", "generic", "any"];

function rank(order: string[], value: string) {
  const index = order.indexOf(value);
  return index === -1 ? order.length : index;
}

/** Form, then target model, then id. */
export function orderModes(modes: QuillMode[]) {
  return [...modes].sort(
    (a, b) =>
      rank(FORM_ORDER, a.form) - rank(FORM_ORDER, b.form) ||
      rank(TARGET_ORDER, a.targetModel) - rank(TARGET_ORDER, b.targetModel) ||
      (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
  );
}

/** The session choice, then the stored default when listed, then the catalog default. */
export function effectiveMode(modes: QuillMode[], sessionMode: string | null, storedDefault: string) {
  return (
    modes.find((entry) => entry.id === sessionMode) ??
    modes.find((entry) => entry.id === storedDefault) ??
    modes.find((entry) => entry.default) ??
    modes[0]
  );
}

export function modeName(modes: QuillMode[] | null, id: string) {
  return modes?.find((entry) => entry.id === id)?.name ?? id;
}

export class QuillCatalog {
  modes: QuillMode[] | null = null;
  private pending: Promise<QuillMode[]> | null = null;

  constructor(private readonly invoke: QuillInvoke) {}

  load(): Promise<QuillMode[]> {
    if (this.modes) return Promise.resolve(this.modes);
    this.pending ??= this.invoke("quill_list_modes")
      .then((result) => {
        this.modes = (result as { modes: QuillMode[] }).modes;
        return this.modes;
      })
      .finally(() => {
        this.pending = null;
      });
    return this.pending;
  }

  clear() {
    this.modes = null;
  }
}

export type QuillSettingsView = {
  quillEnabled?: boolean;
  quillCommandError?: string | null;
  quillDictationCleanup?: boolean;
  quillDefaultMode?: string;
};

/** The hook is usable only while it is on and its command validated. */
export function quillHookReady(settings: QuillSettingsView) {
  return Boolean(settings.quillEnabled) && !settings.quillCommandError;
}

/**
 * Whether a successful hotkey rewrite may open its revert slot. Rust arms the revert hotkey only
 * while the hook is ready, and a successful rewrite already means Rust saw it ready, so until the
 * frontend holds settings from Rust (before they load, or the load-failure fallback, which has no
 * quill fields) it defers to that; afterwards it follows the hook state Rust last reported.
 */
export function hotkeySlotAllowed(settings: QuillSettingsView | null) {
  if (!settings || !quillSettingsFromRust(settings)) return true;
  return quillHookReady(settings);
}

/** Whether these settings came from Rust; the load-failure fallback carries no quill fields. */
export function quillSettingsFromRust(settings: QuillSettingsView) {
  return settings.quillEnabled !== undefined;
}

export function dictationCleanupEffective(settings: QuillSettingsView) {
  return quillHookReady(settings) && Boolean(settings.quillDictationCleanup);
}

export type QuillModeSelect = {
  disabled: boolean;
  hint: string | null;
  options: { value: string; label: string }[];
  selected: string | null;
  modes: QuillMode[];
};

/**
 * The default-mode select. While the hook is off or invalid it is empty and disabled with a
 * hint, and the engine is not called; otherwise it lists the catalog by `name`.
 */
export async function loadModeSelect(settings: QuillSettingsView, invoke: QuillInvoke): Promise<QuillModeSelect> {
  if (!quillHookReady(settings)) {
    return { disabled: true, hint: QUILL_MODE_HINT, options: [], selected: null, modes: [] };
  }
  try {
    const { modes } = (await invoke("quill_list_modes")) as { modes: QuillMode[] };
    const ordered = orderModes(modes);
    return {
      disabled: false,
      hint: null,
      options: ordered.map((entry) => ({ value: entry.id, label: entry.name })),
      selected: effectiveMode(ordered, null, settings.quillDefaultMode ?? "")?.id ?? null,
      modes,
    };
  } catch (error) {
    return { disabled: true, hint: quillNotice(error), options: [], selected: null, modes: [] };
  }
}

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------
export type QuillPickerKey = "up" | "down" | "enter" | "escape" | string;

export type QuillPickerView = {
  open: boolean;
  rows: { id: string; name: string; selected: boolean; isDefault: boolean }[];
  description: string;
};

export class QuillPicker {
  isOpen = false;
  onRun: (modeId: string, pressedAtMs: number) => void;
  onClose: () => void;
  /** A close the picker started itself (idle timer, click) failed; `event` names which. */
  onError: (event: string, error: unknown) => void;
  private rows: QuillMode[] = [];
  private highlighted = 0;
  private defaultId = "";
  private timer: unknown = null;
  private readonly timers: Timers;

  constructor(private readonly deps: {
    announce: (text: string) => void;
    onRun?: (modeId: string, pressedAtMs: number) => void;
    onClose?: () => void;
    onError?: (event: string, error: unknown) => void;
    render?: (view: QuillPickerView) => void;
    timers?: Timers;
  }) {
    this.onRun = deps.onRun ?? (() => {});
    this.onClose = deps.onClose ?? (() => {});
    this.onError = deps.onError ?? (() => {});
    this.timers = deps.timers ?? defaultTimers;
  }

  open(modes: QuillMode[], defaultId: string) {
    if (this.isOpen || modes.length === 0) return;
    this.rows = orderModes(modes);
    this.defaultId = defaultId;
    this.highlighted = Math.max(0, this.rows.findIndex((entry) => entry.id === defaultId));
    this.isOpen = true;
    let shown = false;
    try {
      this.restartTimer();
      this.deps.announce(`Rewrite modes open. ${this.highlightText()}`);
      this.render();
      shown = true;
    } finally {
      // A failed open stays closed with no timer; the caller releases the keys it registered.
      if (!shown) {
        this.isOpen = false;
        this.clearTimer();
      }
    }
  }

  key(key: QuillPickerKey) {
    if (!this.isOpen) return;
    if (key === "up" || key === "down") {
      const step = key === "up" ? -1 : 1;
      this.highlighted = (this.highlighted + step + this.rows.length) % this.rows.length;
      this.restartTimer();
      this.deps.announce(this.highlightText());
      this.render();
    } else if (key === "enter") {
      this.run(this.highlighted);
    } else if (key === "escape") {
      this.close();
    } else if (/^[0-9]$/.test(key)) {
      const index = key === "0" ? 9 : Number(key) - 1;
      if (index < this.rows.length) this.run(index);
    }
  }

  /** A click on the widget while open closes the picker. */
  click() {
    this.dismiss("picker_click_failed");
  }

  /** Closes the picker for its owner; a render failure is reported as `event`, never thrown. */
  dismiss(event: string) {
    try {
      this.close();
    } catch (error) {
      this.onError(event, error);
    }
  }

  close() {
    if (!this.isOpen) return;
    this.isOpen = false;
    this.clearTimer();
    try {
      this.deps.announce("Rewrite modes closed");
      this.render();
    } finally {
      this.onClose();
    }
  }

  view(): QuillPickerView {
    return {
      open: this.isOpen,
      rows: this.rows.map((entry, index) => ({
        id: entry.id,
        name: entry.name,
        selected: index === this.highlighted,
        isDefault: entry.id === this.defaultId,
      })),
      description: this.rows[this.highlighted]?.description ?? "",
    };
  }

  private run(index: number) {
    const mode = this.rows[index];
    const pressedAtMs = this.timers.now();
    try {
      this.close();
    } finally {
      // The keys are released by close; a render failure must not drop the chosen rewrite.
      this.onRun(mode.id, pressedAtMs);
    }
  }

  private highlightText() {
    return `${this.rows[this.highlighted].name}, ${this.highlighted + 1} of ${this.rows.length}`;
  }

  private restartTimer() {
    this.clearTimer();
    this.timer = this.timers.setTimeout(() => {
      this.timer = null;
      try {
        this.close();
      } catch (error) {
        this.onError("picker_timer_failed", error);
      }
    }, QUILL_PICKER_IDLE_MS);
  }

  private clearTimer() {
    if (this.timer !== null) this.timers.clearTimeout(this.timer);
    this.timer = null;
  }

  private render() {
    this.deps.render?.(this.view());
  }
}

// ---------------------------------------------------------------------------
// Press window
// ---------------------------------------------------------------------------
type RewriteSelectionResult = {
  rewriteId: string;
  mode: string;
  model: string;
  latencyMs: number;
  endToEndMs: number;
  anchored: boolean;
  clipboardReplaced: boolean;
};

export type QuillRewritten = { rewriteId: string; name: string; delivered: "inserted" | "copied" };

/**
 * The `Control+Option+R` press window: one press rewrites with the effective default after
 * 600 ms, a second press inside the window opens the picker.
 */
export class QuillHotkey {
  sessionMode: string | null = null;
  inFlight = false;
  private pressTimer: unknown = null;
  private pickerOpening = false;
  private readonly timers: Timers;

  constructor(private readonly deps: {
    invoke: QuillInvoke;
    catalog: QuillCatalog;
    picker: QuillPicker;
    storedDefault: () => string;
    notify: QuillNotify;
    onRewritten: (rewrite: QuillRewritten) => void;
    /** Whether the hook is on with a valid command; while not, a rewrite opens no revert slot. */
    hookReady?: () => boolean;
    /** Reads settings from Rust after a rewrite, so the slot follows what Rust decided when arming. */
    readSettings?: () => Promise<QuillSettingsView>;
    log?: (line: string) => void;
    timers?: Timers;
  }) {
    this.timers = deps.timers ?? defaultTimers;
    deps.picker.onRun = (modeId, pressedAtMs) => {
      this.sessionMode = modeId;
      void this.dispatch(modeId, pressedAtMs);
    };
    deps.picker.onClose = () => {
      void this.releasePickerKeys();
    };
    deps.picker.onError = (event, error) => this.fail(event, error, false);
  }

  press() {
    if (this.deps.picker.isOpen || this.pickerOpening) return;
    if (this.inFlight) {
      this.deps.notify(QUILL_STILL_REWRITING, "status");
      return;
    }
    if (this.pressTimer !== null) {
      this.timers.clearTimeout(this.pressTimer);
      this.pressTimer = null;
      void this.openPicker();
      return;
    }
    const pressedAtMs = this.timers.now();
    void this.deps.catalog.load().catch(() => {});
    this.pressTimer = this.timers.setTimeout(() => {
      this.pressTimer = null;
      void this.dispatch(null, pressedAtMs);
    }, QUILL_PRESS_WINDOW_MS);
  }

  /** A picker key event. A render failure is logged; the key's run and release still happen. */
  pickerKey(key: QuillPickerKey) {
    try {
      this.deps.picker.key(key);
    } catch (error) {
      this.fail("picker_key_failed", error, false);
    }
  }

  private async openPicker() {
    this.pickerOpening = true;
    let registered = false;
    try {
      const modes = await this.deps.catalog.load();
      await this.deps.invoke("quill_picker_open");
      registered = true;
      const mode = effectiveMode(modes, this.sessionMode, this.deps.storedDefault());
      this.deps.picker.open(modes, mode?.id ?? "");
    } catch (error) {
      this.fail("picker_open_failed", error, true);
    } finally {
      this.pickerOpening = false;
      if (registered && !this.deps.picker.isOpen) await this.releasePickerKeys();
    }
  }

  /**
   * Rust re-checks the hook under the save lock before arming, and a save writes its settings
   * and disarms under the same lock. The read does not take that lock, so it shows settings as of
   * the read, not as of Rust's decision: a save before the re-check or between it and the read is
   * seen, and a save after the read closes the slot through the settings sync. A failed read falls
   * back to the last state.
   */
  private async slotAllowed() {
    if (this.deps.readSettings) {
      try {
        return hotkeySlotAllowed(await this.deps.readSettings());
      } catch {
        // Fall back to the last settings the frontend holds.
      }
    }
    return this.deps.hookReady?.() !== false;
  }

  /** Releases the native picker keys; a failure is logged, never shown as an alert. */
  private releasePickerKeys() {
    return this.deps.invoke("quill_picker_close").catch((error) => this.fail("picker_close_failed", error, false));
  }

  private async dispatch(modeId: string | null, pressedAtMs: number) {
    if (this.inFlight) return;
    this.inFlight = true;
    try {
      const modes = await this.deps.catalog.load();
      const mode = modeId ?? effectiveMode(modes, this.sessionMode, this.deps.storedDefault())?.id;
      if (!mode) return;
      const result = (await this.deps.invoke("quill_rewrite_selection", { mode, pressedAtMs })) as RewriteSelectionResult;
      // Rust arms the revert hotkey only while the hook is ready, so a slot opened otherwise
      // would only offer a revert that fails with `disabled`.
      if (await this.slotAllowed()) {
        this.deps.onRewritten({ rewriteId: result.rewriteId, name: modeName(modes, result.mode), delivered: "inserted" });
      }
      if (result.clipboardReplaced) this.deps.notify(QUILL_CLIPBOARD_REPLACED, "status");
    } catch (error) {
      this.fail("rewrite_failed", error, true);
    } finally {
      this.inFlight = false;
    }
  }

  private fail(event: string, error: unknown, notify: boolean) {
    const { code } = toQuillCommandError(error);
    this.deps.log?.(quillLogLine(event, code));
    if (notify) this.deps.notify(QUILL_NOTICES[code], "alert");
  }
}

// ---------------------------------------------------------------------------
// Dictation branch
// ---------------------------------------------------------------------------
export function shouldRewriteDictation(input: { cleanupEffective: boolean; copyOnly: boolean; streaming: boolean }) {
  return input.cleanupEffective && !input.copyOnly && !input.streaming;
}

export function recordingStartBlocked(state: string) {
  return state === "rewriting";
}

export function dictationFallbackNotice(code: QuillErrorCode) {
  if (code === "too_long") return "Transcript is over 8,000 characters. Inserted it without rewriting.";
  const cause = code === "timeout"
    ? "Rewrite timed out"
    : code === "no_key"
      ? "No OpenRouter key"
      : "Rewrite service unavailable";
  return `Rewrite failed (${cause}). Inserted the raw transcript.`;
}

export type QuillDictationRewrite = { rewriteId: string; text: string; mode: string };

/**
 * Rewrites a finished transcript under the 8,000 ms guard. Returns `null` on any error or at
 * the guard, after showing the fallback notice, so the raw transcript is delivered.
 */
export async function rewriteDictation(options: {
  invoke: QuillInvoke;
  text: string;
  notify: QuillNotify;
  announce: (text: string) => void;
  log?: (line: string) => void;
  timers?: Timers;
}): Promise<QuillDictationRewrite | null> {
  const timers = options.timers ?? defaultTimers;
  options.announce(QUILL_LIVE_REWRITING);
  let guard: unknown = null;
  const timedOut = new Promise<never>((_, reject) => {
    guard = timers.setTimeout(
      () => reject({ code: "timeout", message: "The dictation rewrite took too long." }),
      QUILL_DICTATION_GUARD_MS,
    );
  });
  try {
    const result = (await Promise.race([
      options.invoke("quill_rewrite_text", {
        text: options.text,
        mode: "dictation-cleanup",
        surface: "pill",
        sourceApp: null,
      }),
      timedOut,
    ])) as QuillDictationRewrite & Record<string, unknown>;
    return { rewriteId: result.rewriteId, text: result.text, mode: result.mode };
  } catch (error) {
    const { code } = toQuillCommandError(error);
    options.log?.(quillLogLine("dictation_rewrite_failed", code));
    options.notify(dictationFallbackNotice(code), "alert");
    return null;
  } finally {
    timers.clearTimeout(guard);
  }
}

export function dictationDelivered(delivery: { inserted: boolean } | null | undefined): QuillDelivered {
  if (!delivery) return "failed";
  return delivery.inserted ? "inserted" : "copied";
}

export function openSlotAfterDictation(delivered: QuillDelivered) {
  return delivered === "inserted" || delivered === "copied";
}

/**
 * After the unchanged delivery: release the clipboard hold (restore only after an inserted
 * paste), report the outcome, and open the revert slot when something was delivered.
 */
export async function settleDictationRewrite(options: {
  invoke: QuillInvoke;
  rewriteId: string;
  delivered: QuillDelivered;
  pasted: string;
  slot?: QuillRevertSlot;
  name?: string;
  log?: (line: string) => void;
}) {
  const { invoke, rewriteId, delivered, pasted } = options;
  await invoke("quill_clipboard_release", { restore: delivered === "inserted", pasted }).catch((error) => {
    options.log?.(quillLogLine("clipboard_release_failed", toQuillCommandError(error).code));
  });
  await invoke("quill_report_outcome", { rewriteId, delivered }).catch((error) => {
    options.log?.(quillLogLine("report_outcome_failed", toQuillCommandError(error).code));
  });
  if (options.slot && delivered !== "failed") {
    const armed = await invoke("quill_revert_arm", { rewriteId }).catch((error) => {
      options.log?.(quillLogLine("revert_arm_failed", toQuillCommandError(error).code));
      return null;
    });
    // `false` means the hook is off or its command invalid, so a revert would only fail.
    if (armed !== false) options.slot.open({ rewriteId, name: options.name ?? rewriteId, delivered });
  }
}

// ---------------------------------------------------------------------------
// Revert slot
// ---------------------------------------------------------------------------
export type QuillRevertView = { visible: boolean; label: string; countdown: string; text: string };

/** The 30 s revert slot, apart from the toast. Hook-core reports the revert's outcome. */
export class QuillRevertSlot {
  private active: (QuillRewritten & { endsAt: number }) | null = null;
  private timer: unknown = null;
  private reverting = false;
  private readonly timers: Timers;

  constructor(private readonly deps: {
    invoke: QuillInvoke;
    notify: QuillNotify;
    announce: (text: string) => void;
    render?: (view: QuillRevertView) => void;
    log?: (line: string) => void;
    timers?: Timers;
  }) {
    this.timers = deps.timers ?? defaultTimers;
  }

  /** Shows the slot for a new rewrite, replacing any earlier one. */
  open(rewrite: QuillRewritten) {
    this.clearTimer();
    this.active = { ...rewrite, endsAt: this.timers.now() + QUILL_REVERT_WINDOW_MS };
    this.deps.announce(`Rewritten with ${rewrite.name}.`);
    this.schedule();
    this.render();
  }

  view(): QuillRevertView {
    if (!this.active) return { visible: false, label: "", countdown: "", text: "" };
    const seconds = Math.max(0, Math.ceil((this.active.endsAt - this.timers.now()) / 1000));
    const label = `Rewritten with ${this.active.name}.`;
    const countdown = `(${seconds}s)`;
    return { visible: true, label, countdown, text: `${label} Revert ${countdown}` };
  }

  /** The Revert button. Only an open slot calls `quill_revert`. */
  async revert() {
    const active = this.active;
    if (!active || this.reverting) return;
    this.reverting = true;
    try {
      const result = (await this.deps.invoke("quill_revert", {
        rewriteId: active.rewriteId,
        delivered: active.delivered,
      })) as { delivered?: string } | null;
      if (result?.delivered === "copied") this.deps.notify(QUILL_REVERT_COPIED, "status");
    } catch (error) {
      const { code } = toQuillCommandError(error);
      this.deps.log?.(quillLogLine("revert_failed", code));
      this.deps.notify(QUILL_NOTICES[code], "alert");
    } finally {
      this.reverting = false;
      if (this.active === active) this.close();
    }
  }

  /** The `quill-revert` event; ignored when no slot is open. */
  revertFromHotkey() {
    return this.revert();
  }

  close() {
    this.clearTimer();
    this.active = null;
    this.render();
  }

  private schedule() {
    this.timer = this.timers.setTimeout(() => {
      this.timer = null;
      if (!this.active) return;
      if (this.timers.now() >= this.active.endsAt) {
        this.close();
        return;
      }
      this.render();
      this.schedule();
    }, 1000);
  }

  private clearTimer() {
    if (this.timer !== null) this.timers.clearTimeout(this.timer);
    this.timer = null;
  }

  private render() {
    this.deps.render?.(this.view());
  }
}
