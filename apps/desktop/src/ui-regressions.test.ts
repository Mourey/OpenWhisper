import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { TRANSCRIPTION_MODELS } from "./transcription-model";

const desktopRoot = resolve(import.meta.dirname, "..");
const repositoryRoot = resolve(desktopRoot, "../..");

describe("desktop regression contracts", () => {
  it("offers the supported transcription models including local Large v3 Turbo", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const menu = main.match(/<select id="menu-model-select"[^>]*>([\s\S]*?)<\/select>/)?.[1];
    expect(menu).toBeDefined();
    const models = Array.from(menu!.matchAll(/<option value="([^"]+)"/g), (match) => match[1]);
    expect(models).toEqual([...TRANSCRIPTION_MODELS]);
    expect(menu).toContain('<option value="whisper-turbo-local">Local — Whisper Large v3 Turbo</option>');
  });

  it("keeps custom Cargo targets out of Tauri's watched source tree", () => {
    const gitignore = readFileSync(resolve(repositoryRoot, ".gitignore"), "utf8");
    const runLocal = readFileSync(resolve(repositoryRoot, "scripts/run-local.ps1"), "utf8");

    expect(gitignore).toMatch(/^target\*\/\s*$/m);
    expect(runLocal).toContain('"target\\desktop-dev"');
    expect(runLocal).not.toContain('"apps\\desktop\\src-tauri\\target');
  });

  it("keeps the collapsed pill interior fully transparent", () => {
    const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
    const collapsedRule = css.match(
      /#widget\[data-expanded="false"\] \.widget-surface \{([\s\S]*?)\n\}/,
    )?.[1];

    expect(collapsedRule).toBeDefined();
    expect(collapsedRule).toContain("border: 1px solid rgba(120, 126, 136, 0.9)");
    expect(collapsedRule).toContain("background: transparent");
    expect(collapsedRule).toContain("box-shadow: none");
    expect(collapsedRule).not.toContain("backdrop-filter");
    expect(collapsedRule).not.toMatch(/box-shadow:[^;]*inset/);
  });

  it("can completely hide the idle pill and recover it from the tray", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const native = readFileSync(resolve(desktopRoot, "src-tauri/src/lib.rs"), "utf8");

    expect(main).toContain("Show pill when idle");
    expect(main.indexOf("Show pill when idle")).toBeLessThan(main.indexOf("Microphone</div>"));
    expect(main).toContain('aria-label="Show pill when idle"');
    expect(main).toContain('visible: isRecordingWindowVisible({');
    expect(main).toContain('starting: recordingStartupVisible');
    expect(main).toContain('manuallyRevealed: manualOverlayReveal');
    expect(native).toContain("window.hide().map_err");
    expect(native).toContain('MenuItem::with_id(app, "show", "Show Pill"');
  });

  it("reveals a hidden recording window before microphone acquisition without requesting permission at startup", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const refresh = main.match(/async function refreshMicDevices\(\) \{([\s\S]*?)\n\}/)?.[1];
    const start = main.match(/async function startRecording\(\) \{([\s\S]*?)\n\}/)?.[1];
    const focusLoss = main.match(/void listen\("overlay-lost-focus", \(\) => \{([\s\S]*?)\n\}\);/)?.[1];

    expect(refresh).toContain("enumerateDevices()");
    expect(refresh).not.toContain("getUserMedia");
    expect(start).toBeDefined();
    expect(start!.indexOf('invoke("capture_paste_target")')).toBeLessThan(start!.indexOf("recordingStartupVisible = true"));
    expect(start).toContain("revealWindow: () => applyOverlayLayout(true)");
    expect(start).toContain("return navigator.mediaDevices.getUserMedia(constraints)");
    expect(start).toContain("discard: (stream) => stream.getTracks().forEach((track) => track.stop())");
    expect(start).toContain("finally {\n    recordingStartupVisible = false;");
    expect(focusLoss).toContain("manualOverlayReveal = false");
    expect(focusLoss).not.toContain("recordingStartupVisible");
  });

  it("offers hold-to-talk and press-to-toggle shortcut modes", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const native = readFileSync(resolve(desktopRoot, "src-tauri/src/lib.rs"), "utf8");

    expect(main).toContain("Hold ${defaultShortcutLabel} to talk");
    expect(main).toContain("press ${defaultShortcutLabel} once to start listening and again to finish");
    expect(main).toContain('menuHoldToTalkToggle.setAttribute("aria-label", `Hold ${label} to talk`)');
    expect(main).toContain("holdToTalk: menuHoldToTalkToggle.checked");
    expect(native).toContain("HOLD_TO_TALK_ENABLED");
    expect(native).toContain("TOGGLE_RECORDING.store(true");
    expect(native).toContain('emit("toggle-recording", ())');
  });

  it("persists Mac shortcut choices and restores the active shortcut on registration errors", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");

    expect(main).toContain('aria-label="Recording shortcut"');
    expect(main).toContain('getRecordingShortcutChoices(isMac)');
    expect(main).toContain('recordingShortcut: shortcut,');
    expect(main).toContain('...(shortcut === "RightControl" ? { holdToTalk: true } : {})');
    expect(main).toContain('recordingShortcut = result.recordingShortcut');
    expect(main).toContain('syncRecordingShortcutUi(result.error)');
    expect(main).toContain('syncRecordingShortcutUi(settings.recordingShortcutError ?? null)');
    expect(main).not.toContain("Hold Ctrl+Space to talk");
  });

  it("keeps optional recording feedback failures from aborting transcription startup", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const feedback = main.match(/function startRecordingFeedback\(stream: MediaStream\) \{([\s\S]*?)\n\}/)?.[1];
    const capture = main.match(/onCaptureStarted: async \(stream\) => \{([\s\S]*?)\n      \},/)?.[1];

    expect(feedback).toContain('logStartupStage("visualizer create begin")');
    expect(feedback).toContain('logStartupStage("visualizer create complete")');
    expect(feedback).toContain('logStartupStage("visualizer start begin")');
    expect(feedback).toContain('logStartupStage("visualizer start complete")');
    expect(feedback).toContain('logStartupStage("volume monitor begin")');
    expect(feedback).toContain('logStartupStage("volume monitor complete")');
    expect(feedback).toContain('optional recording visualizer failed=');
    expect(feedback).toContain('optional recording volume monitor failed=');
    expect(feedback).not.toContain("throw");
    expect(capture).toContain("startRecordingFeedback(stream)");
    expect(capture).toContain("await stopRecording()");
  });

  it("cancels modifier chords without writing transcripts or showing no-speech errors", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const cancel = main.match(/async function handlePressToTalkCancel\(\) \{([\s\S]*?)\n\}/)?.[1];
    const poll = main.match(/async function pollOnce\(\) \{([\s\S]*?)\n\}/)?.[1];
    const failure = main.match(/async function handleTranscriptionFailure\(error: unknown\) \{([\s\S]*?)\n\}/)?.[1];

    expect(main).toContain('void listen("press-to-talk-cancel"');
    expect(main).toContain('void listen("recording-shortcut-status-changed"');
    expect(cancel).toContain("pressToTalkHeld = false");
    expect(cancel).toContain("shouldPasteFinalTranscript = false");
    expect(cancel).toContain("cancellation.abort()");
    expect(cancel).toContain("await stopPcmCapture()");
    expect(cancel).not.toContain("finalizeRecording");
    expect(poll).toContain("if (signal?.aborted || currentSessionId !== sessionId) return");
    expect(failure).toContain("if (signal?.aborted) return");
    expect(main).toContain('status: "cancelled"');
    expect(main).not.toContain('shortcut === "LeftCommand"');
  });

  it("keeps pending modifier cancellation from starting or cleaning up another recording", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const toggle = main.match(/async function toggleRecording\(\) \{([\s\S]*?)\n\}/)?.[1];
    const cleanup = main.match(/async function finishModifierCancelledRecording\(\) \{([\s\S]*?)\n\}/)?.[1];
    const poll = main.match(/async function pollOnce\(\) \{([\s\S]*?)\n\}/)?.[1];

    expect(toggle).toContain("if (pressToTalkStarting) return");
    expect(cleanup).toContain("if (recordingCancellationCleanup) return recordingCancellationCleanup");
    expect(cleanup).toContain("const sessionId = currentSessionId");
    expect(cleanup).toContain("const recordingId = recordingAttemptBackupId ?? currentRecordingId");
    expect(cleanup).toContain("if (currentRecordingCancellation !== cancellation) return");
    expect(cleanup).not.toContain("recordingCancellationCleanup = null");
    expect(poll).toMatch(/await finishCurrentRecording\("complete", finalTranscript, ""\);\n\s+if \(signal\?\.aborted \|\| currentSessionId !== sessionId\) return;/);
    expect(poll).toContain("isCurrent: () => !signal?.aborted && currentSessionId === sessionId");
    expect(poll).toContain('copy: () => invoke<void>("copy_to_clipboard", { text: finalTranscript })');
  });

  it("refreshes the stable local installer after every Windows build", () => {
    const buildScript = readFileSync(
      resolve(repositoryRoot, "scripts/build-windows-installer.ps1"),
      "utf8",
    );

    expect(buildScript).toContain('$StableInstaller = Join-Path $InstallerDir');
    expect(buildScript).toContain('Copy-Item -LiteralPath $VersionedInstaller -Destination $StableInstaller -Force');
    expect(buildScript).toContain('$OutDir = Join-Path $OriginalRepoRoot "artifacts/windows-installer"');
    expect(buildScript).not.toContain("if ($IsUncRepo) {");
  });

  it("updates the installed Windows app during every normal main push", () => {
    const releaseScript = readFileSync(
      resolve(repositoryRoot, "scripts/pre-push-release.ps1"),
      "utf8",
    );
    const syncScript = readFileSync(
      resolve(repositoryRoot, "scripts/sync-local-windows-release.ps1"),
      "utf8",
    );

    expect(releaseScript).toContain('sync-local-windows-release.ps1');
    expect(releaseScript).toContain('-InstallerPath $versionedInstaller');
    expect(releaseScript).toContain('-ExpectedVersion $version');
    expect(syncScript).toContain('VersionInfo.ProductVersion');
    expect(syncScript).toContain('Start-Process -FilePath $resolvedInstaller -ArgumentList "/S" -Wait');
    expect(syncScript).toContain('Local\\OpenWhisperLocalReleaseInstall');
    expect(syncScript).toContain('Start-Process -FilePath $installedExecutable');
  });

  it("builds signed releases without waiting for an interactive key prompt", () => {
    const packageJson = JSON.parse(
      readFileSync(resolve(desktopRoot, "package.json"), "utf8"),
    );

    expect(packageJson.scripts["tauri:build:signed"]).toContain("--ci");
  });

  it("keeps the transparent canvas around the expanded pill clear", () => {
    const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
    const expandedRule = css.match(
      /#widget\[data-expanded="true"\] \.widget-surface \{([\s\S]*?)\n\}/,
    )?.[1];
    const hoveredRule = css.match(
      /#widget\[data-expanded="true"\]\[data-hovered="true"\] \.widget-surface,([\s\S]*?)\n\}/,
    )?.[1];
    const recordingRule = css.match(
      /\[data-state="recording"\] \.widget-surface \{([\s\S]*?)\n\}/,
    )?.[1];

    expect(expandedRule).toBeDefined();
    expect(hoveredRule).toBeDefined();
    expect(recordingRule).toBeDefined();
    expect(expandedRule).toContain("box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.06)");
    expect(hoveredRule).toContain("box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.08)");
    expect(recordingRule).toContain("box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.08)");
    expect(expandedRule).not.toContain("backdrop-filter");
    expect(expandedRule?.match(/box-shadow:/g)).toHaveLength(1);
    expect(hoveredRule?.match(/box-shadow:/g)).toHaveLength(1);
    expect(recordingRule?.match(/box-shadow:/g)).toHaveLength(1);
  });

  it("streams realtime transcript deltas into the previously focused field", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");

    expect(main).not.toContain('id="live-transcript"');
    expect(main).toContain('invoke<PasteResult>("paste_to_target", { text: delta })');
    expect(main).toContain('invoke<PasteResult>("replace_streamed_target"');
    expect(main).toContain("import.meta.env.DEV");
    expect(main).toContain("__openwhisperRealtimeSmoke");
    expect(css).not.toMatch(/\.live-transcript\s*\{/);
  });

  it("inserts the finished transcript at the cursor without a preview pill", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
    const poll = main.match(/async function pollOnce\(\) \{([\s\S]*?)\n\}/)?.[1];

    expect(main).not.toMatch(/transcript-?preview/i);
    expect(css).not.toContain("transcript-preview");
    expect(poll).toContain('invoke<PasteResult>("paste_to_target", { text: finalTranscript })');
    expect(poll).toContain("if (delivery.notice) showToast(delivery.notice");
  });

  it("asks before installing an update and reassures users that settings are kept", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");

    expect(main).toContain("window.confirm(");
    expect(main).toContain("Your settings and API key will be kept.");
    expect(main).toContain("await update.downloadAndInstall()");
    expect(main).toContain("await relaunch()");
  });

  it("can retranscribe saved recordings without replacing their raw audio", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const native = readFileSync(resolve(desktopRoot, "src-tauri/src/lib.rs"), "utf8");

    expect(main).toContain('transcribeButton.textContent = "Transcribe"');
    expect(main).toContain("retranscribeRecording(recording)");
    expect(main).toContain('"recording_read"');
    expect(main).toContain('status: "transcribing"');
    expect(native).toContain("fn recording_read(");
    expect(native).toContain("STANDARD.encode(bytes)");
  });

  it("promotes saved transcripts over redundant retranscription", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");

    expect(main).toContain("Recordings &amp; transcripts");
    expect(main).toContain("if (transcript) {");
    expect(main).toContain('invoke("copy_to_clipboard", { text: transcript })');
    // A non-empty transcript must render the transcript surface, not Transcribe.
    expect(main.indexOf("panel.append(surface, transcriptToggle)"))
      .toBeLessThan(main.indexOf("} else {", main.indexOf("if (transcript) {")));
  });

  it("renders one accessible transcript surface for both preview and full text", () => {
    const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
    const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
    const recordingHistory = main.match(/async function refreshRecordings\(\) \{([\s\S]*?)\n\}/)?.[1];

    // Single text surface — the same element is clamped when collapsed and
    // revealed when expanded, never duplicated into a second block.
    expect(main).toContain("recording-transcript-text");
    expect(main).toContain("transcriptText.id = `recording-transcript-${recording.id}`");
    expect(main).toContain('transcriptText.textContent = transcript');
    expect(main).not.toContain("recording-transcript-body");
    expect(recordingHistory).not.toContain("transcriptPreview");

    // Disclosure control drives expansion via aria-expanded/aria-controls and
    // is a sibling of the copy button (no button nested inside a button).
    expect(main).toContain('transcriptToggle.setAttribute("aria-expanded", "false")');
    expect(main).toContain('transcriptToggle.setAttribute("aria-controls", transcriptText.id)');
    expect(main).toContain('panel.dataset.expanded = String(next)');

    // Compact icon-only copy control: accessible name + title, no "Copy" text.
    expect(main).toContain('copyButton.setAttribute("aria-label", `Copy transcript for ${recording.name}`)');
    expect(main).toContain('copyButton.title = "Copy transcript"');
    expect(main).not.toContain('copyButton.textContent = "Copy"');

    // CSS drives the collapsed preview via line-clamp + fade on one surface.
    expect(css).toContain("-webkit-line-clamp: 3");
    expect(css).toContain('.recording-transcript[data-expanded="false"][data-clamped="true"] .recording-transcript-text');
    expect(css).toContain("mask-image: linear-gradient(to bottom");
    expect(css).not.toContain(".recording-transcript-body");
  });
});

import { existsSync } from "node:fs";

describe("quill-settings", () => {
  const main = readFileSync(resolve(import.meta.dirname, "main.ts"), "utf8");
  const native = readFileSync(resolve(desktopRoot, "src-tauri/src/lib.rs"), "utf8");

  it("uses menu-toggle-row with aria-labels for both quill toggles", () => {
    const rows = Array.from(main.matchAll(/<div class="menu-toggle-row">([\s\S]*?)<\/label>/g), (match) => match[1]);
    expect(rows.some((row) => row.includes('aria-label="Rewrite with quill"') && row.includes(">Rewrite with quill</span>"))).toBe(true);
    expect(rows.some((row) => row.includes('aria-label="Clean up dictations"') && row.includes(">Clean up dictations</span>"))).toBe(true);
  });

  it("has a quill error row that is hidden when the error is null", () => {
    expect(main).toContain('<div id="menu-quill-error" class="menu-prompt-hint" role="alert" style="display:none"></div>');
    expect(main).toContain('menuQuillError.style.display = error ? "" : "none"');
  });

  it("removes the refine block from main.ts", () => {
    expect(main).not.toContain("menu-refine-settings");
    expect(main).not.toContain("menu-refine-toggle");
    expect(main).not.toContain("menu-refine-prompt-input");
  });

  it("keeps refineEnabled and refinePrompt in the type, the fallback and the Rust settings", () => {
    const type = main.match(/type AppSettings = \{([\s\S]*?)\n\};/)?.[1];
    expect(type).toContain("refineEnabled: boolean;");
    expect(type).toContain("refinePrompt: string;");
    expect(main).toContain('refineEnabled: false, refinePrompt: ""');
    const rust = native.match(/struct AppSettings \{([\s\S]*?)\n\}/)?.[1];
    expect(rust).toContain("refine_enabled: bool,");
    expect(rust).toContain("refine_prompt: String,");
  });

  it("names only gpt-realtime-transcribe as streaming in Rust and TypeScript", () => {
    const rust = native.match(/fn model_streams\(model: &str\) -> bool \{([\s\S]*?)\n\}/)?.[1];
    expect(rust).toBeDefined();
    expect(Array.from(rust!.matchAll(/"([^"]*)"/g), (match) => match[1])).toEqual(["gpt-realtime-transcribe"]);
    const line18 = readFileSync(resolve(import.meta.dirname, "transcription-model.ts"), "utf8").split("\n")[17];
    expect(line18).toContain("streamsText");
    expect(Array.from(line18.matchAll(/"([^"]*)"/g), (match) => match[1])).toEqual(["gpt-realtime-transcribe"]);
  });

  it("disables Clean up dictations while the hook is off or invalid", () => {
    expect(main).toContain("menuQuillCleanupToggle.disabled = !quillHookReady(");
  });
});

describe("quill-cut", () => {
  const source = (path: string) => readFileSync(resolve(desktopRoot, path), "utf8");
  const native = () => source("src-tauri/src/lib.rs");
  const main = () => source("src/main.ts");
  const surfaces = () => source("src/quill-surfaces.ts");
  const styles = () => source("src/styles.css");

  /** Every name `quill-surfaces.ts` exports, read from its source text. */
  const exportedNames = (text: string) => {
    const names = new Set<string>();
    for (const match of text.matchAll(
      /\bexport\s+(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:const|let|var|function\*?|class|type|interface|enum)\s+([A-Za-z_$][\w$]*)/g,
    )) {
      names.add(match[1]);
    }
    for (const match of text.matchAll(/\bexport\s+(?:type\s+)?\{([^}]*)\}/g)) {
      for (const part of match[1].split(",")) {
        const name = part.trim().split(/\s+as\s+/).pop()?.replace(/^type\s+/, "").trim();
        if (name) names.add(name);
      }
    }
    return names;
  };

  const handlerNames = (text: string) => {
    const handler = text.match(/tauri::generate_handler!\[([\s\S]*?)\]/)?.[1];
    expect(handler).toBeDefined();
    return handler!.split(",").map((name) => name.trim()).filter(Boolean);
  };

  const WITHDRAWN_EXPORTS = [
    "QuillPicker",
    "QuillPickerKey",
    "QuillPickerView",
    "QuillHotkey",
    "QuillRewritten",
    "QuillRevertSlot",
    "QuillRevertView",
    "hotkeySlotAllowed",
    "openSlotAfterDictation",
    "QUILL_PRESS_WINDOW_MS",
    "QUILL_PICKER_IDLE_MS",
    "QUILL_REVERT_WINDOW_MS",
    "QUILL_STILL_REWRITING",
    "QUILL_REVERT_COPIED",
    "QUILL_CLIPBOARD_REPLACED",
  ];

  const MODE_SELECT_EXPORTS = [
    "loadModeSelect",
    "QuillModeSelect",
    "QUILL_MODE_HINT",
    "QuillCatalog",
    "effectiveMode",
    "orderModes",
    "modeName",
  ];

  const HOOK_CORE_COMMANDS = [
    "quill_rewrite_selection",
    "quill_rewrite_text",
    "quill_revert",
    "quill_report_outcome",
    "quill_list_modes",
  ];

  // Hook-core's two selection commands as they stand at 93e4234, held verbatim.
  const QUILL_REWRITE_SELECTION_AT_BASE = `async fn quill_rewrite_selection(
    state: State<'_, Arc<AppState>>,
    mode: String,
    pressed_at_ms: u64,
) -> Result<QuillRewriteSelectionResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        with_quill(&state, |slot, command, env| {
            Box::pin(async move {
                quill_client::run_rewrite_selection(
                    slot,
                    command,
                    env,
                    &mut quill_client::NativeHost,
                    &mode,
                    pressed_at_ms,
                )
                .await
            })
        })
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, mode, pressed_at_ms);
        Err(quill_unsupported())
    }
}`;

  const QUILL_REVERT_AT_BASE = `async fn quill_revert(
    state: State<'_, Arc<AppState>>,
    rewrite_id: String,
    delivered: Option<String>,
) -> Result<QuillRevertResult, QuillCommandError> {
    #[cfg(target_os = "macos")]
    {
        with_quill(&state, |slot, command, env| {
            Box::pin(async move {
                quill_client::run_revert(
                    slot,
                    command,
                    env,
                    &mut quill_client::NativeHost,
                    &rewrite_id,
                    delivered.as_deref(),
                )
                .await
            })
        })
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&state, rewrite_id, delivered);
        Err(quill_unsupported())
    }
}`;

  it("has no picker or revert command, revert arming or revert key in lib.rs", () => {
    const rust = native();
    for (const name of ["quill_picker_open", "quill_picker_close", "quill_revert_arm", "quill_revert_armed", "arm_revert", "KeyZ"]) {
      expect(rust, name).not.toContain(name);
    }
  });

  it("has no hotkey listener, picker or revert slot in main.ts", () => {
    const text = main();
    for (const name of ["quill-picker", "quill-revert", "quill-rewrite", "QuillHotkey"]) {
      expect(text, name).not.toContain(name);
    }
    for (const name of ["quill_picker_open", "quill_picker_close", "quill_revert_arm", "quill_revert_armed"]) {
      expect(text, name).not.toContain(name);
    }
  });

  it("exports no hotkey, picker or revert-slot name from quill-surfaces.ts", () => {
    const text = surfaces();
    const names = exportedNames(text);
    expect(names.size).toBeGreaterThan(0);
    for (const name of WITHDRAWN_EXPORTS) {
      expect(names.has(name), name).toBe(false);
    }
    for (const name of ["quill_picker_open", "quill_picker_close", "quill_revert_arm", "quill_revert_armed"]) {
      expect(text, name).not.toContain(name);
    }
  });

  it("has no default-mode select in main.ts", () => {
    const text = main();
    for (const name of ["menu-quill-mode", "quillDefaultMode", "loadModeSelect", "QuillCatalog", "quill_list_modes"]) {
      expect(text, name).not.toContain(name);
    }
  });

  it("exports no mode-select name from quill-surfaces.ts and never names quillDefaultMode", () => {
    const text = surfaces();
    const names = exportedNames(text);
    for (const name of MODE_SELECT_EXPORTS) {
      expect(names.has(name), name).toBe(false);
    }
    expect(text).not.toContain("quillDefaultMode");
  });

  it("still exports QUILL_NOTICES and quillNotice from quill-surfaces.ts", () => {
    const names = exportedNames(surfaces());
    expect(names.has("QUILL_NOTICES")).toBe(true);
    expect(names.has("quillNotice")).toBe(true);
  });

  it("still names every hook-core command in generate_handler!, and no withdrawn quill command", () => {
    const names = handlerNames(native());
    for (const command of HOOK_CORE_COMMANDS) {
      expect(names, command).toContain(command);
    }
    expect(names.filter((name) => name.startsWith("quill_")).sort()).toEqual(
      [...HOOK_CORE_COMMANDS, "quill_clipboard_hold", "quill_clipboard_release"].sort(),
    );
  });

  it("keeps quill_rewrite_selection and quill_revert as they stand at 93e4234", () => {
    const rust = native();
    expect(QUILL_REWRITE_SELECTION_AT_BASE.startsWith("async fn quill_rewrite_selection(")).toBe(true);
    expect(QUILL_REVERT_AT_BASE.startsWith("async fn quill_revert(")).toBe(true);
    expect(rust).toContain(`#[tauri::command]\n${QUILL_REWRITE_SELECTION_AT_BASE}`);
    expect(rust).toContain(`#[tauri::command]\n${QUILL_REVERT_AT_BASE}`);
  });

  it("has no picker or revert-slot rule in styles.css, and keeps the rules kept code uses", () => {
    const css = styles();
    expect(css).not.toContain("quill-picker");
    expect(css).not.toContain("quill-revert");
    expect(css).toMatch(/^\.visually-hidden \{$/m);
    expect(css).toMatch(/^\.toast\[role="alert"\] \{$/m);
  });

  it("does not create the hotkey, picker or revert test files", () => {
    for (const file of ["quill-hotkey.test.ts", "quill-picker.test.ts", "quill-revert.test.ts"]) {
      expect(existsSync(resolve(desktopRoot, "src", file)), file).toBe(false);
    }
  });
});

describe("dictation-copy-settings", () => {
  const main = () => readFileSync(resolve(desktopRoot, "src/main.ts"), "utf8");
  const native = () => readFileSync(resolve(desktopRoot, "src-tauri/src/lib.rs"), "utf8");

  const LABEL = "Also copy dictations to clipboard";
  const INPUT = `<input id="menu-copy-dictations-toggle" type="checkbox" aria-label="${LABEL}" />`;
  const QUILL_BLOCK_END = '<div id="menu-quill-error" class="menu-prompt-hint" role="alert" style="display:none"></div>';

  /** The toggle's row, from its `menu-toggle-row` opening tag to the row's closing tag. */
  const toggleRow = (text: string) => {
    const rows = Array.from(text.matchAll(/<div class="menu-toggle-row">[\s\S]*?<\/label>\s*<\/div>/g), (match) => match[0]);
    const matching = rows.filter((row) => row.includes("menu-copy-dictations-toggle"));
    expect(matching).toHaveLength(1);
    return matching[0];
  };

  /** The name main.ts gives the toggle's element. */
  const toggleElement = (text: string) => {
    const name = text.match(/const (\w+) = document\.getElementById\("menu-copy-dictations-toggle"\) as HTMLInputElement;/)?.[1];
    expect(name).toBeDefined();
    return name!;
  };

  // The two clipboard commands as they stand at 93e4234, held verbatim.
  const COPY_TO_CLIPBOARD_AT_BASE = `fn copy_to_clipboard(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|e| e.to_string())
}`;

  const PASTE_TO_TARGET_AT_BASE = `async fn paste_to_target(text: String) -> Result<PasteResult, String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(&text))
        .map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    return Ok(macos_text_inserter::paste_to_captured_target(&text));

    #[cfg(target_os = "windows")]
    {
        text_inserter::restore_and_paste();
        Ok(PasteResult {
            pasted: true,
            reason: None,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Ok(PasteResult {
        pasted: false,
        reason: Some("unsupported_target"),
    })
}`;

  it("puts the toggle in a menu-toggle-row with its label", () => {
    const row = toggleRow(main());
    expect(row.startsWith('<div class="menu-toggle-row">')).toBe(true);
    expect(row).toContain(`>${LABEL}</span>`);
    expect(row).toContain('<label class="toggle-switch"');
  });

  it("gives the input its id and aria-label, and never a disabled attribute", () => {
    const text = main();
    const row = toggleRow(text);
    expect(row).toContain(INPUT);
    expect(text.split('id="menu-copy-dictations-toggle"')).toHaveLength(2);
    const input = row.match(/<input[^>]*>/g);
    expect(input).toHaveLength(1);
    expect(input![0]).not.toContain("disabled");
  });

  it("places the row right after the quill block's closing markup", () => {
    const text = main();
    const end = text.indexOf(QUILL_BLOCK_END);
    expect(end).toBeGreaterThan(0);
    const row = toggleRow(text);
    const at = text.indexOf(row);
    expect(at).toBeGreaterThan(end);
    expect(text.slice(end + QUILL_BLOCK_END.length, at).trim()).toBe("");
    for (const id of ["menu-quill-enabled-toggle", "menu-quill-cleanup-toggle", "menu-quill-error"]) {
      expect(text.indexOf(`id="${id}"`), id).toBeLessThan(at);
      expect(text.indexOf(`id="${id}"`, at), id).toBe(-1);
    }
  });

  it("saves copyDictationsToClipboard on change and shows exactly Copy dictations on or off", () => {
    const text = main();
    const element = toggleElement(text);
    const start = text.indexOf(`${element}.addEventListener("change", () => {`);
    expect(start).toBeGreaterThan(0);
    const handler = text.slice(start, text.indexOf("\n});\n", start));
    expect(handler).toMatch(
      new RegExp(`const (\\w+) = ${element}\\.checked;\\s*void saveQuillSettings\\(\\{ copyDictationsToClipboard: \\1 \\}, \\1 \\? "Copy dictations on" : "Copy dictations off"\\);`),
    );
    expect(text.split('"Copy dictations on"')).toHaveLength(2);
    expect(text.split('"Copy dictations off"')).toHaveLength(2);
    expect(text).toContain('const updated = await invoke<AppSettings>("app_save_settings", update);');
  });

  it("shows the stored value: checked comes from copyDictationsToClipboard ?? false", () => {
    const text = main();
    const element = toggleElement(text);
    expect(text).toContain(`${element}.checked = settings.copyDictationsToClipboard ?? false;`);
  });

  it("never assigns disabled on the toggle's element", () => {
    const text = main();
    const element = toggleElement(text);
    expect(text).not.toContain(`${element}.disabled`);
    expect(text).not.toMatch(new RegExp(`${element}\\.(?:setAttribute|toggleAttribute)\\("disabled"`));
  });

  it("declares copyDictationsToClipboard in the AppSettings type", () => {
    const type = main().match(/type AppSettings = \{([\s\S]*?)\n\};/)?.[1];
    expect(type).toContain("copyDictationsToClipboard?: boolean;");
  });

  it("keeps copy_to_clipboard and paste_to_target as they stand at 93e4234", () => {
    const rust = native();
    expect(COPY_TO_CLIPBOARD_AT_BASE.startsWith("fn copy_to_clipboard(text: String)")).toBe(true);
    expect(PASTE_TO_TARGET_AT_BASE.startsWith("async fn paste_to_target(text: String)")).toBe(true);
    expect(rust).toContain(`#[tauri::command]\n${COPY_TO_CLIPBOARD_AT_BASE}`);
    expect(rust).toContain(`#[tauri::command]\n${PASTE_TO_TARGET_AT_BASE}`);
  });

  it("adds no history exclusion, transient type or concealed type to any clipboard write", () => {
    const rust = native();
    for (const name of ["exclude_from_history", "TransientType", "ConcealedType"]) {
      expect(rust, name).not.toContain(name);
    }
  });
});
