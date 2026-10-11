import { describe, expect, it, vi } from "vitest";
import {
  formatRecordingShortcut,
  getRecordingShortcutChoices,
  isMacPlatform,
  saveRecordingShortcut,
} from "./recording-shortcut";

describe("recording shortcuts", () => {
  it("detects macOS from platform or user agent", () => {
    expect(isMacPlatform("MacIntel", "")).toBe(true);
    expect(isMacPlatform("", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)")).toBe(true);
    expect(isMacPlatform("Win32", "Mozilla/5.0 (Windows NT 10.0)")).toBe(false);
    expect(isMacPlatform("Linux x86_64", "Mozilla/5.0 (X11; Linux x86_64)")).toBe(false);
  });

  it("offers Right Control, Space and W on Mac while keeping Ctrl+Space on Windows", () => {
    expect(getRecordingShortcutChoices(true)).toEqual([
      "RightControl",
      "Control+Alt+Space",
      "Control+Alt+KeyW",
    ]);
    expect(getRecordingShortcutChoices(false)).toEqual(["Control+Space"]);
  });

  it.each([
    ["RightControl", "Right Control"],
    ["Control+Alt+Space", "Control+Option+Space"],
    ["Control+Alt+KeyW", "Control+Option+W"],
    ["Control+Space", "Ctrl+Space"],
  ])("formats %s as %s", (shortcut, label) => {
    expect(formatRecordingShortcut(shortcut)).toBe(label);
  });

  it("uses Right Control as the initial Mac choice", () => {
    expect(getRecordingShortcutChoices(true)[0]).toBe("RightControl");
    expect(formatRecordingShortcut(getRecordingShortcutChoices(true)[0])).toBe("Right Control");
  });

  it("accepts a Right Control hold-to-talk binding returned by the native save", async () => {
    const settings = { recordingShortcut: "RightControl", holdToTalk: true };
    const save = vi.fn().mockResolvedValue(settings);
    const result = await saveRecordingShortcut("Control+Alt+Space", "RightControl", save);
    expect(save).toHaveBeenCalledWith("RightControl");
    expect(result.settings).toEqual(settings);
    expect(result.recordingShortcut).toBe("RightControl");
    expect(result.error).toBeNull();
  });

  it("uses the registered shortcut returned by a successful save", async () => {
    const settings = { recordingShortcut: "Control+Alt+KeyW", holdToTalk: true };
    const save = vi.fn().mockResolvedValue(settings);
    const result = await saveRecordingShortcut(
      "Control+Alt+Space",
      "Control+Alt+KeyW",
      save,
    );

    expect(save).toHaveBeenCalledWith("Control+Alt+KeyW");
    expect(result).toEqual({
      recordingShortcut: "Control+Alt+KeyW",
      settings,
      error: null,
    });
  });

  it("restores the active shortcut when registration rejects the selected key", async () => {
    const save = vi.fn().mockRejectedValue(new Error("Shortcut is already in use"));
    const result = await saveRecordingShortcut(
      "Control+Alt+Space",
      "Control+Alt+KeyW",
      save,
    );

    expect(save).toHaveBeenCalledWith("Control+Alt+KeyW");
    expect(result).toEqual({
      recordingShortcut: "Control+Alt+Space",
      settings: null,
      error: "Shortcut is already in use",
    });
  });
});
