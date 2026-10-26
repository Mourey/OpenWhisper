import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  QUILL_PICKER_IDLE_MS,
  QuillCatalog,
  QuillHotkey,
  QuillPicker,
  orderModes,
  type QuillMode,
} from "./quill-surfaces";

function mode(id: string, form: QuillMode["form"], targetModel: QuillMode["targetModel"], isDefault = false): QuillMode {
  return { id, name: `Name of ${id}`, description: `Description of ${id}.`, form, targetModel, default: isDefault };
}

// Shuffled on purpose: the picker orders by form, then target model, then id.
const TEN_MODES: QuillMode[] = [
  mode("dictation-cleanup", "dictation", "any"),
  mode("skill-generic", "skill", "generic"),
  mode("system-gpt", "system", "gpt"),
  mode("task-generic", "task", "generic"),
  mode("task-claude-brief", "task", "claude"),
  mode("skill-claude", "skill", "claude"),
  mode("system-generic", "system", "generic"),
  mode("task-gpt", "task", "gpt"),
  mode("task-claude", "task", "claude", true),
  mode("system-claude", "system", "claude"),
];

const ORDER = [
  "task-claude",
  "task-claude-brief",
  "task-gpt",
  "task-generic",
  "system-claude",
  "system-gpt",
  "system-generic",
  "skill-claude",
  "skill-generic",
  "dictation-cleanup",
];

function pickerHarness() {
  const announced: string[] = [];
  const runs: { modeId: string; pressedAtMs: number }[] = [];
  let closes = 0;
  const picker = new QuillPicker({
    announce: (text) => announced.push(text),
    onRun: (modeId, pressedAtMs) => runs.push({ modeId, pressedAtMs }),
    onClose: () => { closes += 1; },
  });
  return { picker, announced, runs, closes: () => closes };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-10-23T10:00:00.000Z"));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("quill-picker-model", () => {
  it("orders the ten modes by form, target model and id, with dictation-cleanup last", () => {
    expect(orderModes(TEN_MODES).map((entry) => entry.id)).toEqual(ORDER);
  });

  it("labels rows by name, marks the effective default and shows the highlighted description", () => {
    const { picker } = pickerHarness();
    picker.open(TEN_MODES, "task-gpt");
    const view = picker.view();
    expect(view.open).toBe(true);
    expect(view.rows.map((row) => row.name)).toEqual(ORDER.map((id) => `Name of ${id}`));
    expect(view.rows.filter((row) => row.isDefault).map((row) => row.id)).toEqual(["task-gpt"]);
    expect(view.rows.filter((row) => row.selected).map((row) => row.id)).toEqual(["task-gpt"]);
    expect(view.description).toBe("Description of task-gpt.");
    picker.key("down");
    expect(picker.view().description).toBe("Description of task-generic.");
    expect(picker.view().rows.filter((row) => row.isDefault).map((row) => row.id)).toEqual(["task-gpt"]);
  });

  it("wraps Up and Down and restarts the 10 s timer on each", async () => {
    const { picker, closes } = pickerHarness();
    picker.open(TEN_MODES, "task-claude");
    picker.key("up");
    expect(picker.view().rows.findIndex((row) => row.selected)).toBe(9);
    picker.key("down");
    expect(picker.view().rows.findIndex((row) => row.selected)).toBe(0);
    await vi.advanceTimersByTimeAsync(9000);
    picker.key("down");
    await vi.advanceTimersByTimeAsync(9000);
    picker.key("up");
    await vi.advanceTimersByTimeAsync(QUILL_PICKER_IDLE_MS - 1);
    expect(picker.isOpen).toBe(true);
    await vi.advanceTimersByTimeAsync(1);
    expect(picker.isOpen).toBe(false);
    expect(closes()).toBe(1);
  });

  it("runs row three on digit 3 and row ten on digit 0", () => {
    const first = pickerHarness();
    first.picker.open(TEN_MODES, "task-claude");
    first.picker.key("3");
    expect(first.runs.map((run) => run.modeId)).toEqual([ORDER[2]]);
    expect(first.picker.isOpen).toBe(false);
    const second = pickerHarness();
    second.picker.open(TEN_MODES, "task-claude");
    second.picker.key("0");
    expect(second.runs.map((run) => run.modeId)).toEqual([ORDER[9]]);
    expect(second.picker.isOpen).toBe(false);
  });

  it("closes after 10 s without a key", async () => {
    const { picker, announced, closes } = pickerHarness();
    picker.open(TEN_MODES, "task-claude");
    expect(QUILL_PICKER_IDLE_MS).toBe(10_000);
    await vi.advanceTimersByTimeAsync(9999);
    expect(picker.isOpen).toBe(true);
    await vi.advanceTimersByTimeAsync(1);
    expect(picker.isOpen).toBe(false);
    expect(closes()).toBe(1);
    expect(announced.at(-1)).toBe("Rewrite modes closed");
  });

  it("runs Enter with pressedAtMs taken at the Enter press and sets the session default", async () => {
    const calls: { cmd: string; args?: Record<string, unknown> }[] = [];
    const invoke = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
      calls.push({ cmd, args });
      if (cmd === "quill_list_modes") return { modes: TEN_MODES };
      if (cmd === "quill_rewrite_selection") {
        return { rewriteId: "r1", mode: args?.mode, model: "m", latencyMs: 1, endToEndMs: 2, anchored: true, clipboardReplaced: false };
      }
      return null;
    });
    const picker = new QuillPicker({ announce: () => {} });
    const hotkey = new QuillHotkey({
      invoke,
      catalog: new QuillCatalog(invoke),
      picker,
      storedDefault: () => "task-claude",
      notify: () => {},
      onRewritten: () => {},
    });
    hotkey.press();
    await vi.advanceTimersByTimeAsync(100);
    hotkey.press();
    await vi.advanceTimersByTimeAsync(0);
    expect(picker.isOpen).toBe(true);
    await vi.advanceTimersByTimeAsync(4000);
    hotkey.pickerKey("down");
    hotkey.pickerKey("down");
    await vi.advanceTimersByTimeAsync(2500);
    const enterAt = Date.now();
    hotkey.pickerKey("enter");
    await vi.advanceTimersByTimeAsync(0);
    const rewrites = calls.filter((call) => call.cmd === "quill_rewrite_selection");
    expect(rewrites).toEqual([{ cmd: "quill_rewrite_selection", args: { mode: ORDER[2], pressedAtMs: enterAt } }]);
    expect(picker.isOpen).toBe(false);
    expect(calls.map((call) => call.cmd)).toContain("quill_picker_close");
    expect(hotkey.sessionMode).toBe(ORDER[2]);
    hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(calls.filter((call) => call.cmd === "quill_rewrite_selection").at(-1)?.args?.mode).toBe(ORDER[2]);
  });

  it("closes on Escape and on a click without running a mode", () => {
    const escape = pickerHarness();
    escape.picker.open(TEN_MODES, "task-claude");
    escape.picker.key("escape");
    expect(escape.picker.isOpen).toBe(false);
    expect(escape.runs).toEqual([]);
    expect(escape.closes()).toBe(1);
    const click = pickerHarness();
    click.picker.open(TEN_MODES, "task-claude");
    click.picker.click();
    expect(click.picker.isOpen).toBe(false);
    expect(click.runs).toEqual([]);
    expect(click.closes()).toBe(1);
    click.picker.click();
    expect(click.closes()).toBe(1);
  });

  it("announces open, each highlight as <name>, <n> of <total>, and close", () => {
    const { picker, announced } = pickerHarness();
    picker.open(TEN_MODES, "task-claude");
    picker.key("down");
    picker.key("up");
    picker.key("up");
    picker.key("escape");
    expect(announced).toEqual([
      "Rewrite modes open. Name of task-claude, 1 of 10",
      "Name of task-claude-brief, 2 of 10",
      "Name of task-claude, 1 of 10",
      "Name of dictation-cleanup, 10 of 10",
      "Rewrite modes closed",
    ]);
  });

  it("ignores keys while closed", () => {
    const { picker, announced, runs } = pickerHarness();
    picker.key("enter");
    picker.key("1");
    picker.key("down");
    expect(announced).toEqual([]);
    expect(runs).toEqual([]);
  });

  it("releases the picker keys even when the close fails to render", () => {
    let fail = false;
    let closes = 0;
    const picker = new QuillPicker({
      announce: () => {},
      onClose: () => { closes += 1; },
      render: () => {
        if (fail) throw new Error("render failed");
      },
    });
    picker.open(TEN_MODES, "task-claude");
    fail = true;
    expect(() => picker.close()).toThrow("render failed");
    expect(picker.isOpen).toBe(false);
    expect(closes).toBe(1);
  });

  it("stays closed with no timer when the open fails to render", async () => {
    let closes = 0;
    const picker = new QuillPicker({
      announce: () => {},
      onClose: () => { closes += 1; },
      render: () => {
        throw new Error("render failed");
      },
    });
    expect(() => picker.open(TEN_MODES, "task-claude")).toThrow("render failed");
    expect(picker.isOpen).toBe(false);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(closes).toBe(0);
  });
});

describe("quill-picker-markup", () => {
  const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
  const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
  const shellStart = main.indexOf('<div id="widget-shell">');
  const toastAt = main.indexOf('<div id="toast" class="toast hidden"></div>');

  it("puts #quill-picker in #widget-shell as a listbox named Rewrite modes", () => {
    const picker = main.match(/<div id="quill-picker"[^>]*>/)?.[0];
    expect(picker).toBeDefined();
    expect(picker).toContain('role="listbox"');
    expect(picker).toContain('aria-label="Rewrite modes"');
    const at = main.indexOf('<div id="quill-picker"');
    expect(at).toBeGreaterThan(shellStart);
    expect(at).toBeLessThan(toastAt);
  });

  it("renders rows as options with aria-selected", () => {
    expect(main).toContain('role="option"');
    expect(main).toContain('aria-selected="${row.selected}"');
  });

  it("marks the default with an aria-hidden check and visually hidden text", () => {
    expect(main).toMatch(/<span class="quill-picker-check" aria-hidden="true">[^<]+<\/span><span class="visually-hidden">default<\/span>/);
  });

  it("has a polite, visually hidden #quill-live region", () => {
    const live = main.match(/<div id="quill-live"[^>]*>/)?.[0];
    expect(live).toBeDefined();
    expect(live).toContain('aria-live="polite"');
    expect(live).toContain('class="visually-hidden"');
    expect(css).toMatch(/\.visually-hidden \{[^}]*clip: rect\(0, 0, 0, 0\)/);
  });

  it("keeps #quill-revert in #widget-shell, apart from #toast", () => {
    const at = main.indexOf('<div id="quill-revert"');
    expect(at).toBeGreaterThan(shellStart);
    expect(at).toBeLessThan(toastAt);
    expect(main).toContain('<div id="toast" class="toast hidden"></div>');
    const revert = main.slice(at, main.indexOf("</div>", at));
    expect(revert).not.toContain("toast");
  });

  it("gives the revert button aria-keyshortcuts Control+Alt+Z", () => {
    expect(main).toMatch(/<button id="quill-revert-button"[^>]*aria-keyshortcuts="Control\+Alt\+Z"/);
  });

  it("styles the picker without pointer events, hides the inactive slot and respects reduced motion", () => {
    const pickerRule = css.match(/#quill-picker \{([\s\S]*?)\n\}/)?.[1];
    expect(pickerRule).toBeDefined();
    expect(pickerRule).toContain("pointer-events: none");
    const inactive = css.match(/#quill-revert\[data-active="false"\] \{([\s\S]*?)\n\}/)?.[1];
    expect(inactive).toBeDefined();
    expect(inactive).toContain("visibility: hidden");
    const reduced = css.match(/@media \(prefers-reduced-motion: reduce\) \{([\s\S]*?)\n\}/)?.[1];
    expect(reduced).toBeDefined();
    expect(reduced).toContain("rewriting");
    expect(reduced).toContain("animation: none");
  });
});
