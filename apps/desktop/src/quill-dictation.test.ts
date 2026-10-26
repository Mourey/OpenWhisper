import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  QUILL_DICTATION_GUARD_MS,
  QUILL_ERROR_CODES,
  QUILL_LIVE_REWRITING,
  dictationDelivered,
  dictationFallbackNotice,
  quillNoticeDurationMs,
  recordingStartBlocked,
  rewriteDictation,
  settleDictationRewrite,
  shouldRewriteDictation,
} from "./quill-surfaces";

type Call = { cmd: string; args?: Record<string, unknown> };

function fakeInvoke(rewrite: () => Promise<unknown>) {
  const calls: Call[] = [];
  const invoke = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "quill_rewrite_text") return rewrite();
    return null;
  });
  return { calls, invoke };
}

const TEXT_RESULT = { rewriteId: "r-7", text: "Clean dictation.", mode: "dictation-cleanup", model: "m", latencyMs: 400 };

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("quill-dictation", () => {
  it("calls quill_rewrite_text with the exact argument object", async () => {
    const { calls, invoke } = fakeInvoke(async () => TEXT_RESULT);
    await rewriteDictation({ invoke, text: "um so the raw transcript", notify: () => {}, announce: () => {} });
    expect(calls).toEqual([
      {
        cmd: "quill_rewrite_text",
        args: { text: "um so the raw transcript", mode: "dictation-cleanup", surface: "pill", sourceApp: null },
      },
    ]);
  });

  it("rewrites only when cleanup was effective at session start, not copy-only and not streaming", () => {
    expect(shouldRewriteDictation({ cleanupEffective: true, copyOnly: false, streaming: false })).toBe(true);
    expect(shouldRewriteDictation({ cleanupEffective: false, copyOnly: false, streaming: false })).toBe(false);
  });

  it("skips a copy-only delivery", () => {
    expect(shouldRewriteDictation({ cleanupEffective: true, copyOnly: true, streaming: false })).toBe(false);
  });

  it("skips the streaming model", () => {
    expect(shouldRewriteDictation({ cleanupEffective: true, copyOnly: false, streaming: true })).toBe(false);
    expect(shouldRewriteDictation({ cleanupEffective: true, copyOnly: true, streaming: true })).toBe(false);
  });

  it("returns the rewritten text and rewrite id on success, with no notice", async () => {
    const notices: string[] = [];
    const { invoke } = fakeInvoke(async () => TEXT_RESULT);
    const result = await rewriteDictation({ invoke, text: "raw", notify: (text) => notices.push(text), announce: () => {} });
    expect(result).toEqual({ rewriteId: "r-7", text: "Clean dictation.", mode: "dictation-cleanup" });
    expect(notices).toEqual([]);
  });

  it("falls back with each error code's notice, as an alert for 6 s", async () => {
    const causes: Record<string, string> = { timeout: "Rewrite timed out", no_key: "No OpenRouter key" };
    for (const code of QUILL_ERROR_CODES) {
      const notices: { text: string; role: string }[] = [];
      const { invoke } = fakeInvoke(() => Promise.reject({ code, message: `message for ${code}` }));
      const result = await rewriteDictation({ invoke, text: "raw", notify: (text, role) => notices.push({ text, role }), announce: () => {} });
      expect(result).toBeNull();
      const expected = code === "too_long"
        ? "Transcript is over 8,000 characters. Inserted it without rewriting."
        : `Rewrite failed (${causes[code] ?? "Rewrite service unavailable"}). Inserted the raw transcript.`;
      expect(notices).toEqual([{ text: expected, role: "alert" }]);
      expect(dictationFallbackNotice(code)).toBe(expected);
    }
    expect(quillNoticeDurationMs("alert")).toBe(6000);
    expect(quillNoticeDurationMs("status")).toBe(3000);
  });

  it("falls back to the raw transcript when a rejection is not {code, message}", async () => {
    const notices: string[] = [];
    const { invoke } = fakeInvoke(() => Promise.reject("not an error object"));
    expect(await rewriteDictation({ invoke, text: "raw", notify: (text) => notices.push(text), announce: () => {} })).toBeNull();
    expect(notices).toEqual(["Rewrite failed (Rewrite service unavailable). Inserted the raw transcript."]);
  });

  it("gives up at the 8,000 ms guard with the timeout fallback", async () => {
    const notices: string[] = [];
    const { invoke } = fakeInvoke(() => new Promise(() => {}));
    let settled: unknown = "pending";
    void rewriteDictation({ invoke, text: "raw", notify: (text) => notices.push(text), announce: () => {} })
      .then((result) => { settled = result; });
    expect(QUILL_DICTATION_GUARD_MS).toBe(8000);
    await vi.advanceTimersByTimeAsync(7999);
    expect(settled).toBe("pending");
    expect(notices).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    expect(settled).toBeNull();
    expect(notices).toEqual(["Rewrite failed (Rewrite timed out). Inserted the raw transcript."]);
  });

  it("announces Rewriting transcript in the live region", async () => {
    const announced: string[] = [];
    const { invoke } = fakeInvoke(async () => TEXT_RESULT);
    await rewriteDictation({ invoke, text: "raw", notify: () => {}, announce: (text) => announced.push(text) });
    expect(QUILL_LIVE_REWRITING).toBe("Rewriting transcript");
    expect(announced).toEqual(["Rewriting transcript"]);
  });

  it("asks for a restore after an inserted delivery and reports the outcome", async () => {
    const { calls, invoke } = fakeInvoke(async () => TEXT_RESULT);
    await settleDictationRewrite({ invoke, rewriteId: "r-7", delivered: "inserted", pasted: "Clean dictation." });
    expect(calls).toEqual([
      { cmd: "quill_clipboard_release", args: { restore: true, pasted: "Clean dictation." } },
      { cmd: "quill_report_outcome", args: { rewriteId: "r-7", delivered: "inserted" } },
    ]);
  });

  it("asks for no restore after a copy fallback and reports copied", async () => {
    const { calls, invoke } = fakeInvoke(async () => TEXT_RESULT);
    await settleDictationRewrite({ invoke, rewriteId: "r-7", delivered: "copied", pasted: "Clean dictation." });
    expect(calls).toEqual([
      { cmd: "quill_clipboard_release", args: { restore: false, pasted: "Clean dictation." } },
      { cmd: "quill_report_outcome", args: { rewriteId: "r-7", delivered: "copied" } },
    ]);
  });

  it("maps the delivery to inserted, copied or failed", () => {
    expect(dictationDelivered({ inserted: true })).toBe("inserted");
    expect(dictationDelivered({ inserted: false })).toBe("copied");
    expect(dictationDelivered(null)).toBe("failed");
  });

  it("refuses a recording start while rewriting", () => {
    expect(recordingStartBlocked("rewriting")).toBe(true);
    for (const state of ["idle", "recording", "transcribing", "error"]) {
      expect(recordingStartBlocked(state)).toBe(false);
    }
  });

  it("adds rewriting to WidgetState in main.ts", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const states = main.match(/type WidgetState = ([^;]+);/)?.[1];
    expect(states).toBeDefined();
    expect(states!.split("|").map((part) => part.trim())).toEqual(['"idle"', '"recording"', '"transcribing"', '"error"', '"rewriting"']);
  });

  it("returns from toggleRecording and handlePressToTalkStart without starting while rewriting", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const toggle = main.match(/async function toggleRecording\(\) \{([\s\S]*?)\n\}/)?.[1];
    const pressStart = main.match(/async function handlePressToTalkStart\(\) \{([\s\S]*?)\n\}/)?.[1];
    for (const body of [toggle, pressStart]) {
      expect(body).toBeDefined();
      const guard = body!.indexOf("if (recordingStartBlocked(state)) return;");
      expect(guard).toBeGreaterThan(-1);
      expect(guard).toBeLessThan(body!.indexOf("startRecording()"));
    }
    expect(pressStart!.indexOf("if (recordingStartBlocked(state)) return;")).toBeLessThan(pressStart!.indexOf("pressToTalkHeld = true"));
  });

  it("asks for no revert hotkey when the rewrite lands after the 8,000 ms guard", async () => {
    let land: (value: unknown) => void = () => {};
    const { calls, invoke } = fakeInvoke(() => new Promise((resolve) => { land = resolve; }));
    let settled: unknown = "pending";
    void rewriteDictation({ invoke, text: "raw", notify: () => {}, announce: () => {} })
      .then((result) => { settled = result; });
    await vi.advanceTimersByTimeAsync(QUILL_DICTATION_GUARD_MS);
    expect(settled).toBeNull();
    land(TEXT_RESULT);
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toBeNull();
    expect(calls.map((call) => call.cmd)).toEqual(["quill_rewrite_text"]);
  });

  it("ends the clipboard hold with a failed outcome when the delivery is abandoned", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const start = main.indexOf("async function pollOnce(");
    expect(start).toBeGreaterThan(0);
    const poll = main.slice(start, main.indexOf("\n}\n", start));
    const abandoned = poll.indexOf("if (!delivery || signal?.aborted || currentSessionId !== sessionId) return;");
    const settle = poll.indexOf("await settleQuillDictation(delivery);");
    const ending = poll.indexOf("if (quillDictationPending) void settleQuillDictation(null);");
    expect(abandoned).toBeGreaterThan(0);
    expect(settle).toBeGreaterThan(abandoned);
    expect(ending).toBeGreaterThan(settle);
    expect(poll.lastIndexOf("} finally {", ending)).toBeGreaterThan(settle);
    expect(dictationDelivered(null)).toBe("failed");
  });
});
