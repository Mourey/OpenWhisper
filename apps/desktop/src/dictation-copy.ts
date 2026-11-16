// The "Also copy dictations to clipboard" decision. It imports nothing from Tauri and touches
// no DOM, so vitest runs it in node; main.ts makes the copy.

/**
 * The text to copy after a finished dictation, or `null` for no copy: the setting is off, the
 * session was abandoned or replaced (`delivery` is null), or the text was not inserted, in which
 * case the delivery already copied it. Otherwise `text`, the transcript that was delivered.
 */
export function dictationCopyText(input: {
  enabled: boolean;
  delivery: { inserted: boolean } | null | undefined;
  text: string;
}): string | null {
  if (!input.enabled || !input.delivery || !input.delivery.inserted) return null;
  return input.text;
}
