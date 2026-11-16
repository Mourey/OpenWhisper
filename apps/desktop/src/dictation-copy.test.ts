import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { dictationCopyText } from "./dictation-copy";

const RAW = "um so the plan";
const REWRITTEN = "So the plan";

const ABANDONED = "if (!delivery || signal?.aborted || currentSessionId !== sessionId) return;";

/** The body of `pollOnce` in main.ts. */
function pollOnce() {
  const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
  const start = main.indexOf("async function pollOnce(");
  expect(start).toBeGreaterThan(0);
  return main.slice(start, main.indexOf("\n}\n", start));
}

/** The one statement in `pollOnce` that copies the dictation. */
function copyStatement(poll: string) {
  const decided = poll.indexOf("dictationCopyText(");
  expect(decided).toBeGreaterThan(0);
  expect(poll.indexOf("dictationCopyText(", decided + 1)).toBe(-1);
  const start = poll.indexOf('invoke<void>("copy_to_clipboard"', decided);
  expect(start).toBeGreaterThan(decided);
  const end = poll.indexOf("\n", start);
  return { decided, start, end, line: poll.slice(start, end) };
}

describe("dictation-copy", () => {
  it("returns null for an inserted delivery while the setting is off", () => {
    expect(dictationCopyText({ enabled: false, delivery: { inserted: true }, text: REWRITTEN })).toBeNull();
  });

  it("returns null for a copied delivery while the setting is off", () => {
    expect(dictationCopyText({ enabled: false, delivery: { inserted: false }, text: REWRITTEN })).toBeNull();
  });

  it("returns null for an abandoned delivery while the setting is off", () => {
    expect(dictationCopyText({ enabled: false, delivery: null, text: REWRITTEN })).toBeNull();
  });

  it("returns null for an abandoned delivery while the setting is on", () => {
    expect(dictationCopyText({ enabled: true, delivery: null, text: REWRITTEN })).toBeNull();
  });

  it("returns null after a copy fallback while the setting is on, since the text was already copied", () => {
    expect(dictationCopyText({ enabled: true, delivery: { inserted: false }, text: REWRITTEN })).toBeNull();
  });

  it("returns the given text for an inserted delivery while the setting is on", () => {
    expect(dictationCopyText({ enabled: true, delivery: { inserted: true }, text: REWRITTEN })).toBe(REWRITTEN);
    expect(dictationCopyText({ enabled: true, delivery: { inserted: true }, text: RAW })).toBe(RAW);
  });

  it("copies finalTranscript, which holds the rewritten text once cleanup succeeded", () => {
    const poll = pollOnce();
    const rewritten = poll.indexOf("finalTranscript = quillRewrite.text;");
    expect(rewritten).toBeGreaterThan(0);
    const call = poll.match(/dictationCopyText\(\{([^}]*)\}\)/);
    expect(call).not.toBeNull();
    expect(call!.index!).toBeGreaterThan(rewritten);
    const args = call![1].split(",").map((part) => part.trim());
    expect(args).toContain("text: finalTranscript");
    expect(args).toContain("delivery");
    expect(args).toContain("enabled: copyDictationsAtDelivery");
    // Nothing assigns finalTranscript between the rewrite and the copy.
    expect(poll.slice(rewritten + 1, call!.index!)).not.toMatch(/\bfinalTranscript\s*=[^=]/);
    // The cleanup fixture: the text handed over is the one returned, never the raw one.
    expect(dictationCopyText({ enabled: true, delivery: { inserted: true }, text: REWRITTEN })).not.toBe(RAW);
  });

  it("reads the setting once per session, inside pollOnce and before deliverTranscript(", () => {
    const poll = pollOnce();
    const done = poll.indexOf("if (resp.done) {");
    expect(done).toBeGreaterThan(0);
    const assigned = poll.indexOf("copyDictationsAtDelivery = quillSettings?.copyDictationsToClipboard ?? false;");
    expect(assigned).toBeGreaterThan(done);
    expect(assigned).toBeLessThan(poll.indexOf("deliverTranscript("));
    expect(poll.slice(done + "if (resp.done) {".length, assigned).trim()).toBe("");
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    expect(main.match(/\bcopyDictationsAtDelivery\s*=[^=]/g)).toHaveLength(2);
    expect(main).toContain("let copyDictationsAtDelivery = false;");
  });

  it("copies after the abandoned-delivery return and before finishCurrentRecording(, in the same block", () => {
    const poll = pollOnce();
    const delivered = poll.indexOf("const delivery = await deliverTranscript(");
    const abandoned = poll.indexOf(ABANDONED, delivered);
    expect(delivered).toBeGreaterThan(0);
    expect(abandoned).toBeGreaterThan(delivered);
    const copy = copyStatement(poll);
    const finish = poll.indexOf("await finishCurrentRecording(", abandoned);
    const settle = poll.indexOf("await settleQuillDictation(delivery);");
    expect(copy.decided).toBeGreaterThan(abandoned);
    expect(copy.end).toBeLessThan(finish);
    expect(finish).toBeLessThan(settle);
    // Nothing but the copy sits between the return and the finish, so no brace closes the block.
    const between = poll.slice(abandoned + ABANDONED.length, finish);
    expect(between).not.toContain("}\n");
    expect(between.trim().split("\n")).toHaveLength(2);
    expect(copy.line).toContain("{ text: copyText }");
    expect(poll.slice(copy.decided, copy.start)).toContain("if (copyText !== null) await ");
  });

  it("catches a failed copy with exactly a debug line that carries no text", () => {
    const copy = copyStatement(pollOnce());
    const caught = copy.line.indexOf(".catch(");
    expect(caught).toBeGreaterThan(0);
    expect(copy.line.slice(caught)).toBe('.catch(() => logDebug("dictation copy failed"));');
  });

  it("passes keepPasted: copyDictationsAtDelivery from settleQuillDictation", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const body = main.match(/async function settleQuillDictation\([^)]*\) \{([\s\S]*?)\n\}/)?.[1];
    expect(body).toBeDefined();
    expect(body).toContain("keepPasted: copyDictationsAtDelivery,");
    expect(main.match(/await settleDictationRewrite\(/g)).toHaveLength(1);
  });
});
