// Pure logic of the quill hook surfaces: the dictation
// branch and the notice table. It imports nothing from Tauri and touches
// no DOM, so vitest runs it in node; main.ts wires it to events, markup and `invoke`.

export const QUILL_DICTATION_GUARD_MS = 8000;

export const QUILL_LIVE_REWRITING = "Rewriting transcript";

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

export type QuillSettingsView = {
  quillEnabled?: boolean;
  quillCommandError?: string | null;
  quillDictationCleanup?: boolean;
};

/** The hook is usable only while it is on and its command validated. */
export function quillHookReady(settings: QuillSettingsView) {
  return Boolean(settings.quillEnabled) && !settings.quillCommandError;
}

export function dictationCleanupEffective(settings: QuillSettingsView) {
  return quillHookReady(settings) && Boolean(settings.quillDictationCleanup);
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

/**
 * After the unchanged delivery: release the clipboard hold (restore only after an inserted
 * paste) and report the outcome.
 */
export async function settleDictationRewrite(options: {
  invoke: QuillInvoke;
  rewriteId: string;
  delivered: QuillDelivered;
  pasted: string;
  /** Leaves the pasted text on the clipboard: the release restores nothing. */
  keepPasted?: boolean;
  log?: (line: string) => void;
}) {
  const { invoke, rewriteId, delivered, pasted, keepPasted } = options;
  await invoke("quill_clipboard_release", { restore: delivered === "inserted" && !keepPasted, pasted }).catch((error) => {
    options.log?.(quillLogLine("clipboard_release_failed", toQuillCommandError(error).code));
  });
  await invoke("quill_report_outcome", { rewriteId, delivered }).catch((error) => {
    options.log?.(quillLogLine("report_outcome_failed", toQuillCommandError(error).code));
  });
}
