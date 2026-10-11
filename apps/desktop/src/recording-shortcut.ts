export function isMacPlatform(platform: string, userAgent: string) {
  return /Mac/i.test(platform) || /Macintosh|Mac OS X/i.test(userAgent);
}

export function getRecordingShortcutChoices(mac: boolean) {
  return mac
    ? ["RightControl", "Control+Alt+Space", "Control+Alt+KeyW"]
    : ["Control+Space"];
}

export function formatRecordingShortcut(shortcut: string) {
  if (shortcut === "RightControl") return "Right Control";
  return shortcut
    .replace("Control+Alt+", "Control+Option+")
    .replace("Control+", shortcut.includes("Alt+") ? "Control+" : "Ctrl+")
    .replace("KeyW", "W");
}

export async function saveRecordingShortcut<T extends { recordingShortcut: string }>(
  currentShortcut: string,
  selectedShortcut: string,
  save: (shortcut: string) => Promise<T>,
) {
  try {
    const settings = await save(selectedShortcut);
    return { recordingShortcut: settings.recordingShortcut, settings, error: null };
  } catch (error) {
    return {
      recordingShortcut: currentShortcut,
      settings: null,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}
