import { existsSync, readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  QUILL_ERROR_CODES,
  QUILL_NOTICES,
  quillLogLine,
  quillNotice,
  toQuillCommandError,
} from "./quill-surfaces";

const contractsDir = resolve(import.meta.dirname, "../src-tauri/contracts/quill");

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

  it("shows each engine code's notice for a rejection taken from the vendored fixtures by error.code", () => {
    const errors = fixtureErrors();
    const schema = JSON.parse(readFileSync(resolve(contractsDir, "quill-protocol.v1.json"), "utf8"));
    for (const code of schema.$defs.error.properties.code.enum as string[]) {
      const error = errors.get(code);
      expect(error, `fixture for ${code}`).toBeDefined();
      expect(quillNotice(error)).toBe(QUILL_NOTICES[code as keyof typeof QUILL_NOTICES]);
      expect(quillLogLine("rewrite_failed", toQuillCommandError(error).code)).toBe(quillLogLine("rewrite_failed", code));
    }
  });

  it("handles a rejection that is not {code, message} as transport", () => {
    for (const rejection of ["Worker crashed", new Error("boom"), { code: "made_up", message: "x" }, { code: "busy" }, null, undefined]) {
      expect(toQuillCommandError(rejection).code).toBe("transport");
      expect(quillNotice(rejection)).toBe(QUILL_NOTICES.transport);
    }
    expect(toQuillCommandError({ code: "busy", message: "Another quill request is already running." })).toEqual({
      code: "busy",
      message: "Another quill request is already running.",
    });
  });

  it("selects the transport notice for a transport rejection with the worker's exit text", () => {
    const notice = quillNotice({ code: "transport", message: "Worker process exited unexpectedly" });
    expect(notice).toBe(QUILL_NOTICES.transport);
    expect(notice).not.toContain("Worker");
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
});
