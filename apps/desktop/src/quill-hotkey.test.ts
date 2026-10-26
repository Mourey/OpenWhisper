import { existsSync, readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  QUILL_ERROR_CODES,
  QUILL_NOTICES,
  QUILL_STILL_REWRITING,
  QuillCatalog,
  QuillHotkey,
  QuillPicker,
  hotkeySlotAllowed,
  quillSettingsFromRust,
  quillLogLine,
  quillNotice,
  toQuillCommandError,
  type QuillMode,
} from "./quill-surfaces";

const contractsDir = resolve(import.meta.dirname, "../src-tauri/contracts/quill");

const MODES: QuillMode[] = [
  { id: "system-gpt", name: "System prompt for GPT", description: "GPT system prompt.", form: "system", targetModel: "gpt", default: false },
  { id: "task-claude", name: "Task prompt for Claude", description: "Claude task prompt.", form: "task", targetModel: "claude", default: true },
  { id: "dictation-cleanup", name: "Dictation cleanup", description: "Cleans up a dictation.", form: "dictation", targetModel: "any", default: false },
];

const REWRITE_RESULT = {
  rewriteId: "00000000-0000-4000-8000-000000000001",
  mode: "task-claude",
  model: "anthropic/claude-haiku-5.5",
  latencyMs: 812,
  endToEndMs: 1500,
  anchored: true,
  clipboardReplaced: false,
};

type Call = { cmd: string; args?: Record<string, unknown> };

function harness(options: {
  storedDefault?: string;
  rewrite?: (args?: Record<string, unknown>) => Promise<unknown>;
} = {}) {
  const calls: Call[] = [];
  const notices: { text: string; role: string }[] = [];
  const rewritten: { rewriteId: string; name: string; delivered: string }[] = [];
  const logs: string[] = [];
  const invoke = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "quill_list_modes") return { modes: MODES };
    if (cmd === "quill_rewrite_selection") {
      return options.rewrite ? options.rewrite(args) : REWRITE_RESULT;
    }
    return null;
  });
  const catalog = new QuillCatalog(invoke);
  const picker = new QuillPicker({ announce: () => {} });
  const hotkey = new QuillHotkey({
    invoke,
    catalog,
    picker,
    storedDefault: () => options.storedDefault ?? "task-claude",
    notify: (text, role) => notices.push({ text, role }),
    onRewritten: (rewrite) => rewritten.push(rewrite),
    log: (line) => logs.push(line),
  });
  const rewrites = () => calls.filter((call) => call.cmd === "quill_rewrite_selection");
  return { calls, notices, rewritten, logs, invoke, catalog, picker, hotkey, rewrites };
}

function fixtureErrors() {
  const files = readdirSync(resolve(contractsDir, "fixtures")).filter((name) => name.endsWith(".json"));
  const byCode = new Map<string, { code: string; message: string }>();
  for (const file of files) {
    const fixture = JSON.parse(readFileSync(resolve(contractsDir, "fixtures", file), "utf8"));
    const error = fixture.response?.error;
    if (fixture.response?.ok === false && error && !byCode.has(error.code)) byCode.set(error.code, error);
  }
  return byCode;
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-10-23T10:00:00.000Z"));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("quill-press-window", () => {
  it("dispatches one press once at 600 ms with the effective default and the first press's Date.now()", async () => {
    const h = harness();
    const pressedAt = Date.now();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(599);
    expect(h.rewrites()).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(h.rewrites()).toEqual([
      { cmd: "quill_rewrite_selection", args: { mode: "task-claude", pressedAtMs: pressedAt } },
    ]);
    await vi.advanceTimersByTimeAsync(5000);
    expect(h.rewrites()).toHaveLength(1);
  });

  it("opens the picker on a second press at 599 ms and dispatches nothing", async () => {
    const h = harness();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(599);
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(2000);
    expect(h.rewrites()).toHaveLength(0);
    expect(h.calls.map((call) => call.cmd)).toContain("quill_picker_open");
    expect(h.picker.isOpen).toBe(true);
  });

  it("shows the in-flight notice and dispatches nothing for a press during a rewrite", async () => {
    let finish!: (value: unknown) => void;
    const h = harness({ rewrite: () => new Promise((resolveRewrite) => { finish = resolveRewrite; }) });
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.rewrites()).toHaveLength(1);
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(2000);
    expect(h.rewrites()).toHaveLength(1);
    expect(h.notices.map((notice) => notice.text)).toEqual([QUILL_STILL_REWRITING]);
    expect(QUILL_STILL_REWRITING).toBe("Still rewriting the last selection.");
    expect(h.picker.isOpen).toBe(false);
    finish(REWRITE_RESULT);
  });

  it("starts a new window for a press after completion", async () => {
    const h = harness();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    const secondPressAt = Date.now();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(599);
    expect(h.rewrites()).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(h.rewrites()).toHaveLength(2);
    expect(h.rewrites()[1].args).toEqual({ mode: "task-claude", pressedAtMs: secondPressAt });
    expect(h.notices).toEqual([]);
  });

  it("falls back to the catalog default for a stale quillDefaultMode", async () => {
    const h = harness({ storedDefault: "mode-that-was-removed" });
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.rewrites()[0].args?.mode).toBe("task-claude");
  });

  it("uses a listed quillDefaultMode, and the session choice before it", async () => {
    const h = harness({ storedDefault: "system-gpt" });
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.rewrites()[0].args?.mode).toBe("system-gpt");
    h.hotkey.sessionMode = "dictation-cleanup";
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.rewrites()[1].args?.mode).toBe("dictation-cleanup");
  });

  it("ignores a press while the picker is open", async () => {
    const h = harness();
    h.hotkey.press();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(0);
    expect(h.picker.isOpen).toBe(true);
    const before = h.calls.length;
    h.hotkey.press();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(1200);
    expect(h.calls).toHaveLength(before);
    expect(h.picker.isOpen).toBe(true);
    expect(h.notices).toEqual([]);
  });

  it("releases the picker keys when the catalog lists no modes", async () => {
    const calls: string[] = [];
    const invoke = vi.fn(async (cmd: string) => {
      calls.push(cmd);
      return cmd === "quill_list_modes" ? { modes: [] } : null;
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
    hotkey.press();
    await vi.advanceTimersByTimeAsync(0);
    expect(picker.isOpen).toBe(false);
    expect(calls.filter((cmd) => cmd === "quill_picker_open")).toHaveLength(1);
    expect(calls.filter((cmd) => cmd === "quill_picker_close")).toHaveLength(1);
    expect(calls.lastIndexOf("quill_picker_close")).toBeGreaterThan(calls.indexOf("quill_picker_open"));
  });

  it("releases the picker keys when opening fails after they were registered", async () => {
    const calls: string[] = [];
    const notices: string[] = [];
    const invoke = vi.fn(async (cmd: string) => {
      calls.push(cmd);
      return cmd === "quill_list_modes" ? { modes: MODES } : null;
    });
    class ThrowingPicker extends QuillPicker {
      open() {
        throw new Error("render failed");
      }
    }
    const picker = new ThrowingPicker({ announce: () => {} });
    const hotkey = new QuillHotkey({
      invoke,
      catalog: new QuillCatalog(invoke),
      picker,
      storedDefault: () => "task-claude",
      notify: (text) => notices.push(text),
      onRewritten: () => {},
    });
    hotkey.press();
    hotkey.press();
    await vi.advanceTimersByTimeAsync(0);
    expect(picker.isOpen).toBe(false);
    expect(calls.filter((cmd) => cmd === "quill_picker_close")).toHaveLength(1);
    expect(calls.lastIndexOf("quill_picker_close")).toBeGreaterThan(calls.indexOf("quill_picker_open"));
    expect(notices).toEqual([QUILL_NOTICES.transport]);
  });

  it("logs a failed picker-key release without the picker-open alert", async () => {
    const notices: string[] = [];
    const logs: string[] = [];
    const invoke = vi.fn(async (cmd: string) => {
      if (cmd === "quill_list_modes") return { modes: [] };
      if (cmd === "quill_picker_close") throw { code: "transport", message: "Worker process exited unexpectedly" };
      return null;
    });
    const picker = new QuillPicker({ announce: () => {} });
    const hotkey = new QuillHotkey({
      invoke,
      catalog: new QuillCatalog(invoke),
      picker,
      storedDefault: () => "task-claude",
      notify: (text) => notices.push(text),
      onRewritten: () => {},
      log: (line) => logs.push(line),
    });
    hotkey.press();
    hotkey.press();
    await vi.advanceTimersByTimeAsync(0);
    expect(notices).toEqual([]);
    expect(logs).toEqual([quillLogLine("picker_close_failed", "transport")]);
  });

  it("releases the picker keys once when the picker fails to render on open", async () => {
    const calls: string[] = [];
    const invoke = vi.fn(async (cmd: string) => {
      calls.push(cmd);
      return cmd === "quill_list_modes" ? { modes: MODES } : null;
    });
    const picker = new QuillPicker({
      announce: () => {},
      render: () => {
        throw new Error("render failed");
      },
    });
    const hotkey = new QuillHotkey({
      invoke,
      catalog: new QuillCatalog(invoke),
      picker,
      storedDefault: () => "task-claude",
      notify: () => {},
      onRewritten: () => {},
    });
    hotkey.press();
    hotkey.press();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(picker.isOpen).toBe(false);
    expect(calls.filter((cmd) => cmd === "quill_picker_close")).toHaveLength(1);
    expect(calls.lastIndexOf("quill_picker_close")).toBeGreaterThan(calls.indexOf("quill_picker_open"));
  });

  it("logs a picker render failure on a key press and still runs the chosen rewrite", async () => {
    const calls: string[] = [];
    const logs: string[] = [];
    const alerts: string[] = [];
    const invoke = vi.fn(async (cmd: string) => {
      calls.push(cmd);
      if (cmd === "quill_list_modes") return { modes: MODES };
      if (cmd === "quill_rewrite_selection") return REWRITE_RESULT;
      return null;
    });
    let fail = false;
    const picker = new QuillPicker({
      announce: () => {},
      render: () => {
        if (fail) throw new Error("render failed");
      },
    });
    const hotkey = new QuillHotkey({
      invoke,
      catalog: new QuillCatalog(invoke),
      picker,
      storedDefault: () => "task-claude",
      notify: (text) => alerts.push(text),
      onRewritten: () => {},
      log: (line) => logs.push(line),
    });
    hotkey.press();
    hotkey.press();
    await vi.advanceTimersByTimeAsync(0);
    expect(picker.isOpen).toBe(true);
    fail = true;
    expect(() => hotkey.pickerKey("enter")).not.toThrow();
    await vi.advanceTimersByTimeAsync(0);
    expect(picker.isOpen).toBe(false);
    expect(logs).toEqual([quillLogLine("picker_key_failed", "transport")]);
    expect(alerts).toEqual([]);
    expect(calls.filter((cmd) => cmd === "quill_picker_close")).toHaveLength(1);
    expect(calls.filter((cmd) => cmd === "quill_rewrite_selection")).toHaveLength(1);
  });

  it("logs a picker render failure on the idle close and on a widget click, with no alert", async () => {
    for (const via of ["timer", "click"] as const) {
      const calls: string[] = [];
      const logs: string[] = [];
      const alerts: string[] = [];
      const invoke = vi.fn(async (cmd: string) => {
        calls.push(cmd);
        return cmd === "quill_list_modes" ? { modes: MODES } : null;
      });
      let fail = false;
      const picker = new QuillPicker({
        announce: () => {},
        render: () => {
          if (fail) throw new Error("render failed");
        },
      });
      const hotkey = new QuillHotkey({
        invoke,
        catalog: new QuillCatalog(invoke),
        picker,
        storedDefault: () => "task-claude",
        notify: (text) => alerts.push(text),
        onRewritten: () => {},
        log: (line) => logs.push(line),
      });
      hotkey.press();
      hotkey.press();
      await vi.advanceTimersByTimeAsync(0);
      expect(picker.isOpen).toBe(true);
      fail = true;
      if (via === "timer") await vi.advanceTimersByTimeAsync(10_000);
      else expect(() => picker.click()).not.toThrow();
      await vi.advanceTimersByTimeAsync(0);
      expect(picker.isOpen).toBe(false);
      expect(logs).toEqual([quillLogLine(via === "timer" ? "picker_timer_failed" : "picker_click_failed", "transport")]);
      expect(alerts).toEqual([]);
      expect(calls.filter((cmd) => cmd === "quill_picker_close")).toHaveLength(1);
    }
  });

  it("opens no revert slot for a hotkey rewrite while the hook is not ready", async () => {
    for (const ready of [true, false]) {
      const calls: string[] = [];
      const rewritten: string[] = [];
      const invoke = vi.fn(async (cmd: string) => {
        calls.push(cmd);
        if (cmd === "quill_list_modes") return { modes: MODES };
        return cmd === "quill_rewrite_selection" ? REWRITE_RESULT : null;
      });
      const hotkey = new QuillHotkey({
        invoke,
        catalog: new QuillCatalog(invoke),
        picker: new QuillPicker({ announce: () => {} }),
        storedDefault: () => "task-claude",
        notify: () => {},
        onRewritten: (rewrite) => rewritten.push(rewrite.rewriteId),
        hookReady: () => ready,
      });
      hotkey.press();
      await vi.advanceTimersByTimeAsync(600);
      expect(calls.filter((cmd) => cmd === "quill_rewrite_selection")).toHaveLength(1);
      expect(rewritten).toEqual(ready ? [REWRITE_RESULT.rewriteId] : []);
    }
  });

  it("wires the hook state into the hotkey and closes the slot when the hook stops being ready", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const wiring = main.slice(main.indexOf("const quillHotkey = new QuillHotkey({"), main.indexOf("});", main.indexOf("const quillHotkey = new QuillHotkey({")));
    expect(wiring).toContain("hookReady: () => hotkeySlotAllowed(quillSettings),");
    const sync = main.slice(main.indexOf("function syncQuillSettingsUi("), main.indexOf("\n}\n", main.indexOf("function syncQuillSettingsUi(")));
    const notReady = sync.indexOf("if (!quillHookReady(settings)) {");
    expect(notReady).toBeGreaterThan(0);
    const branch = sync.slice(notReady, sync.indexOf("} else if", notReady));
    expect(branch).toContain("quillSlot.close();");
  });

  it("defers the hotkey slot to Rust until settings come from Rust, then follows the hook state", () => {
    expect(hotkeySlotAllowed(null)).toBe(true);
    expect(hotkeySlotAllowed({})).toBe(true);
    expect(hotkeySlotAllowed({ quillEnabled: false })).toBe(false);
    expect(hotkeySlotAllowed({ quillEnabled: true, quillCommandError: null })).toBe(true);
    expect(hotkeySlotAllowed({ quillEnabled: true, quillCommandError: "not executable" })).toBe(false);
  });

  it("decides the hotkey slot on settings read after the rewrite, so a save that landed mid-rewrite keeps it closed", async () => {
    const cases: { read: () => Promise<object>; last: boolean; opens: boolean }[] = [
      { read: async () => ({ quillEnabled: false }), last: true, opens: false },
      { read: async () => ({ quillEnabled: true, quillCommandError: "not executable" }), last: true, opens: false },
      { read: async () => ({ quillEnabled: true, quillCommandError: null }), last: false, opens: true },
      { read: async () => { throw new Error("ipc"); }, last: true, opens: true },
      { read: async () => { throw new Error("ipc"); }, last: false, opens: false },
    ];
    for (const { read, last, opens } of cases) {
      const calls: string[] = [];
      const rewritten: string[] = [];
      const invoke = vi.fn(async (cmd: string) => {
        calls.push(cmd);
        if (cmd === "quill_list_modes") return { modes: MODES };
        return cmd === "quill_rewrite_selection" ? REWRITE_RESULT : null;
      });
      const hotkey = new QuillHotkey({
        invoke,
        catalog: new QuillCatalog(invoke),
        picker: new QuillPicker({ announce: () => {} }),
        storedDefault: () => "task-claude",
        notify: () => {},
        onRewritten: (rewrite) => rewritten.push(rewrite.rewriteId),
        hookReady: () => last,
        readSettings: () => {
          calls.push("readSettings");
          return read();
        },
      });
      hotkey.press();
      await vi.advanceTimersByTimeAsync(600);
      expect(calls.indexOf("readSettings")).toBeGreaterThan(calls.indexOf("quill_rewrite_selection"));
      expect(rewritten).toEqual(opens ? [REWRITE_RESULT.rewriteId] : []);
    }
  });

  it("reads settings from Rust for the hotkey slot and lets only Rust's settings close an open slot", () => {
    expect(quillSettingsFromRust({})).toBe(false);
    expect(quillSettingsFromRust({ quillEnabled: false })).toBe(true);
    expect(quillSettingsFromRust({ quillEnabled: true })).toBe(true);
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const wiring = main.slice(main.indexOf("const quillHotkey = new QuillHotkey({"), main.indexOf("});", main.indexOf("const quillHotkey = new QuillHotkey({")));
    expect(wiring).toContain('readSettings: () => invoke<AppSettings>("app_get_settings"),');
    const sync = main.slice(main.indexOf("function syncQuillSettingsUi("), main.indexOf("\n}\n", main.indexOf("function syncQuillSettingsUi(")));
    const notReady = sync.indexOf("if (!quillHookReady(settings)) {");
    const branch = sync.slice(notReady, sync.indexOf("} else if", notReady));
    expect(branch).toContain("if (quillSettingsFromRust(settings)) quillSlot.close();");
  });
});

describe("quill-error-contract", () => {
  it("fails, not skips, when the vendored contract directory is absent", () => {
    expect(existsSync(contractsDir)).toBe(true);
    expect(existsSync(resolve(contractsDir, "quill-protocol.v1.json"))).toBe(true);
    expect(readdirSync(resolve(contractsDir, "fixtures")).length).toBeGreaterThan(0);
  });

  it("declares the schema's eight engine codes plus the nine client codes, seventeen in all", () => {
    const schema = JSON.parse(readFileSync(resolve(contractsDir, "quill-protocol.v1.json"), "utf8"));
    const engine: string[] = schema.$defs.error.properties.code.enum;
    expect(engine).toHaveLength(8);
    const client = [
      "busy",
      "transport",
      "no_selection",
      "secure_field",
      "too_long",
      "accessibility_permission",
      "modifiers_held",
      "target_changed",
      "disabled",
    ];
    expect([...QUILL_ERROR_CODES]).toEqual([...engine, ...client]);
    expect(new Set(QUILL_ERROR_CODES).size).toBe(17);
  });

  it("has one notice per code, chosen by code and never by message", () => {
    expect(Object.keys(QUILL_NOTICES).sort()).toEqual([...QUILL_ERROR_CODES].sort());
    for (const code of QUILL_ERROR_CODES) {
      expect(QUILL_NOTICES[code].length).toBeGreaterThan(0);
      expect(quillNotice({ code, message: "first message" })).toBe(QUILL_NOTICES[code]);
      expect(quillNotice({ code, message: "a completely different message" })).toBe(QUILL_NOTICES[code]);
    }
    expect(QUILL_NOTICES.no_selection).toBe("Select some text first.");
    expect(QUILL_NOTICES.secure_field).toBe("Can't rewrite in a password field.");
    expect(QUILL_NOTICES.too_long).toBe("Selection is over 8,000 characters. Select less.");
    expect(QUILL_NOTICES.busy).toBe("Finishing the dictation rewrite first. Try again in a moment.");
    expect(QUILL_NOTICES.timeout).toBe("Rewrite timed out. The text was not changed.");
    expect(QUILL_NOTICES.no_key).toBe("No OpenRouter key found. Add one in Keychain as quill-openrouter.");
    expect(QUILL_NOTICES.target_changed).toBe("Copied the rewrite. Press Command+V to paste it.");
    expect(QUILL_NOTICES.transport).toBe("Rewrite service unavailable. Check the quill command in Settings.");
    expect(QUILL_NOTICES.disabled).toBe("Rewrite service unavailable. Check the quill command in Settings.");
    expect(QUILL_NOTICES.accessibility_permission).toBe("Enable Accessibility for automatic insertion.");
    expect(QUILL_NOTICES.modifiers_held).toBe("A shortcut key is still held.");
  });

  it("shows each engine code's notice for a rejection taken from the vendored fixtures by error.code", async () => {
    const errors = fixtureErrors();
    const schema = JSON.parse(readFileSync(resolve(contractsDir, "quill-protocol.v1.json"), "utf8"));
    for (const code of schema.$defs.error.properties.code.enum as string[]) {
      const error = errors.get(code);
      expect(error, `fixture for ${code}`).toBeDefined();
      const h = harness({ rewrite: () => Promise.reject(error) });
      h.hotkey.press();
      await vi.advanceTimersByTimeAsync(600);
      expect(h.notices).toEqual([{ text: QUILL_NOTICES[code as keyof typeof QUILL_NOTICES], role: "alert" }]);
      expect(h.logs).toEqual([quillLogLine("rewrite_failed", code)]);
      expect(h.rewritten).toEqual([]);
    }
  });

  it("handles a rejection that is not {code, message} as transport", async () => {
    for (const rejection of ["Worker crashed", new Error("boom"), { code: "made_up", message: "x" }, { code: "busy" }, null, undefined]) {
      expect(toQuillCommandError(rejection).code).toBe("transport");
      const h = harness({ rewrite: () => Promise.reject(rejection) });
      h.hotkey.press();
      await vi.advanceTimersByTimeAsync(600);
      expect(h.notices).toEqual([{ text: QUILL_NOTICES.transport, role: "alert" }]);
    }
    expect(toQuillCommandError({ code: "busy", message: "Another quill request is already running." })).toEqual({
      code: "busy",
      message: "Another quill request is already running.",
    });
  });

  it("selects the transport notice for a transport rejection with the worker's exit text", async () => {
    const h = harness({ rewrite: () => Promise.reject({ code: "transport", message: "Worker process exited unexpectedly" }) });
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.notices).toEqual([{ text: QUILL_NOTICES.transport, role: "alert" }]);
    expect(h.notices[0].text).not.toContain("Worker");
  });

  it("formats log lines as exactly quill <event> code=<code>", () => {
    expect(quillLogLine("rewrite_failed", "timeout")).toBe("quill rewrite_failed code=timeout");
    expect(quillLogLine("picker_open_failed", "transport")).toBe("quill picker_open_failed code=transport");
  });

  it("keeps quill text out of the debug log", () => {
    const surfaces = readFileSync(resolve(import.meta.dirname, "quill-surfaces.ts"), "utf8");
    expect(surfaces).not.toContain("debug_log");
    expect(surfaces).not.toContain("logDebug");
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const quillLogCalls = main
      .split("\n")
      .filter((line) => line.includes("logDebug(") && /quill/i.test(line));
    for (const line of quillLogCalls) {
      expect(line).toContain("logDebug(quillLogLine(");
    }
  });

  it("reads a success as hook-core's camelCase result", async () => {
    const h = harness();
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.rewritten).toEqual([
      { rewriteId: REWRITE_RESULT.rewriteId, name: "Task prompt for Claude", delivered: "inserted" },
    ]);
    expect(h.notices).toEqual([]);
  });

  it("tells the user a non-text clipboard was replaced", async () => {
    const h = harness({ rewrite: async () => ({ ...REWRITE_RESULT, clipboardReplaced: true }) });
    h.hotkey.press();
    await vi.advanceTimersByTimeAsync(600);
    expect(h.notices).toEqual([
      { text: "Your clipboard held an image or file; it was replaced by the selection.", role: "status" },
    ]);
  });
});
