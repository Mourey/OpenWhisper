import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  QUILL_REVERT_COPIED,
  QUILL_REVERT_WINDOW_MS,
  QuillCatalog,
  QuillHotkey,
  QuillPicker,
  QuillRevertSlot,
  openSlotAfterDictation,
  settleDictationRewrite,
} from "./quill-surfaces";

type Call = { cmd: string; args?: Record<string, unknown> };

function slotHarness(revertResult: unknown = { delivered: "inserted" }) {
  const calls: Call[] = [];
  const notices: { text: string; role: string }[] = [];
  const announced: string[] = [];
  const invoke = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "quill_revert") return revertResult;
    return null;
  });
  const slot = new QuillRevertSlot({
    invoke,
    notify: (text, role) => notices.push({ text, role }),
    announce: (text) => announced.push(text),
  });
  return { calls, notices, announced, invoke, slot };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("quill-revert-slot", () => {
  it("calls quill_revert with the exact argument object", async () => {
    const h = slotHarness();
    h.slot.open({ rewriteId: "r-1", name: "Task prompt for Claude", delivered: "inserted" });
    await h.slot.revert();
    expect(h.calls).toEqual([{ cmd: "quill_revert", args: { rewriteId: "r-1", delivered: "inserted" } }]);
  });

  it("is visible for 30 s with the countdown, then hidden", async () => {
    const h = slotHarness();
    expect(h.slot.view().visible).toBe(false);
    h.slot.open({ rewriteId: "r-1", name: "Task prompt for Claude", delivered: "inserted" });
    expect(QUILL_REVERT_WINDOW_MS).toBe(30_000);
    expect(h.slot.view()).toEqual({
      visible: true,
      label: "Rewritten with Task prompt for Claude.",
      countdown: "(30s)",
      text: "Rewritten with Task prompt for Claude. Revert (30s)",
    });
    await vi.advanceTimersByTimeAsync(1000);
    expect(h.slot.view().text).toBe("Rewritten with Task prompt for Claude. Revert (29s)");
    await vi.advanceTimersByTimeAsync(28_000);
    expect(h.slot.view().countdown).toBe("(1s)");
    await vi.advanceTimersByTimeAsync(999);
    expect(h.slot.view().visible).toBe(true);
    await vi.advanceTimersByTimeAsync(1);
    expect(h.slot.view().visible).toBe(false);
  });

  it("reverts the slot's rewrite from the button and from the hotkey event", async () => {
    const button = slotHarness();
    button.slot.open({ rewriteId: "r-button", name: "A", delivered: "inserted" });
    await button.slot.revert();
    expect(button.calls).toEqual([{ cmd: "quill_revert", args: { rewriteId: "r-button", delivered: "inserted" } }]);
    expect(button.slot.view().visible).toBe(false);
    const hotkey = slotHarness();
    hotkey.slot.open({ rewriteId: "r-hotkey", name: "A", delivered: "copied" });
    await hotkey.slot.revertFromHotkey();
    expect(hotkey.calls).toEqual([{ cmd: "quill_revert", args: { rewriteId: "r-hotkey", delivered: "copied" } }]);
    expect(hotkey.slot.view().visible).toBe(false);
  });

  it("shows the copy notice when the original was copied", async () => {
    const h = slotHarness({ delivered: "copied" });
    h.slot.open({ rewriteId: "r-1", name: "A", delivered: "inserted" });
    await h.slot.revert();
    expect(QUILL_REVERT_COPIED).toBe("Original copied. Press Command+V to restore.");
    expect(h.notices).toEqual([{ text: QUILL_REVERT_COPIED, role: "status" }]);
  });

  it("restarts the count when a new rewrite replaces the slot", async () => {
    const h = slotHarness();
    h.slot.open({ rewriteId: "r-old", name: "Old", delivered: "inserted" });
    await vi.advanceTimersByTimeAsync(20_000);
    h.slot.open({ rewriteId: "r-new", name: "New", delivered: "inserted" });
    expect(h.slot.view().text).toBe("Rewritten with New. Revert (30s)");
    await vi.advanceTimersByTimeAsync(29_999);
    expect(h.slot.view().visible).toBe(true);
    await h.slot.revertFromHotkey();
    expect(h.calls).toEqual([{ cmd: "quill_revert", args: { rewriteId: "r-new", delivered: "inserted" } }]);
  });

  it("stays visible while a toast is shown", async () => {
    const h = slotHarness();
    h.slot.open({ rewriteId: "r-1", name: "A", delivered: "inserted" });
    // A hotkey notice goes to the toast while the slot is open.
    const toasts: string[] = [];
    const hotkeyInvoke = vi.fn(async (cmd: string) =>
      cmd === "quill_list_modes"
        ? { modes: [{ id: "task-claude", name: "A", description: "", form: "task", targetModel: "claude", default: true }] }
        : new Promise(() => {}));
    const hotkey = new QuillHotkey({
      invoke: hotkeyInvoke,
      catalog: new QuillCatalog(hotkeyInvoke),
      picker: new QuillPicker({ announce: () => {} }),
      storedDefault: () => "task-claude",
      notify: (text) => toasts.push(text),
      onRewritten: () => {},
    });
    hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    hotkey.press();
    expect(toasts).toEqual(["Still rewriting the last selection."]);
    await vi.advanceTimersByTimeAsync(5400);
    expect(h.slot.view().visible).toBe(true);
    expect(h.slot.view().countdown).toBe("(24s)");
  });

  it("announces once per rewrite, never each second", async () => {
    const h = slotHarness();
    h.slot.open({ rewriteId: "r-1", name: "A", delivered: "inserted" });
    await vi.advanceTimersByTimeAsync(30_000);
    expect(h.announced).toEqual(["Rewritten with A."]);
    h.slot.open({ rewriteId: "r-2", name: "B", delivered: "inserted" });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(h.announced).toEqual(["Rewritten with A.", "Rewritten with B."]);
  });

  it("reports no second outcome for a revert", async () => {
    const h = slotHarness({ delivered: "copied" });
    h.slot.open({ rewriteId: "r-1", name: "A", delivered: "inserted" });
    await h.slot.revert();
    await h.slot.revert();
    expect(h.calls.map((call) => call.cmd)).toEqual(["quill_revert"]);
  });

  it("invokes nothing for a quill-revert event with no open slot", async () => {
    const h = slotHarness();
    await h.slot.revertFromHotkey();
    h.slot.open({ rewriteId: "r-1", name: "A", delivered: "inserted" });
    await vi.advanceTimersByTimeAsync(QUILL_REVERT_WINDOW_MS);
    await h.slot.revertFromHotkey();
    expect(h.calls).toEqual([]);
  });

  it("opens after a dictation rewrite only when it was inserted or copied", async () => {
    expect(openSlotAfterDictation("inserted")).toBe(true);
    expect(openSlotAfterDictation("copied")).toBe(true);
    expect(openSlotAfterDictation("failed")).toBe(false);
    for (const delivered of ["inserted", "copied", "failed"] as const) {
      const h = slotHarness();
      await settleDictationRewrite({ invoke: h.invoke, rewriteId: "r-d", delivered, pasted: "text", slot: h.slot, name: "Dictation cleanup" });
      expect(h.slot.view().visible).toBe(delivered !== "failed");
      if (delivered !== "failed") {
        await h.slot.revertFromHotkey();
        expect(h.calls.at(-1)).toEqual({ cmd: "quill_revert", args: { rewriteId: "r-d", delivered } });
      }
    }
  });

  it("hides the countdown element from assistive technology", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const countdown = main.match(/<span id="quill-revert-countdown"[^>]*>/)?.[0];
    expect(countdown).toBeDefined();
    expect(countdown).toContain('aria-hidden="true"');
    expect(main).toContain("quillRevertCountdown.textContent = view.countdown");
  });
});
