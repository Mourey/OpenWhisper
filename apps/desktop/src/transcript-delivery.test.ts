import { afterEach, describe, expect, it, vi } from "vitest";
import {
  deliverTranscript,
  describeTranscriptDelivery,
  type PasteResult,
} from "./transcript-delivery";

afterEach(() => vi.useRealTimers());

describe("transcript delivery", () => {
  it("reports cursor insertion only when the backend confirms it", async () => {
    const copy = vi.fn();
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => true,
      paste: async () => ({ pasted: true }), copy,
    });
    expect(delivery).toMatchObject({ inserted: true, notice: null });
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
    expect(delivery?.inserted).toBe(false);
    expect(delivery?.notice).toMatch(/^Copied — use Command\+V\. \S/);
    expect(delivery?.needsPasteAccess).toBe(reason === "accessibility_permission");
  });

  it("keeps imports copy-only and uses the platform's paste shortcut", async () => {
    const paste = vi.fn();
    const copy = vi.fn(async () => {});
    const delivery = await deliverTranscript({ copyOnly: true, isMac: false, isCurrent: () => true, paste, copy });
    expect(paste).not.toHaveBeenCalled();
    expect(copy).toHaveBeenCalledOnce();
    expect(delivery?.notice).toBe("Copied — use Ctrl+V");
    expect(delivery?.needsPasteAccess).toBe(false);
  });

  it("does not claim insertion for an older void response", async () => {
    const copy = vi.fn(async () => {});
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => true, paste: async () => undefined, copy,
    });
    expect(copy).toHaveBeenCalledOnce();
    expect(delivery?.notice).toBe("Copied — use Command+V");
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

  it("falls back to the clipboard when insertion never answers", async () => {
    const copy = vi.fn(async () => {});
    const delivery = await deliverTranscript({
      copyOnly: false, isMac: true, isCurrent: () => true,
      paste: () => new Promise<PasteResult>(() => {}), copy, pasteTimeoutMs: 10,
    });
    expect(copy).toHaveBeenCalledOnce();
    expect(delivery?.inserted).toBe(false);
  });
});
