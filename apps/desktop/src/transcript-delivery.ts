export type PasteResult = {
  pasted: boolean;
  reason?: "accessibility_permission" | "no_target" | "target_changed" | "modifiers_held" | "unsupported_target";
};

export type TranscriptDelivery = {
  inserted: boolean;
  /** Shown only when the text could not be inserted and was copied instead. */
  notice: string | null;
  needsPasteAccess: boolean;
};

/** An insertion that never answers must not keep the app out of idle. */
export const PASTE_TIMEOUT_MS = 5000;

export function describeTranscriptDelivery(result: PasteResult | undefined, isMac: boolean): TranscriptDelivery {
  if (result?.pasted) return { inserted: true, notice: null, needsPasteAccess: false };
  const details = {
    accessibility_permission: "Enable Accessibility for automatic insertion.",
    no_target: "No text field was captured.",
    target_changed: "The target changed before insertion.",
    modifiers_held: "A shortcut key is still held.",
    unsupported_target: "This app could not accept insertion.",
  };
  const detail = result?.reason ? details[result.reason] ?? "" : "";
  return {
    inserted: false,
    notice: `Copied — use ${isMac ? "Command" : "Ctrl"}+V${detail ? `. ${detail}` : ""}`,
    needsPasteAccess: isMac && result?.reason === "accessibility_permission",
  };
}

function settleWithin<T>(promise: Promise<T>, timeoutMs: number): Promise<T | undefined> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => resolve(undefined), timeoutMs);
    promise.then(
      (value) => { clearTimeout(timer); resolve(value); },
      (error) => { clearTimeout(timer); reject(error); },
    );
  });
}

export async function deliverTranscript(options: {
  copyOnly: boolean;
  isMac: boolean;
  isCurrent: () => boolean;
  paste: () => Promise<PasteResult | undefined>;
  copy: () => Promise<void>;
  pasteTimeoutMs?: number;
}): Promise<TranscriptDelivery | null> {
  if (!options.isCurrent()) return null;
  const result = options.copyOnly
    ? undefined
    : await settleWithin(options.paste(), options.pasteTimeoutMs ?? PASTE_TIMEOUT_MS);
  if (!options.isCurrent()) return null;
  if (!result?.pasted) {
    await options.copy();
    if (!options.isCurrent()) return null;
  }
  return describeTranscriptDelivery(result, options.isMac);
}
