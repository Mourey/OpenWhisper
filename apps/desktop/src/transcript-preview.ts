export type PasteResult = {
  pasted: boolean;
  reason?: "accessibility_permission" | "no_target" | "target_changed" | "modifiers_held" | "unsupported_target";
};

export type TranscriptDelivery = {
  status: string;
  detail: string;
  needsPasteAccess: boolean;
};

export const TRANSCRIPT_PREVIEW_WIDTH = 320;
export const TRANSCRIPT_PREVIEW_DURATION_MS = 4000;

export function describeTranscriptDelivery(result: PasteResult | undefined, isMac: boolean): TranscriptDelivery {
  if (result?.pasted) return { status: "Inserted at cursor", detail: "", needsPasteAccess: false };
  const details = {
    accessibility_permission: "Enable Accessibility for automatic insertion.",
    no_target: "No text field was captured.",
    target_changed: "The target changed before insertion.",
    modifiers_held: "A shortcut key is still held.",
    unsupported_target: "This app could not accept insertion.",
  };
  return {
    status: `Copied — use ${isMac ? "Command" : "Ctrl"}+V`,
    detail: result?.reason ? details[result.reason] ?? "" : "",
    needsPasteAccess: isMac && result?.reason === "accessibility_permission",
  };
}

export async function deliverTranscript(options: {
  copyOnly: boolean;
  isMac: boolean;
  isCurrent: () => boolean;
  paste: () => Promise<PasteResult | undefined>;
  copy: () => Promise<void>;
}): Promise<TranscriptDelivery | null> {
  if (!options.isCurrent()) return null;
  const result = options.copyOnly ? undefined : await options.paste();
  if (!options.isCurrent()) return null;
  if (!result?.pasted) {
    await options.copy();
    if (!options.isCurrent()) return null;
  }
  return describeTranscriptDelivery(result, options.isMac);
}

type PreviewElement = { hidden: boolean; textContent: string | null };
type PreviewElements = {
  root: Pick<PreviewElement, "hidden">;
  transcript: Pick<PreviewElement, "textContent">;
  status: Pick<PreviewElement, "textContent">;
  detail: PreviewElement;
  pasteAccess: Pick<PreviewElement, "hidden">;
};

export function createTranscriptPreview(elements: PreviewElements, options: {
  onVisibilityChange: () => void;
  setTimer?: (callback: () => void, ms: number) => unknown;
  clearTimer?: (timer: unknown) => void;
}) {
  const setTimer = options.setTimer ?? ((callback, ms) => setTimeout(callback, ms));
  const clearTimer = options.clearTimer ?? ((timer) => clearTimeout(timer as ReturnType<typeof setTimeout>));
  let timer: unknown;
  let generation = 0;
  let visible = false;

  function dismiss() {
    generation += 1;
    if (timer !== undefined) clearTimer(timer);
    timer = undefined;
    elements.root.hidden = true;
    elements.transcript.textContent = "";
    elements.status.textContent = "";
    elements.detail.textContent = "";
    elements.detail.hidden = true;
    elements.pasteAccess.hidden = true;
    if (visible) {
      visible = false;
      options.onVisibilityChange();
    }
    return generation;
  }

  return {
    get visible() { return visible; },
    dismiss,
    show(text: string, delivery: TranscriptDelivery, expectedGeneration = generation) {
      if (expectedGeneration !== generation) return;
      dismiss();
      if (!text.trim()) return;
      elements.transcript.textContent = text;
      elements.status.textContent = delivery.status;
      elements.detail.textContent = delivery.detail;
      elements.detail.hidden = !delivery.detail;
      elements.pasteAccess.hidden = !delivery.needsPasteAccess;
      elements.root.hidden = false;
      visible = true;
      const currentGeneration = generation;
      timer = setTimer(() => {
        if (generation === currentGeneration) dismiss();
      }, TRANSCRIPT_PREVIEW_DURATION_MS);
      options.onVisibilityChange();
    },
  };
}

export function withTranscriptPreviewLayout<T extends { width: number; height: number; visible: boolean }>(
  layout: T,
  visible: boolean,
  previewHeight: number,
  bottomOffset: number,
): T {
  if (!visible) return layout;
  return {
    ...layout,
    width: Math.max(layout.width, TRANSCRIPT_PREVIEW_WIDTH),
    height: Math.max(layout.height, Math.ceil(previewHeight) + bottomOffset),
    visible: true,
  };
}
