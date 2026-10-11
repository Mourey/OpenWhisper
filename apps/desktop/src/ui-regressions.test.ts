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
