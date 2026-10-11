import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createTranscriptPreview,
  deliverTranscript,
  describeTranscriptDelivery,
  TRANSCRIPT_PREVIEW_DURATION_MS,
  withTranscriptPreviewLayout,
  type PasteResult,
} from "./transcript-preview";

function element() {
  const value = { hidden: true, textContent: "" as string | null };
  Object.defineProperty(value, "innerHTML", { set() { throw new Error("HTML is not a safe transcript sink"); } });
  return value;
}

function previewElements() {
  return {
    root: element(), transcript: element(), status: element(), detail: element(), pasteAccess: element(),
  };
}

afterEach(() => vi.useRealTimers());

describe("transcript delivery", () => {
  it("reports cursor insertion only when the backend confirms it", async () => {
    const copy = vi.fn();
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => true,
      paste: async () => ({ pasted: true }), copy,
    });
    expect(delivery?.status).toBe("Inserted at cursor");
    expect(copy).not.toHaveBeenCalled();
  });

  it.each<PasteResult["reason"]>([
    "accessibility_permission", "no_target", "target_changed", "modifiers_held", "unsupported_target",
  ])("copies the full transcript and explains a %s fallback", async (reason) => {
    const copy = vi.fn(async () => {});
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => true,
      paste: async () => ({ pasted: false, reason }), copy,
    });
    expect(copy).toHaveBeenCalledOnce();
    expect(delivery?.status).toBe("Copied — use Command+V");
    expect(delivery?.detail).not.toBe("");
    expect(delivery?.needsPasteAccess).toBe(reason === "accessibility_permission");
  });

  it("keeps imports copy-only and uses the platform's paste shortcut", async () => {
    const paste = vi.fn();
    const copy = vi.fn(async () => {});
    const delivery = await deliverTranscript({ copyOnly: true, isMac: false, isCurrent: () => true, paste, copy });
    expect(paste).not.toHaveBeenCalled();
    expect(copy).toHaveBeenCalledOnce();
    expect(delivery?.status).toBe("Copied — use Ctrl+V");
    expect(delivery?.needsPasteAccess).toBe(false);
  });

  it("does not claim insertion for an older void response", async () => {
    const copy = vi.fn(async () => {});
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => true, paste: async () => undefined, copy,
    });
    expect(copy).toHaveBeenCalledOnce();
    expect(delivery?.status).toBe("Copied — use Command+V");
  });

  it("does not deliver a cancelled or replaced session", async () => {
    const paste = vi.fn();
    const copy = vi.fn();
    expect(await deliverTranscript({ copyOnly: false, isMac: true, isCurrent: () => false, paste, copy })).toBeNull();
    expect(paste).not.toHaveBeenCalled();
    expect(copy).not.toHaveBeenCalled();
  });

  it("does not copy or preview if cancellation arrives during insertion", async () => {
    let current = true;
    const copy = vi.fn();
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => current,
      paste: async () => { current = false; return { pasted: false, reason: "target_changed" }; }, copy,
    });
    expect(delivery).toBeNull();
    expect(copy).not.toHaveBeenCalled();
  });

  it("does not preview a session cancelled while copying", async () => {
    let current = true;
    const delivery = await deliverTranscript({
      copyOnly: true, isMac: true, isCurrent: () => current, paste: vi.fn(),
      copy: async () => { current = false; },
    });
    expect(delivery).toBeNull();
  });

  it("does not report copied if clipboard delivery fails", async () => {
    await expect(deliverTranscript({
      copyOnly: true, isMac: true, isCurrent: () => true, paste: vi.fn(),
      copy: async () => { throw new Error("clipboard unavailable"); },
    })).rejects.toThrow("clipboard unavailable");
  });
});

describe("transcript preview", () => {
  it("keeps hidden idle visible for four seconds, then restores its layout", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const idle = { width: 92, height: 28, visible: false, anchorOffsetY: 2 };
    let layout = idle;
    const preview = createTranscriptPreview(elements, {
      onVisibilityChange: () => { layout = withTranscriptPreviewLayout(idle, preview.visible, 140, 40); },
    });
    preview.show("Hello", describeTranscriptDelivery({ pasted: true }, true));
    expect(layout).toEqual({ width: 320, height: 180, visible: true, anchorOffsetY: 2 });
    expect(elements.root.hidden).toBe(false);
    vi.advanceTimersByTime(TRANSCRIPT_PREVIEW_DURATION_MS - 1);
    expect(preview.visible).toBe(true);
    vi.advanceTimersByTime(1);
    expect(preview.visible).toBe(false);
    expect(elements.root.hidden).toBe(true);
    expect(layout).toBe(idle);
  });

  it("replaces a previous preview and ignores an already queued old timer", () => {
    const elements = previewElements();
    const callbacks: Array<() => void> = [];
    const clearTimer = vi.fn();
    const preview = createTranscriptPreview(elements, {
      onVisibilityChange: vi.fn(),
      setTimer: (callback) => { callbacks.push(callback); return callbacks.length; }, clearTimer,
    });
    const delivery = describeTranscriptDelivery({ pasted: true }, true);
    preview.show("First", delivery);
    preview.show("Second", delivery);
    callbacks[0]();
    expect(preview.visible).toBe(true);
    expect(elements.transcript.textContent).toBe("Second");
    expect(clearTimer).toHaveBeenCalledWith(1);
    callbacks[1]();
    expect(preview.visible).toBe(false);
  });

  it.each(["new recording", "cancellation"])("invalidates the old preview on %s", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const preview = createTranscriptPreview(elements, { onVisibilityChange: vi.fn() });
    preview.show("Old text", describeTranscriptDelivery({ pasted: true }, true));
    preview.dismiss();
    expect(preview.visible).toBe(false);
    expect(elements.transcript.textContent).toBe("");
    expect(vi.getTimerCount()).toBe(0);
    vi.advanceTimersByTime(4000);
    expect(elements.root.hidden).toBe(true);
  });

  it("rejects a late completion from a cancelled or replaced recording", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const preview = createTranscriptPreview(elements, { onVisibilityChange: vi.fn() });
    const firstAttempt = preview.dismiss();
    const nextAttempt = preview.dismiss();
    const delivery = describeTranscriptDelivery({ pasted: true }, true);
    preview.show("Cancelled completion", delivery, firstAttempt);
    expect(preview.visible).toBe(false);
    preview.show("Current completion", delivery, nextAttempt);
    preview.show("Old completion", delivery, firstAttempt);
    expect(elements.transcript.textContent).toBe("Current completion");
    expect(vi.getTimerCount()).toBe(1);
  });

  it("renders Unicode and markup as unchanged plaintext with an explicit permission action", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const preview = createTranscriptPreview(elements, { onVisibilityChange: vi.fn() });
    const text = 'Español 中文 🎙️\n<img src=x onerror="alert(1)">';
    preview.show(text, describeTranscriptDelivery({ pasted: false, reason: "accessibility_permission" }, true));
    expect(elements.transcript.textContent).toBe(text);
    expect(elements.status.textContent).toBe("Copied — use Command+V");
    expect(elements.detail.hidden).toBe(false);
    expect(elements.pasteAccess.hidden).toBe(false);
    preview.show("Inserted", describeTranscriptDelivery({ pasted: true }, true));
    expect(elements.detail.hidden).toBe(true);
    expect(elements.pasteAccess.hidden).toBe(true);
  });

  it("does not show an empty transcript or shrink an open menu", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const preview = createTranscriptPreview(elements, { onVisibilityChange: vi.fn() });
    preview.show("  \n", describeTranscriptDelivery({ pasted: true }, true));
    expect(preview.visible).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
    const menu = { width: 390, height: 550, visible: true };
    expect(withTranscriptPreviewLayout(menu, true, 100, 40)).toEqual(menu);
  });
});
