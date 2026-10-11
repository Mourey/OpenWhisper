import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createTranscriptPreview,
  describeTranscriptDelivery,
  TRANSCRIPT_PREVIEW_DURATION_MS,
  withTranscriptPreviewLayout,
} from "./transcript-preview";

function element() {
  const value = { hidden: true, textContent: "" as string | null };
  Object.defineProperty(value, "innerHTML", { set() { throw new Error("HTML is not a safe transcript sink"); } });
  return value;
}

function previewElements() {
  return {
    root: element(), transcript: element(), status: element(), detail: element(),
  };
}

afterEach(() => vi.useRealTimers());

describe("transcript delivery", () => {
  it("never claims cursor insertion and names the platform's paste shortcut", () => {
    expect(describeTranscriptDelivery(true)).toEqual({ status: "Copied — use Command+V", detail: "" });
    expect(describeTranscriptDelivery(false)).toEqual({ status: "Copied — use Ctrl+V", detail: "" });
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
    preview.show("Hello", describeTranscriptDelivery(true));
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
    const delivery = describeTranscriptDelivery(true);
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
    preview.show("Old text", describeTranscriptDelivery(true));
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
    const delivery = describeTranscriptDelivery(true);
    preview.show("Cancelled completion", delivery, firstAttempt);
    expect(preview.visible).toBe(false);
    preview.show("Current completion", delivery, nextAttempt);
    preview.show("Old completion", delivery, firstAttempt);
    expect(elements.transcript.textContent).toBe("Current completion");
    expect(vi.getTimerCount()).toBe(1);
  });

  it("renders Unicode and markup as unchanged plaintext", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const preview = createTranscriptPreview(elements, { onVisibilityChange: vi.fn() });
    const text = 'Español 中文 🎙️\n<img src=x onerror="alert(1)">';
    preview.show(text, describeTranscriptDelivery(true));
    expect(elements.transcript.textContent).toBe(text);
    expect(elements.status.textContent).toBe("Copied — use Command+V");
    expect(elements.detail.hidden).toBe(true);
  });

  it("does not show an empty transcript or shrink an open menu", () => {
    vi.useFakeTimers();
    const elements = previewElements();
    const preview = createTranscriptPreview(elements, { onVisibilityChange: vi.fn() });
    preview.show("  \n", describeTranscriptDelivery(true));
    expect(preview.visible).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
    const menu = { width: 390, height: 550, visible: true };
    expect(withTranscriptPreviewLayout(menu, true, 100, 40)).toEqual(menu);
  });
});
