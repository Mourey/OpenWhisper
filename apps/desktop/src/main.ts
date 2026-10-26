import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  enable as enableAutostart,
  isEnabled as isAutostartEnabled,
} from "@tauri-apps/plugin-autostart";
import { relaunch } from "@tauri-apps/plugin-process";
import { check } from "@tauri-apps/plugin-updater";
import { continuousVisualizer } from "sound-visualizer";
import {
  COLLAPSED_HEIGHT,
  COLLAPSED_WIDTH,
  EXPANDED_HEIGHT,
  EXPANDED_WIDTH,
  WINDOW_BOTTOM_PADDING,
  getOverlayLayout,
} from "./overlay-layout";
import { prepareRecordingCapture, SessionAudioQueue, shouldCancelEmptyHold } from "./recording-pipeline";
import {
  DEFAULT_TRANSCRIPTION_MODEL,
  getModelCapabilities,
  needsApiKey,
  type TranscriptionModel,
} from "./transcription-model";
import {
  formatRecordingShortcut,
  getRecordingShortcutChoices,
  isMacPlatform,
  saveRecordingShortcut,
} from "./recording-shortcut";
import {
  AUDIO_CONTEXT_STARTUP_TIMEOUT_MS,
  acquireVisibleMicrophone,
  isRecordingWindowVisible,
  resumeRecordingAudioContext,
  waitForStartupStep,
} from "./recording-startup";
import {
  cancellableRecordingStep,
  completeRecordingCancellation,
  throwIfRecordingCancelled,
} from "./recording-cancellation";
import {
  deliverTranscript,
  type PasteResult,
} from "./transcript-delivery";
import {
  QuillCatalog,
  QuillHotkey,
  QuillPicker,
  QuillRevertSlot,
  dictationCleanupEffective,
  dictationDelivered,
  escapeHtml,
  loadModeSelect,
  modeName,
  quillHookReady,
  hotkeySlotAllowed,
  quillSettingsFromRust,
  quillLogLine,
  quillNoticeDurationMs,
  recordingStartBlocked,
  rewriteDictation,
  settleDictationRewrite,
  shouldRewriteDictation,
  toQuillCommandError,
  type QuillDictationRewrite,
  type QuillNoticeRole,
  type QuillPickerView,
  type QuillRevertView,
} from "./quill-surfaces";
import "./styles.css";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------
type TranscriptEvent = { type: "partial" | "final" | "warning" | "error"; text: string };
type PollResponse = { events: TranscriptEvent[]; done?: boolean };
type AppSettings = {
  hasOpenaiApiKey: boolean;
  openaiApiKeyPreview: string | null;
  transcriptionModel: TranscriptionModel;
  transcriptionPrompt: string;
  refineEnabled: boolean;
  refinePrompt: string;
  showIdlePill: boolean;
  holdToTalk: boolean;
  recordingShortcut: string;
  recordingShortcutError?: string | null;
  quillCommand?: string;
  quillCommandError?: string | null;
  quillEnabled?: boolean;
  quillDefaultMode?: string;
  quillDictationCleanup?: boolean;
};
type WidgetState = "idle" | "recording" | "transcribing" | "error" | "rewriting";
const isMac = isMacPlatform(navigator.platform, navigator.userAgent);
const shortcutChoices = getRecordingShortcutChoices(isMac);
const defaultRecordingShortcut = shortcutChoices[0];
const defaultShortcutLabel = formatRecordingShortcut(defaultRecordingShortcut);
type MicDevice = { id: string; label: string };
type RecordingMetadata = {
  id: string;
  createdAtMs: number;
  name: string;
  source: string;
  mimeType: string;
  fileName: string;
  status: string;
  bytes: number;
  transcript: string;
  error: string;
};
type RecordingAudio = {
  mimeType: string;
  fileBase64: string;
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let state: WidgetState = "idle";
let currentSessionId: string | null = null;
let pollTimer: number | null = null;
let pollInFlight = false;
let pollFailureCount = 0;
let finalTranscript = "";
let liveTranscript = "";
let streamTextToTarget = false;
let pastedTranscript = "";
let targetWriteQueue: Promise<void> = Promise.resolve();
let lastTranscript = localStorage.getItem("ow_last_transcript") ?? "";
let mediaRecorder: MediaRecorder | null = null;
let pcmAudioContext: AudioContext | null = null;
let pcmSource: MediaStreamAudioSourceNode | null = null;
let pcmProcessor: ScriptProcessorNode | null = null;
let pcmSilentGain: GainNode | null = null;
let recordingMimeType = "audio/webm";
let isRecording = false;
let currentRecordingId: string | null = null;
let lastFailedRecordingId: string | null = null;
let backupTail: Promise<void> = Promise.resolve();
let backupError: unknown = null;
let backupAudioQueue: SessionAudioQueue | null = null;
let capturedPcmBytes = 0;
let pendingAudioParts: Uint8Array[] = [];
let pendingAudioBytes = 0;
let sessionFailure = "";
let shouldPasteFinalTranscript = true;
let pressToTalkHeld = false;
let pressToTalkStarting = false;
let toggleRecordingStarting = false;
let toggleStopRequestedDuringStart = false;
let recordingStartupVisible = false;
let currentRecordingCancellation: AbortController | null = null;
let recordingCancellationCleanup: Promise<void> | null = null;
let recordingAttemptBackupId: string | null = null;
// Quill: cleanup decided at session start, and a dictation rewrite awaiting its outcome.
let quillCleanupAtStart = false;
let quillDictationPending: QuillDictationRewrite | null = null;
const audioQueue = new SessionAudioQueue((sessionId, buffer) =>
  invoke<void>("worker_append_audio_chunk", {
    sessionId,
    chunkBase64: toBase64(buffer),
  }),
);

let micStream: MediaStream | null = null;
let soundVisualizer: { start: () => void; stop: () => void; reset: () => void } | null = null;
let volumeMonitorFrameId: number | null = null;
let volumeMonitorDisconnect: (() => void) | null = null;
const SILENT_THRESHOLD = 14;
let micDevices: MicDevice[] = [];
let selectedMicId = "default";
let menuOpen = false;

const HOVER_EXPAND_DELAY_MS = 40;
const HOVER_COLLAPSE_GRACE_MS = 90;
const AUDIO_BATCH_BYTES = 16 * 1024;

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------
const app = document.getElementById("app")!;
app.innerHTML = `
  <div id="widget-shell">
    <div id="api-key-bubble" class="api-key-bubble hidden" role="button" tabindex="0" aria-label="Add your OpenAI API key in settings">
      <span class="api-key-bubble-logo" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none">
          <path d="M2 4L6 20L12 10L18 20L22 4" />
        </svg>
      </span>
      <span class="api-key-bubble-text">Add OpenAI API key</span>
      <span class="api-key-bubble-arrow" aria-hidden="true">
        <svg viewBox="0 0 20 20" fill="none">
          <path d="M7 5L12 10L7 15" />
        </svg>
      </span>
    </div>
    <div id="widget" tabindex="0" role="button" aria-label="Click to start or stop transcription">
      <div class="widget-surface" aria-hidden="true"></div>

      <span class="meter meter-wave-bars" aria-hidden="true">
        <span class="meter-baseline" aria-hidden="true"></span>
        <svg class="meter-idle-wave" viewBox="0 0 76 28" aria-hidden="true">
          <path class="meter-idle-wave-path" d="M0,14 C6,11 12,17 18,14 C24,11 30,17 36,14 C42,11 48,17 54,14 C60,11 66,17 72,14 C74,12 76,14" fill="none" stroke="currentColor" stroke-width="0.9" stroke-linecap="round" />
        </svg>
        <canvas id="meter-canvas" class="meter-canvas" width="48" height="28"></canvas>
        <span class="meter-idle-bars">
          <span class="wave-bar" data-bar="0"></span>
          <span class="wave-bar" data-bar="1"></span>
          <span class="wave-bar" data-bar="2"></span>
          <span class="wave-bar" data-bar="3"></span>
          <span class="wave-bar" data-bar="4"></span>
          <span class="wave-bar" data-bar="5"></span>
          <span class="wave-bar" data-bar="6"></span>
        </span>
      </span>

      <button id="btn-stop" class="stop-btn" type="button" title="Stop recording" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="currentColor">
          <rect x="6.5" y="6.5" width="11" height="11" rx="3" />
        </svg>
      </button>

      <button id="btn-mic" class="mic-btn" type="button" title="Click to start recording">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" />
          <path d="M19 10v2a7 7 0 0 1-14 0v-2" />
          <line x1="12" x2="12" y1="19" y2="22" />
        </svg>
      </button>

      <span id="record-control" class="record-control" aria-hidden="true" title="Click to toggle recording">
        <svg class="icon-dots" viewBox="0 0 24 8" fill="currentColor">
          <circle cx="4" cy="4" r="2" />
          <circle cx="12" cy="4" r="2" />
          <circle cx="20" cy="4" r="2" />
        </svg>
        <svg class="icon-spin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4">
          <path d="M12 2.75A9.25 9.25 0 0 1 21.25 12" stroke-linecap="round" />
        </svg>
      </span>

      <button id="btn-dropdown" class="dropdown-btn" type="button" title="Select microphone and settings">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
          <path d="M7.5 10.5 12 15 16.5 10.5" />
        </svg>
      </button>
    </div>

    <div id="mic-menu" class="mic-menu hidden">
      <div id="failure-panel" class="failure-panel hidden" role="alert">
        <div class="failure-title">Transcription failed</div>
        <div id="failure-message" class="failure-message"></div>
        <div class="menu-action-row">
          <button id="failure-open-recording" class="menu-action" type="button">Open raw audio</button>
          <button id="failure-dismiss" class="menu-action menu-action-secondary" type="button">Dismiss</button>
        </div>
      </div>
      <div class="menu-section">Recordings &amp; transcripts</div>
      <div class="menu-action-row menu-action-row-wide">
        <button id="menu-import-audio" class="menu-action" type="button">Transcribe audio file</button>
        <button id="menu-open-recordings" class="menu-icon-action" type="button" title="Open recordings folder" aria-label="Open recordings folder">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
            <path d="M3.5 7.5h6l2 2h9v9.5a1.5 1.5 0 0 1-1.5 1.5H5A1.5 1.5 0 0 1 3.5 19Z" />
            <path d="M3.5 7.5V5A1.5 1.5 0 0 1 5 3.5h5l2 2h5" />
          </svg>
        </button>
      </div>
      <input id="audio-file-input" type="file" accept="audio/*,.wav,.mp3,.m4a,.mp4,.mpeg,.mpga,.webm,.ogg" hidden />
      <div id="recent-recordings" class="recent-recordings"></div>
      <div class="menu-divider"></div>
      <div class="menu-section">Appearance</div>
      <div class="menu-toggle-row">
        <span class="menu-section menu-section-inline">Show pill when idle</span>
        <label class="toggle-switch" title="Keep the collapsed pill visible when not recording">
          <input id="menu-show-idle-pill-toggle" type="checkbox" aria-label="Show pill when idle" />
          <span class="toggle-track"></span>
          <span class="toggle-thumb"></span>
        </label>
      </div>
      <div class="menu-prompt-hint">Turn this off to completely hide the collapsed pill. Use the tray icon to show it again.</div>
      <div class="menu-divider"></div>
      <div class="menu-section">Shortcut</div>
      <select id="menu-recording-shortcut" class="menu-select" aria-label="Recording shortcut" ${isMac ? "" : 'style="display:none"'}>
        ${shortcutChoices.map((shortcut) => `<option value="${shortcut}">${formatRecordingShortcut(shortcut)}</option>`).join("")}
      </select>
      <div class="menu-toggle-row">
        <span id="menu-hold-to-talk-label" class="menu-section menu-section-inline">Hold ${defaultShortcutLabel} to talk</span>
        <label id="menu-hold-to-talk-switch" class="toggle-switch" title="When off, press ${defaultShortcutLabel} once to start and again to finish">
          <input id="menu-hold-to-talk-toggle" type="checkbox" aria-label="Hold ${defaultShortcutLabel} to talk" />
          <span class="toggle-track"></span>
          <span class="toggle-thumb"></span>
        </label>
      </div>
      <div id="menu-shortcut-hint" class="menu-prompt-hint">Turn this off to press ${defaultShortcutLabel} once to start listening and again to finish.</div>
      <div id="menu-shortcut-error" class="menu-prompt-hint" role="alert" style="display:none"></div>
      <div class="menu-divider"></div>
      <div class="menu-section">Microphone</div>
      <div id="mic-list"></div>
      <div class="menu-divider"></div>
      <div class="menu-section">Transcription model</div>
      <select id="menu-model-select" class="menu-select" aria-label="Transcription model">
        <option value="whisper-base-local">Local — Whisper Base (60 MB, multilingual)</option>
        <option value="whisper-tiny-local">Smallest local — Whisper Tiny (32 MB, multilingual)</option>
        <option value="whisper-turbo-local">Local — Whisper Large v3 Turbo</option>
        <option value="gpt-4o-mini-transcribe">Fast — GPT-4o mini ($1.25/$5 per 1M tokens)</option>
        <option value="gpt-4o-transcribe">Accurate — GPT-4o ($2.50/$10 per 1M tokens)</option>
        <option value="gpt-realtime-transcribe">Realtime — GPT-Realtime-Whisper ($0.017/min)</option>
      </select>
      <div id="menu-model-hint" class="menu-prompt-hint"></div>
      <div id="menu-cloud-settings" style="display:none">
        <div class="menu-divider"></div>
        <div class="menu-section">OpenAI API Key</div>
        <div class="menu-item" id="menu-key-status">Not configured</div>
        <input id="menu-key-input" class="menu-key-input" type="password" placeholder="sk-..." autocomplete="off" spellcheck="false" />
        <a id="menu-key-link" class="menu-link" href="https://platform.openai.com/api-keys">
          <span class="menu-link-bold">Create or manage</span> keys at platform.openai.com/api-keys
        </a>
      </div>
      <div class="menu-divider"></div>
      <div class="menu-section">Dictionary</div>
      <div id="menu-target-app" class="menu-target-app">
        <span class="target-app-dot"></span>
        <span id="menu-target-app-name">Detecting…</span>
      </div>
      <textarea id="menu-prompt-input" class="menu-prompt-input" rows="3" placeholder="e.g. Glossary: @README.md, @package.json, Daniël" spellcheck="false"></textarea>
      <div class="menu-prompt-hint">Custom vocabulary hints for the transcription model (224 token limit)</div>
      <div class="menu-divider"></div>
      <div class="menu-section">Quill</div>
      <div class="menu-toggle-row">
        <span class="menu-section menu-section-inline">Rewrite with quill</span>
        <label class="toggle-switch" title="Rewrite the selected text with Control+Option+R">
          <input id="menu-quill-enabled-toggle" type="checkbox" aria-label="Rewrite with quill" />
          <span class="toggle-track"></span>
          <span class="toggle-thumb"></span>
        </label>
      </div>
      <select id="menu-quill-mode" class="menu-select" aria-label="Default rewrite mode" disabled></select>
      <div id="menu-quill-mode-hint" class="menu-prompt-hint">Turn on Quill to choose a mode</div>
      <div class="menu-toggle-row">
        <span class="menu-section menu-section-inline">Clean up dictations</span>
        <label class="toggle-switch" title="Rewrite each finished dictation with quill before inserting it">
          <input id="menu-quill-cleanup-toggle" type="checkbox" aria-label="Clean up dictations" disabled />
          <span class="toggle-track"></span>
          <span class="toggle-thumb"></span>
        </label>
      </div>
      <div id="menu-quill-error" class="menu-prompt-hint" role="alert" style="display:none"></div>
    </div>

    <div id="quill-picker" class="quill-picker hidden" role="listbox" aria-label="Rewrite modes"></div>
    <div id="quill-revert" class="quill-revert" data-active="false">
      <span id="quill-revert-text"></span>
      <button id="quill-revert-button" type="button" aria-keyshortcuts="Control+Alt+Z">Revert <span id="quill-revert-countdown" aria-hidden="true"></span></button>
    </div>
    <div id="quill-live" class="visually-hidden" aria-live="polite"></div>
  </div>

  <div id="toast" class="toast hidden"></div>
`;

const apiKeyBubble = document.getElementById("api-key-bubble")!;
const widget = document.getElementById("widget")!;
const meterCanvas = document.getElementById("meter-canvas") as HTMLCanvasElement;
const waveBars = document.querySelectorAll<HTMLElement>(".wave-bar");
const btnMic = document.getElementById("btn-mic") as HTMLButtonElement;
const btnDropdown = document.getElementById("btn-dropdown") as HTMLButtonElement;
const micMenu = document.getElementById("mic-menu")!;
const micList = document.getElementById("mic-list")!;
const menuModelSelect = document.getElementById("menu-model-select") as HTMLSelectElement;
const menuModelHint = document.getElementById("menu-model-hint")!;
const menuCloudSettings = document.getElementById("menu-cloud-settings")!;
const toast = document.getElementById("toast")!;
const menuKeyStatus = document.getElementById("menu-key-status")!;
const menuKeyInput = document.getElementById("menu-key-input") as HTMLInputElement;
const menuKeyLink = document.getElementById("menu-key-link") as HTMLAnchorElement;
const menuPromptInput = document.getElementById("menu-prompt-input") as HTMLTextAreaElement;
const menuQuillEnabledToggle = document.getElementById("menu-quill-enabled-toggle") as HTMLInputElement;
const menuQuillMode = document.getElementById("menu-quill-mode") as HTMLSelectElement;
const menuQuillModeHint = document.getElementById("menu-quill-mode-hint")!;
const menuQuillCleanupToggle = document.getElementById("menu-quill-cleanup-toggle") as HTMLInputElement;
const menuQuillError = document.getElementById("menu-quill-error")!;
const quillPickerElement = document.getElementById("quill-picker")!;
const quillRevertElement = document.getElementById("quill-revert")!;
const quillRevertText = document.getElementById("quill-revert-text")!;
const quillRevertButton = document.getElementById("quill-revert-button") as HTMLButtonElement;
const quillRevertCountdown = document.getElementById("quill-revert-countdown")!;
const quillLive = document.getElementById("quill-live")!;
const menuShowIdlePillToggle = document.getElementById("menu-show-idle-pill-toggle") as HTMLInputElement;
const menuHoldToTalkToggle = document.getElementById("menu-hold-to-talk-toggle") as HTMLInputElement;
const menuRecordingShortcut = document.getElementById("menu-recording-shortcut") as HTMLSelectElement;
const menuHoldToTalkLabel = document.getElementById("menu-hold-to-talk-label")!;
const menuHoldToTalkSwitch = document.getElementById("menu-hold-to-talk-switch")!;
const menuShortcutHint = document.getElementById("menu-shortcut-hint")!;
const menuShortcutError = document.getElementById("menu-shortcut-error")!;
const menuTargetAppName = document.getElementById("menu-target-app-name")!;
const failurePanel = document.getElementById("failure-panel")!;
const failureMessage = document.getElementById("failure-message")!;
const failureOpenRecording = document.getElementById("failure-open-recording") as HTMLButtonElement;
const failureDismiss = document.getElementById("failure-dismiss") as HTMLButtonElement;
const menuImportAudio = document.getElementById("menu-import-audio") as HTMLButtonElement;
const menuOpenRecordings = document.getElementById("menu-open-recordings") as HTMLButtonElement;
const audioFileInput = document.getElementById("audio-file-input") as HTMLInputElement;
const recentRecordings = document.getElementById("recent-recordings")!;
let pasteAccessRequested = false;

let hasOpenaiApiKey = false;
let transcriptionModel: TranscriptionModel = DEFAULT_TRANSCRIPTION_MODEL;
let settingsLoaded = false;
let storedKeyPreview = "";
let showingStoredKeyPreview = false;
let keyInputDirty = false;
let keySaveInFlight = false;
let keyRevealInFlight = false;
let promptDirty = false;
let promptSaveInFlight = false;
let modelSaveInFlight = false;
let quillSettings: AppSettings | null = null;
let quillSaveInFlight = false;
let quillSaveError: string | null = null;
let showIdlePill = true;
let showIdlePillSaveInFlight = false;
let holdToTalkSaveInFlight = false;
let recordingShortcut = defaultRecordingShortcut;
let recordingShortcutSaveInFlight = false;
let recordingShortcutError: string | null = null;
let manualOverlayReveal = false;
let widgetHovered = false;
let expandTimeoutId: number | null = null;
let collapseTimeoutId: number | null = null;
let lastAppliedLayout: { width: number; height: number; anchorOffsetY: number; visible: boolean } | null = null;
let overlayLayoutQueue: Promise<void> = Promise.resolve();

function logDebug(message: string) {
  const timestamp = new Date().toISOString();
  void invoke("debug_log", { message: `${timestamp} ${message}` }).catch(() => {});
}

function isWidgetExpanded() {
  return widgetHovered || menuOpen || state !== "idle";
}

function getWidgetSize() {
  if (isWidgetExpanded()) {
    return { width: EXPANDED_WIDTH, height: EXPANDED_HEIGHT };
  }
  return { width: COLLAPSED_WIDTH, height: COLLAPSED_HEIGHT };
}

const widgetShell = document.getElementById("widget-shell")!;

function syncWidgetFrame() {
  const widgetSize = getWidgetSize();
  widgetShell.style.setProperty("--widget-width", `${widgetSize.width}px`);
  widgetShell.style.setProperty("--widget-height", `${widgetSize.height}px`);
  widget.dataset.expanded = String(isWidgetExpanded());
  widget.dataset.hovered = String(widgetHovered);
  widget.dataset.menuOpen = String(menuOpen);
  widget.dataset.hasLastTranscript = String(lastTranscript.length > 0);
}

function syncApiKeyBubble() {
  const show = settingsLoaded && needsApiKey(transcriptionModel, hasOpenaiApiKey) && state === "idle";
  if (show) {
    apiKeyBubble.classList.remove("hidden");
  } else {
    apiKeyBubble.classList.add("hidden");
  }
}

function setWidgetHovered(next: boolean) {
  if (widgetHovered === next) return;
  widgetHovered = next;
  widget.dataset.hovered = String(next);
  void applyOverlayLayout();
}

/** The open picker and the active revert slot, which grow the window like the menu. */
function quillOverlayRects() {
  const rects: DOMRect[] = [];
  if (quillPicker.isOpen) rects.push(quillPickerElement.getBoundingClientRect());
  if (quillRevertElement.dataset.active === "true") rects.push(quillRevertElement.getBoundingClientRect());
  return rects;
}

function getWindowLayout() {
  const menuVisible = menuOpen && !micMenu.classList.contains("hidden");
  const menuRect = menuVisible ? micMenu.getBoundingClientRect() : undefined;
  const quillRects = quillOverlayRects();
  const overlayRects = [...(menuRect ? [menuRect] : []), ...quillRects];
  const layout = getOverlayLayout({
    expanded: isWidgetExpanded(),
    settingsLoaded,
    hasOpenaiApiKey: !needsApiKey(transcriptionModel, hasOpenaiApiKey) || state !== "idle",
    menuVisible: menuVisible || quillRects.length > 0,
    menuWidth: overlayRects.length ? Math.max(...overlayRects.map((rect) => rect.width)) : undefined,
    menuHeight: overlayRects.length ? Math.max(...overlayRects.map((rect) => rect.height)) : undefined,
  });
  return {
    ...layout,
    visible: isRecordingWindowVisible({
      showIdlePill,
      active: state !== "idle",
      menuOpen,
      manuallyRevealed: manualOverlayReveal,
      starting: recordingStartupVisible,
    }) || quillRects.length > 0,
  };
}

async function applyOverlayLayout(force = false) {
  syncWidgetFrame();
  const layout = getWindowLayout();
  if (
    !force &&
    lastAppliedLayout?.width === layout.width &&
    lastAppliedLayout?.height === layout.height &&
    lastAppliedLayout?.anchorOffsetY === layout.anchorOffsetY &&
    lastAppliedLayout?.visible === layout.visible
  ) {
    return;
  }

  lastAppliedLayout = layout;
  const apply = overlayLayoutQueue
    .catch(() => {})
    .then(() => waitForStartupStep("Applying recording window layout", () => invoke<void>("overlay_apply_layout", {
      width: layout.width,
      height: layout.height,
      anchorOffsetY: layout.anchorOffsetY,
      visible: layout.visible,
    })));
  overlayLayoutQueue = apply;
  await apply;
}

// ---------------------------------------------------------------------------
// Mic devices
// ---------------------------------------------------------------------------
async function refreshMicDevices() {
  try {
    const devices = await navigator.mediaDevices.enumerateDevices();
    micDevices = devices
      .filter((d) => d.kind === "audioinput")
      .map((d, i) => ({ id: d.deviceId, label: d.label || `Microphone ${i + 1}` }));
    renderMicList();
  } catch (error) {
    logDebug(`microphone enumeration failed=${errorMessage(error)}`);
  }
}

function renderMicList() {
  micList.innerHTML = "";
  const all: MicDevice[] = [{ id: "default", label: "System default" }, ...micDevices];
  for (const dev of all) {
    const el = document.createElement("div");
    el.className = `menu-item${dev.id === selectedMicId ? " selected" : ""}`;
    el.textContent = dev.label;
    el.addEventListener("click", () => {
      selectedMicId = dev.id;
      renderMicList();
      closeMenu();
    });
    micList.appendChild(el);
  }
}

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------
function toggleMenu() {
  void (micMenu.classList.contains("hidden") ? openMenu() : collapseOverlay());
}

async function openMenu() {
  menuOpen = true;
  micMenu.classList.remove("hidden");
  startAppPoll();
  void refreshRecordings();
  await applyOverlayLayout();
}

function closeMenu() {
  void collapseOverlay();
}

async function collapseOverlay() {
  menuOpen = false;
  if (expandTimeoutId !== null) {
    clearTimeout(expandTimeoutId);
    expandTimeoutId = null;
  }
  if (collapseTimeoutId !== null) {
    clearTimeout(collapseTimeoutId);
    collapseTimeoutId = null;
  }
  setWidgetHovered(false);
  stopAppPoll();
  micMenu.classList.add("hidden");
  await applyOverlayLayout();
}

btnDropdown.addEventListener("click", (e) => {
  e.stopPropagation();
  toggleMenu();
});

async function openSettingsForApiKey() {
  if (!menuOpen) {
    await openMenu();
  }
  requestAnimationFrame(() => {
    menuKeyInput.focus();
  });
}

apiKeyBubble.addEventListener("click", (e) => {
  e.stopPropagation();
  e.preventDefault();
  void openSettingsForApiKey();
});

apiKeyBubble.addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    void openSettingsForApiKey();
  }
});

function handleWidgetClick(e: MouseEvent) {
  if (quillPicker.isOpen) {
    quillPicker.click();
    e.stopPropagation();
    e.preventDefault();
    return;
  }
  if (menuOpen) {
    closeMenu();
    e.stopPropagation();
    e.preventDefault();
    return;
  }

  if (state === "recording" || state === "transcribing") {
    e.stopPropagation();
    e.preventDefault();
    if (state === "recording") {
      void stopRecording();
    } else {
      void cancelTranscription();
    }
    return;
  }
  if (btnMic.contains(e.target as Node) || btnDropdown.contains(e.target as Node)) return;
  void toggleRecording();
}

function handleWidgetKeydown(e: KeyboardEvent) {
  if (state === "recording" || state === "transcribing") {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (state === "recording") void stopRecording();
      else {
        void cancelTranscription();
      }
    }
    return;
  }
  if ((e.key === "Enter" || e.key === " ") && !btnMic.contains(e.target as Node) && !btnDropdown.contains(e.target as Node)) {
    e.preventDefault();
    void toggleRecording();
  }
}

widget.addEventListener("click", handleWidgetClick, true);

widget.addEventListener("keydown", handleWidgetKeydown);

function handleWidgetPointerEnter() {
  if (collapseTimeoutId !== null) {
    clearTimeout(collapseTimeoutId);
    collapseTimeoutId = null;
  }
  if (expandTimeoutId !== null || widgetHovered) return;
  expandTimeoutId = window.setTimeout(() => {
    expandTimeoutId = null;
    setWidgetHovered(true);
  }, HOVER_EXPAND_DELAY_MS);
}

function handleWidgetPointerLeave(event: PointerEvent) {
  if (event.relatedTarget instanceof Node && widget.contains(event.relatedTarget)) {
    return;
  }
  if (expandTimeoutId !== null) {
    clearTimeout(expandTimeoutId);
    expandTimeoutId = null;
  }
  if (collapseTimeoutId !== null) {
    clearTimeout(collapseTimeoutId);
    collapseTimeoutId = null;
  }
  if (!widgetHovered) return;
  collapseTimeoutId = window.setTimeout(() => {
    collapseTimeoutId = null;
    setWidgetHovered(false);
  }, HOVER_COLLAPSE_GRACE_MS);
}

widget.addEventListener("pointerenter", handleWidgetPointerEnter);
widget.addEventListener("pointerleave", handleWidgetPointerLeave);

document.addEventListener("click", (e) => {
  if (!micMenu.contains(e.target as Node) && !micMenu.classList.contains("hidden")) {
    closeMenu();
  }
});

window.addEventListener("blur", () => {
  if (menuOpen) closeMenu();
});

// ---------------------------------------------------------------------------
// Toast
// ---------------------------------------------------------------------------
let toastTimer: number | null = null;
function showToast(msg: string, durationMs = 3000, role?: QuillNoticeRole) {
  if (role) toast.setAttribute("role", role);
  else toast.removeAttribute("role");
  toast.textContent = msg;
  toast.classList.remove("hidden");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => toast.classList.add("hidden"), durationMs);
}

// ---------------------------------------------------------------------------
// Quill: hotkey, picker, revert slot
// ---------------------------------------------------------------------------
const quillInvoke = (cmd: string, args?: Record<string, unknown>) => invoke<unknown>(cmd, args);

function showQuillNotice(text: string, role: QuillNoticeRole) {
  showToast(text, quillNoticeDurationMs(role), role);
}

function announceQuill(text: string) {
  quillLive.textContent = text;
}

function renderQuillPicker(view: QuillPickerView) {
  quillPickerElement.classList.toggle("hidden", !view.open);
  quillPickerElement.innerHTML = view.open
    ? view.rows.map((row, index) => `
      <div class="quill-picker-row" role="option" id="quill-picker-row-${index}" aria-selected="${row.selected}">
        <span class="quill-picker-index" aria-hidden="true">${(index + 1) % 10}</span>
        <span class="quill-picker-name">${escapeHtml(row.name)}</span>
        ${row.isDefault ? '<span class="quill-picker-check" aria-hidden="true">✓</span><span class="visually-hidden">default</span>' : "<span></span>"}
        ${row.selected ? `<span class="quill-picker-description">${escapeHtml(view.description)}</span>` : ""}
      </div>`).join("")
    : "";
  const selected = view.rows.findIndex((row) => row.selected);
  if (view.open && selected >= 0) quillPickerElement.setAttribute("aria-activedescendant", `quill-picker-row-${selected}`);
  else quillPickerElement.removeAttribute("aria-activedescendant");
  void applyOverlayLayout();
}

function renderQuillRevert(view: QuillRevertView) {
  const wasActive = quillRevertElement.dataset.active === "true";
  quillRevertElement.dataset.active = String(view.visible);
  quillRevertText.textContent = view.label;
  quillRevertCountdown.textContent = view.countdown;
  if (wasActive !== view.visible) void applyOverlayLayout();
}

const quillCatalog = new QuillCatalog(quillInvoke);
const quillPicker = new QuillPicker({ announce: announceQuill, render: renderQuillPicker });
const quillSlot = new QuillRevertSlot({
  invoke: quillInvoke,
  notify: showQuillNotice,
  announce: announceQuill,
  render: renderQuillRevert,
  log: (line) => logDebug(line),
});
const quillHotkey = new QuillHotkey({
  invoke: quillInvoke,
  catalog: quillCatalog,
  picker: quillPicker,
  storedDefault: () => quillSettings?.quillDefaultMode ?? "",
  notify: showQuillNotice,
  onRewritten: (rewrite) => quillSlot.open(rewrite),
  hookReady: () => hotkeySlotAllowed(quillSettings),
  readSettings: () => invoke<AppSettings>("app_get_settings"),
  revertArmed: (rewriteId) => invoke<boolean>("quill_revert_armed", { rewriteId }),
  log: (line) => logDebug(line),
});

quillRevertButton.addEventListener("click", (e) => {
  e.stopPropagation();
  void quillSlot.revert();
});

/** Rewrites the finished dictation while the widget shows `rewriting`; null keeps the raw text. */
async function rewriteFinishedDictation(text: string) {
  setState("rewriting");
  try {
    return await rewriteDictation({
      invoke: quillInvoke,
      text,
      notify: showQuillNotice,
      announce: announceQuill,
      log: (line) => logDebug(line),
    });
  } finally {
    setState("transcribing");
  }
}

/** Ends the clipboard hold, reports the delivery and opens the revert slot. */
async function settleQuillDictation(delivery: { inserted: boolean } | null) {
  const rewrite = quillDictationPending;
  if (!rewrite) return;
  quillDictationPending = null;
  await settleDictationRewrite({
    invoke: quillInvoke,
    rewriteId: rewrite.rewriteId,
    delivered: dictationDelivered(delivery),
    pasted: rewrite.text,
    slot: quillSlot,
    name: modeName(quillCatalog.modes, rewrite.mode),
    log: (line) => logDebug(line),
  });
}

// ---------------------------------------------------------------------------
// Visual state
// ---------------------------------------------------------------------------
function setState(next: WidgetState) {
  logDebug(`setState ${state} -> ${next}`);
  state = next;
  widget.dataset.state = next;
  syncApiKeyBubble();
  void applyOverlayLayout();
}

// ---------------------------------------------------------------------------
// Meter — sound-visualizer (recording) + idle bars (transcribing)
// ---------------------------------------------------------------------------
const VOLUME_BAR_COUNT = 7;

function setWaveBarLevels(levels: number[]) {
  waveBars.forEach((bar, i) => {
    const level = levels[i] ?? 0.15;
    const clamped = Math.max(0.15, Math.min(level, 1));
    bar.style.setProperty("--wave-level", clamped.toFixed(3));
  });
}

// ---------------------------------------------------------------------------
// Base64 helper
// ---------------------------------------------------------------------------
function toBase64(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

function fromBase64(value: string): ArrayBuffer {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index++) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes.buffer;
}

// ---------------------------------------------------------------------------
// Volume monitor — show wobbly line when mic is silent during recording
// ---------------------------------------------------------------------------
function startVolumeMonitor(stream: MediaStream) {
  const ctx = new AudioContext();
  volumeMonitorDisconnect = () => {
    void ctx.close().catch((error) => logDebug(`volume monitor close failed=${errorMessage(error)}`));
    volumeMonitorDisconnect = null;
  };
  const analyser = ctx.createAnalyser();
  analyser.fftSize = 256;
  const source = ctx.createMediaStreamSource(stream);
  source.connect(analyser);
  const data = new Uint8Array(analyser.fftSize);

  function tick() {
    if (state !== "recording") return;
    analyser.getByteTimeDomainData(data);
    let min = 255, max = 0;
    for (let i = 0; i < data.length; i++) {
      if (data[i] < min) min = data[i];
      if (data[i] > max) max = data[i];
    }
    const range = max - min;
    widget.dataset.silent = range < SILENT_THRESHOLD ? "true" : "false";
    volumeMonitorFrameId = requestAnimationFrame(tick);
  }
  volumeMonitorFrameId = requestAnimationFrame(tick);

  volumeMonitorDisconnect = () => {
    if (volumeMonitorFrameId !== null) {
      cancelAnimationFrame(volumeMonitorFrameId);
      volumeMonitorFrameId = null;
    }
    source.disconnect();
    analyser.disconnect();
    void ctx.close().catch((error) => logDebug(`volume monitor close failed=${errorMessage(error)}`));
    volumeMonitorDisconnect = null;
    delete widget.dataset.silent;
  };
}

function stopVolumeMonitor() {
  try {
    volumeMonitorDisconnect?.();
  } catch (error) {
    logDebug(`volume monitor stop failed=${errorMessage(error)}`);
  }
  volumeMonitorDisconnect = null;
}

function stopRecordingVisualizer() {
  const visualizer = soundVisualizer;
  soundVisualizer = null;
  try {
    visualizer?.stop();
  } catch (error) {
    logDebug(`visualizer stop failed=${errorMessage(error)}`);
  }
}

function startRecordingFeedback(stream: MediaStream) {
  try {
    logStartupStage("visualizer create begin");
    soundVisualizer = continuousVisualizer(stream, meterCanvas, {
      strokeColor: "#9ef0c9",
      rectWidth: 3,
      slices: 20,
    });
    logStartupStage("visualizer create complete");
    logStartupStage("visualizer start begin");
    soundVisualizer.start();
    logStartupStage("visualizer start complete");
  } catch (error) {
    stopRecordingVisualizer();
    logDebug(`optional recording visualizer failed=${errorMessage(error)}`);
  }
  try {
    logStartupStage("volume monitor begin");
    startVolumeMonitor(stream);
    logStartupStage("volume monitor complete");
  } catch (error) {
    stopVolumeMonitor();
    logDebug(`optional recording volume monitor failed=${errorMessage(error)}`);
  }
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------
function resampleToPcm16(input: Float32Array, sourceRate: number): ArrayBuffer {
  const targetRate = 24_000;
  const outputLength = Math.max(1, Math.round(input.length * targetRate / sourceRate));
  const buffer = new ArrayBuffer(outputLength * 2);
  const view = new DataView(buffer);
  for (let i = 0; i < outputLength; i++) {
    const sourceIndex = Math.min(input.length - 1, Math.floor(i * sourceRate / targetRate));
    const sample = Math.max(-1, Math.min(1, input[sourceIndex]));
    view.setInt16(i * 2, sample < 0 ? sample * 0x8000 : sample * 0x7fff, true);
  }
  return buffer;
}

function queueAudioChunk(buffer: ArrayBuffer) {
  capturedPcmBytes += buffer.byteLength;
  pendingAudioParts.push(new Uint8Array(buffer));
  pendingAudioBytes += buffer.byteLength;
  if (pendingAudioBytes >= AUDIO_BATCH_BYTES) flushAudioBatch();
}

function flushAudioBatch() {
  if (pendingAudioBytes === 0) return;
  const combined = new Uint8Array(pendingAudioBytes);
  let offset = 0;
  for (const part of pendingAudioParts) {
    combined.set(part, offset);
    offset += part.byteLength;
  }
  pendingAudioParts = [];
  pendingAudioBytes = 0;

  const backupQueue = backupAudioQueue;
  if (backupQueue) {
    backupQueue.append(combined.buffer);
    backupTail = backupQueue.drain().catch((error) => {
      if (backupAudioQueue === backupQueue) backupError ??= error;
    });
  }
  audioQueue.append(combined.buffer);
}

function errorMessage(error: unknown, fallback = "Transcription failed"): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  const message = String(error ?? "").trim();
  return message || fallback;
}

async function finishCurrentRecording(status: string, transcript: string, error: string) {
  const recordingId = currentRecordingId;
  if (!recordingId) return;
  currentRecordingId = null;
  try {
    await invoke("recording_finish", {
      recordingId,
      status,
      transcript,
      error,
    });
  } catch (finishError) {
    logDebug(`recording metadata update failed=${errorMessage(finishError)}`);
  }
  void refreshRecordings();
}

async function handleTranscriptionFailure(error: unknown) {
  const signal = currentRecordingCancellation?.signal;
  if (signal?.aborted) return;
  if (pcmAudioContext) await stopPcmCapture();
  flushAudioBatch();
  await backupTail;
  if (signal?.aborted) return;
  const message = errorMessage(error);
  const recordingId = currentRecordingId;
  if (recordingId) {
    lastFailedRecordingId = recordingId;
    await finishCurrentRecording("failed", "", message);
  }
  if (signal?.aborted) return;
  failureMessage.textContent = recordingId
    ? `${message} The raw audio was saved locally and can be opened below.`
    : message;
  failureOpenRecording.disabled = !lastFailedRecordingId;
  failurePanel.classList.remove("hidden");
  showToast(message, 8000);
  await openMenu();
  await applyOverlayLayout(true);
}

function formatRecordingSize(bytes: number): string {
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

async function refreshRecordings() {
  try {
    const recordings = await invoke<RecordingMetadata[]>("recording_list");
    recentRecordings.replaceChildren();
    for (const recording of recordings.slice(0, 4)) {
      const transcript = recording.transcript.trim();
      const card = document.createElement("div");
      card.className = "recording-card";
      const audioButton = document.createElement("button");
      audioButton.type = "button";
      audioButton.className = "recording-row";
      const when = new Date(recording.createdAtMs).toLocaleString([], {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      });
      const main = document.createElement("span");
      main.className = "recording-row-main";
      main.textContent = recording.name;
      const meta = document.createElement("span");
      meta.className = "recording-row-meta";
      meta.textContent = `${when} | ${formatRecordingSize(recording.bytes)} | ${recording.status}`;
      audioButton.append(main, meta);
      audioButton.title = "Open raw audio";
      audioButton.addEventListener("click", () => {
        void invoke("recording_open", { recordingId: recording.id }).catch((error) => {
          showToast(errorMessage(error, "Could not open recording"));
        });
      });

      card.appendChild(audioButton);
      if (transcript) {
        // A single transcript surface serves as both the collapsed preview
        // (CSS line-clamp + fade) and the fully revealed text. Expanding does
        // not render a second copy — the disclosure toggle only unclamps it.
        const panel = document.createElement("div");
        panel.className = "recording-transcript";
        panel.dataset.expanded = "false";
        panel.dataset.clamped = "false";

        const surface = document.createElement("div");
        surface.className = "recording-transcript-surface";

        const transcriptText = document.createElement("div");
        transcriptText.id = `recording-transcript-${recording.id}`;
        transcriptText.className = "recording-transcript-text";
        transcriptText.textContent = transcript;

        const copyButton = document.createElement("button");
        copyButton.type = "button";
        copyButton.className = "recording-copy";
        copyButton.title = "Copy transcript";
        copyButton.setAttribute("aria-label", `Copy transcript for ${recording.name}`);
        copyButton.innerHTML = `
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <rect x="9" y="9" width="11" height="11" rx="2.4" />
            <path d="M6 15H5.4A1.6 1.6 0 0 1 4 13.4V5.4A1.6 1.6 0 0 1 5.4 4h8A1.6 1.6 0 0 1 15 5.4V6" />
          </svg>`;
        copyButton.addEventListener("click", (event) => {
          event.stopPropagation();
          void invoke("copy_to_clipboard", { text: transcript })
            .then(() => showToast("Transcript copied to clipboard"))
            .catch((error) => showToast(errorMessage(error, "Could not copy transcript")));
        });
        surface.append(transcriptText, copyButton);

        const transcriptToggle = document.createElement("button");
        transcriptToggle.type = "button";
        transcriptToggle.className = "recording-transcript-toggle";
        transcriptToggle.setAttribute("aria-expanded", "false");
        transcriptToggle.setAttribute("aria-controls", transcriptText.id);
        transcriptToggle.title = "Show full transcript";
        transcriptToggle.hidden = true;
        const toggleLabel = document.createElement("span");
        toggleLabel.className = "recording-transcript-toggle-label";
        toggleLabel.textContent = "Show more";
        transcriptToggle.append(toggleLabel);
        transcriptToggle.insertAdjacentHTML(
          "beforeend",
          `<svg class="recording-transcript-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7.5 10.5 12 15 16.5 10.5" /></svg>`,
        );
        transcriptToggle.addEventListener("click", () => {
          const expanded = panel.dataset.expanded === "true";
          const next = !expanded;
          panel.dataset.expanded = String(next);
          transcriptToggle.setAttribute("aria-expanded", String(next));
          toggleLabel.textContent = next ? "Show less" : "Show more";
          transcriptToggle.title = next ? "Collapse transcript" : "Show full transcript";
          if (menuOpen) void applyOverlayLayout(true);
        });

        panel.append(surface, transcriptToggle);
        card.appendChild(panel);

        // Only offer the disclosure when the preview is actually truncated.
        // Measured after layout so short transcripts read as plain text.
        requestAnimationFrame(() => {
          const clamped = transcriptText.scrollHeight - transcriptText.clientHeight > 2;
          panel.dataset.clamped = String(clamped);
          transcriptToggle.hidden = !clamped;
          if (clamped && menuOpen) void applyOverlayLayout(true);
        });
      } else {
        const transcribeButton = document.createElement("button");
        transcribeButton.type = "button";
        transcribeButton.className = "recording-transcribe";
        transcribeButton.textContent = "Transcribe";
        transcribeButton.title = `Transcribe ${recording.name}`;
        transcribeButton.setAttribute("aria-label", `Transcribe ${recording.name}`);
        transcribeButton.disabled = state !== "idle" || recording.id === currentRecordingId;
        transcribeButton.addEventListener("click", () => {
          void retranscribeRecording(recording);
        });
        card.appendChild(transcribeButton);
      }
      recentRecordings.appendChild(card);
    }
    if (recordings.length === 0) {
      const empty = document.createElement("div");
      empty.className = "recordings-empty";
      empty.textContent = "No saved recordings yet";
      recentRecordings.appendChild(empty);
    }
    if (menuOpen) await applyOverlayLayout(true);
  } catch (error) {
    logDebug(`recording list failed=${errorMessage(error)}`);
  }
}

failureOpenRecording.addEventListener("click", () => {
  if (!lastFailedRecordingId) return;
  void invoke("recording_open", { recordingId: lastFailedRecordingId }).catch((error) => {
    showToast(errorMessage(error, "Could not open recording"));
  });
});

failureDismiss.addEventListener("click", () => {
  failurePanel.classList.add("hidden");
  void applyOverlayLayout(true);
});

menuOpenRecordings.addEventListener("click", () => {
  void invoke("recordings_open_folder").catch((error) => {
    showToast(errorMessage(error, "Could not open recordings folder"));
  });
});

menuImportAudio.addEventListener("click", () => audioFileInput.click());

audioFileInput.addEventListener("change", () => {
  const file = audioFileInput.files?.[0];
  audioFileInput.value = "";
  if (file) void transcribeImportedFile(file);
});

async function startPcmCapture(stream: MediaStream, signal: AbortSignal, startupStartedAt: number) {
  const context = new AudioContext();
  logStartupStage(`audio context resume begin context=${context.state}`);
  try {
    await cancellableRecordingStep(signal, () => resumeRecordingAudioContext(context));
    throwIfRecordingCancelled(signal);
  } catch (error) {
    void context.close().catch(() => {});
    throw error;
  }
  logStartupStage(`audio context resume complete context=${context.state}`);
  pcmAudioContext = context;
  pcmSource = context.createMediaStreamSource(stream);
  pcmProcessor = context.createScriptProcessor(4096, 1, 1);
  pcmSilentGain = context.createGain();
  pcmSilentGain.gain.value = 0;
  const sourceRate = context.sampleRate;
  const connectedAt = performance.now();
  let firstPcm = true;
  pcmProcessor.onaudioprocess = (event) => {
    if (signal.aborted || pcmAudioContext !== context) return;
    if (firstPcm) {
      firstPcm = false;
      logStartupStage(`first PCM startup_ms=${Math.round(performance.now() - startupStartedAt)} graph_ms=${Math.round(performance.now() - connectedAt)} source_rate=${sourceRate} frames=${event.inputBuffer.length}`);
    }
    queueAudioChunk(resampleToPcm16(event.inputBuffer.getChannelData(0), sourceRate));
  };
  pcmSource.connect(pcmProcessor);
  pcmProcessor.connect(pcmSilentGain);
  pcmSilentGain.connect(context.destination);
  logStartupStage(`PCM graph connected startup_ms=${Math.round(performance.now() - startupStartedAt)}`);
  recordingMimeType = "audio/pcm;rate=24000";
}

async function stopPcmCapture() {
  if (pcmProcessor) pcmProcessor.onaudioprocess = null;
  pcmSource?.disconnect();
  pcmProcessor?.disconnect();
  pcmSilentGain?.disconnect();
  pcmSource = null;
  pcmProcessor = null;
  pcmSilentGain = null;
  const context = pcmAudioContext;
  pcmAudioContext = null;
  micStream?.getTracks().forEach((track) => track.stop());
  micStream = null;
  isRecording = false;
  if (context) {
    await waitForStartupStep("Closing audio capture", () => context.close(), AUDIO_CONTEXT_STARTUP_TIMEOUT_MS)
      .catch((error) => logDebug(`audio context close failed=${errorMessage(error)}`));
  }
}

function logStartupStage(stage: string) {
  logDebug(`recording startup stage=${stage} visibility=${document.visibilityState} focused=${document.hasFocus()}`);
}

async function startRecording() {
  const startupStartedAt = performance.now();
  logDebug(
    `startRecording enter state=${state} held=${pressToTalkHeld} starting=${pressToTalkStarting}`,
  );
  if (state !== "idle") return;
  quillPicker.dismiss("picker_recording_close_failed");
  const cancellation = new AbortController();
  currentRecordingCancellation = cancellation;
  recordingCancellationCleanup = null;
  recordingAttemptBackupId = null;
  const signal = cancellation.signal;
  const attemptBackupQueue = new SessionAudioQueue((recordingId, buffer) => invoke<void>("recording_append_chunk", {
    recordingId,
    chunkBase64: toBase64(buffer),
  }));
  backupAudioQueue = attemptBackupQueue;
  backupTail = Promise.resolve();
  backupError = null;
  capturedPcmBytes = 0;
  pendingAudioParts = [];
  pendingAudioBytes = 0;
  audioQueue.reset();
  currentRecordingId = null;
  sessionFailure = "";
  pollFailureCount = 0;
  shouldPasteFinalTranscript = true;
  let settleCapture!: () => void;
  const captureSettled = new Promise<void>((resolve) => { settleCapture = resolve; });
  const discardedBackups = new Set<string>();
  const discardBackup = (recording: { id: string }) => {
    if (discardedBackups.has(recording.id)) return;
    discardedBackups.add(recording.id);
    void captureSettled.then(async () => {
      attemptBackupQueue.attachSession(recording.id);
      await attemptBackupQueue.drain().catch((error) => {
        logDebug(`expired recording backup audio failed=${errorMessage(error)}`);
      });
      await invoke("recording_finish", {
        recordingId: recording.id,
        status: signal.aborted ? "cancelled" : "failed",
        transcript: "",
        error: signal.aborted ? "Cancelled by keyboard chord" : "Recording startup failed before backup was attached",
      });
      void refreshRecordings();
    }).catch((error) => logDebug(`expired recording backup update failed=${errorMessage(error)}`));
  };

  try {
    logStartupStage("paste target begin");
    await cancellableRecordingStep(signal, () => waitForStartupStep("Capturing paste target", () => invoke("capture_paste_target")));
    logStartupStage("paste target complete");
    const settings = await cancellableRecordingStep(signal, () => waitForStartupStep("Loading recording settings", () => invoke<AppSettings>("app_get_settings")));
    logStartupStage("settings ready");
    quillCleanupAtStart = dictationCleanupEffective(settings);
    if (needsApiKey(settings.transcriptionModel, settings.hasOpenaiApiKey)) {
      showToast("Set your OpenAI API key first (click arrow)");
      setState("idle");
      return;
    }

    const constraints: MediaStreamConstraints = {
      audio: selectedMicId === "default" ? true : { deviceId: { exact: selectedMicId } },
    };
    recordingStartupVisible = true;
    const result = await prepareRecordingCapture({
      acquireMicrophone: async () => {
        micStream = await cancellableRecordingStep(signal, () => acquireVisibleMicrophone({
          revealWindow: () => applyOverlayLayout(true),
          document,
          acquire: () => {
            throwIfRecordingCancelled(signal);
            return navigator.mediaDevices.getUserMedia(constraints);
          },
          discard: (stream) => stream.getTracks().forEach((track) => track.stop()),
          onStage: logStartupStage,
        }), (stream) => stream.getTracks().forEach((track) => track.stop()));
        throwIfRecordingCancelled(signal);
        void refreshMicDevices();
        return micStream;
      },
      createBackup: async () => {
        logStartupStage("recording backup begin");
        const backup = await cancellableRecordingStep(signal, () => waitForStartupStep("Creating recording backup", () => invoke<{ id: string }>("recording_begin", {
          source: "microphone",
          name: `Recording ${new Date().toLocaleString()}`,
        }), undefined, discardBackup), discardBackup);
        logStartupStage("recording backup ready");
        return backup;
      },
      onBackupReady: (backup) => {
        throwIfRecordingCancelled(signal);
        currentRecordingId = backup.id;
        recordingAttemptBackupId = backup.id;
        attemptBackupQueue.attachSession(backup.id);
        backupTail = attemptBackupQueue.drain().catch((error) => {
          if (backupAudioQueue === attemptBackupQueue) backupError ??= error;
        });
      },
      discardBackup,
      startCapture: async (stream) => {
        throwIfRecordingCancelled(signal);
        await startPcmCapture(stream, signal, startupStartedAt);
        isRecording = true;
        return stream;
      },
      onCaptureStarted: async (stream) => {
        throwIfRecordingCancelled(signal);
        setState("recording");
        recordingStartupVisible = false;
        widget.dataset.silent = "true";
        startRecordingFeedback(stream);
        logDebug(`capture ready startup_ms=${Math.round(performance.now() - startupStartedAt)}`);

        // A hold shortcut may be released, or a toggle shortcut may be pressed
        // again, while microphone permission/capture is still starting.
        if (
          (pressToTalkStarting && !pressToTalkHeld) ||
          toggleStopRequestedDuringStart
        ) {
          await stopRecording();
        }
      },
      startSession: () => {
        throwIfRecordingCancelled(signal);
        logStartupStage("worker session begin");
        return invoke<{ session_id: string }>("worker_start_session", {
          profileId: "default",
        });
      },
    });
    currentSessionId = result.session_id;
    if (signal.aborted) {
      await finishModifierCancelledRecording();
      return;
    }
    audioQueue.attachSession(currentSessionId);
    finalTranscript = "";
    liveTranscript = "";
    streamTextToTarget = getModelCapabilities(settings.transcriptionModel).streamsText;
    pastedTranscript = "";
    targetWriteQueue = Promise.resolve();
    startPolling();
    logDebug(`session ready startup_ms=${Math.round(performance.now() - startupStartedAt)}`);

    if (state === "transcribing") {
      await finalizeRecording();
      return;
    }
    logDebug(`startRecording ready session=${currentSessionId ?? "none"} held=${pressToTalkHeld}`);
  } catch (err) {
    if (signal.aborted) {
      await finishModifierCancelledRecording();
      return;
    }
    logDebug(`startRecording error=${err instanceof Error ? err.message : String(err)}`);
    if (pcmAudioContext) await stopPcmCapture();
    micStream?.getTracks().forEach((track) => track.stop());
    micStream = null;
    flushAudioBatch();
    await backupTail;
    await waitForStartupStep("Showing recording failure", () => handleTranscriptionFailure(err))
      .catch((error) => logDebug(`recording failure presentation failed=${errorMessage(error)}`));
    cleanup();
    setState("idle");
  } finally {
    recordingStartupVisible = false;
    settleCapture();
    void applyOverlayLayout();
  }
}

async function stopRecording() {
  logDebug(`stopRecording enter state=${state} session=${currentSessionId ?? "none"}`);
  if (state !== "recording") return;
  setState("transcribing");
  stopRecordingVisualizer();
  stopVolumeMonitor();
  setWaveBarLevels(Array(VOLUME_BAR_COUNT).fill(0.5));

  if (pcmAudioContext) {
    await stopPcmCapture();
    flushAudioBatch();
  } else if (mediaRecorder && mediaRecorder.state !== "inactive") {
    await new Promise<void>((resolve) => {
      mediaRecorder!.addEventListener("stop", () => resolve(), { once: true });
      mediaRecorder!.stop();
    });
  }

  if (currentSessionId) await finalizeRecording();
}

async function finalizeRecording() {
  if (!currentSessionId) return;
  const signal = currentRecordingCancellation?.signal;
  if (signal?.aborted) return;
  try {
    await audioQueue.drain();
    await backupTail;
    if (signal?.aborted) return;
    if (backupError) throw backupError;
    await invoke("worker_finalize_session_audio", {
      sessionId: currentSessionId,
      mimeType: recordingMimeType,
    });
    logDebug(`stopRecording finalized session=${currentSessionId}`);
  } catch (err) {
    if (signal?.aborted) return;
    logDebug(`stopRecording error=${err instanceof Error ? err.message : String(err)}`);
    await handleTranscriptionFailure(err);
    if (signal?.aborted) return;
    cleanup();
    setState("idle");
  }
}

async function finishModifierCancelledRecording() {
  if (recordingCancellationCleanup) return recordingCancellationCleanup;
  const cancellation = currentRecordingCancellation;
  const sessionId = currentSessionId;
  const recordingId = recordingAttemptBackupId ?? currentRecordingId;
  recordingCancellationCleanup = completeRecordingCancellation({
    stopCapture: async () => {
      if (currentRecordingCancellation !== cancellation) return;
      stopRecordingVisualizer();
      stopVolumeMonitor();
      if (pcmAudioContext) await stopPcmCapture();
      micStream?.getTracks().forEach((track) => track.stop());
      micStream = null;
    },
    flushBackup: () => waitForStartupStep("Saving cancelled audio backup", async () => {
      if (currentRecordingCancellation !== cancellation) return;
      flushAudioBatch();
      await backupTail;
    }),
    stopSession: async () => {
      if (sessionId) {
        await waitForStartupStep("Stopping cancelled recording", () => invoke("worker_stop_session", { sessionId }))
          .catch((error) => logDebug(`cancelled session stop failed=${errorMessage(error)}`));
      }
    },
    markCancelled: async () => {
      if (!recordingId) return;
      if (currentRecordingId === recordingId) currentRecordingId = null;
      await waitForStartupStep("Saving cancelled recording", () => invoke("recording_finish", {
        recordingId,
        status: "cancelled",
        transcript: "",
        error: "Cancelled by keyboard chord",
      }));
      void refreshRecordings();
    },
    cleanup: () => {
      if (currentRecordingCancellation !== cancellation) return;
      cleanup();
      setState("idle");
    },
  }).catch((error) => {
    logDebug(`recording cancellation cleanup failed=${errorMessage(error)}`);
  });
  await recordingCancellationCleanup;
}

async function handlePressToTalkCancel() {
  logDebug(`pressToTalkCancel event state=${state} starting=${pressToTalkStarting}`);
  pressToTalkHeld = false;
  const cancellation = currentRecordingCancellation;
  if (!cancellation || (state === "idle" && !pressToTalkStarting)) return;
  shouldPasteFinalTranscript = false;
  cancellation.abort();
  stopPolling();
  stopRecordingVisualizer();
  stopVolumeMonitor();
  if (pcmAudioContext) {
    setState("transcribing");
    await stopPcmCapture();
  }
  if (currentRecordingCancellation !== cancellation) return;
  flushAudioBatch();
  if (!pressToTalkStarting && !toggleRecordingStarting) {
    await finishModifierCancelledRecording();
  }
}

async function cancelTranscription() {
  if (currentRecordingCancellation?.signal.aborted) return;
  const sessionId = currentSessionId;
  if (sessionId) {
    await invoke("worker_stop_session", { sessionId }).catch((error) => {
      logDebug(`stop session failed=${errorMessage(error)}`);
    });
  }
  await finishCurrentRecording("cancelled", "", "Cancelled by user");
  cleanup();
  setState("idle");
  showToast("Transcription cancelled. Raw audio is still saved.", 4000);
}

async function transcribeImportedFile(file: File) {
  if (state !== "idle") {
    showToast("Finish the current recording first");
    return;
  }
  try {
    const settings = await invoke<AppSettings>("app_get_settings");
    if (needsApiKey(settings.transcriptionModel, settings.hasOpenaiApiKey)) {
      throw new Error("Set your OpenAI API key before importing audio");
    }
    if (getModelCapabilities(settings.transcriptionModel).streamsText) {
      throw new Error("Choose Local, Fast or Accurate transcription for imported audio files");
    }
    const bytes = await file.arrayBuffer();
    const mimeType = file.type || mimeTypeForFileName(file.name);
    const stored = await invoke<{ id: string }>("recording_import", {
      name: file.name,
      mimeType,
      fileBase64: toBase64(bytes),
    });
    await transcribeStoredAudio(stored.id, bytes, mimeType, "file-import");
  } catch (error) {
    await handleTranscriptionFailure(error);
    cleanup();
    setState("idle");
  }
}

async function retranscribeRecording(recording: RecordingMetadata) {
  if (state !== "idle") {
    showToast("Finish the current recording first");
    return;
  }
  try {
    const settings = await invoke<AppSettings>("app_get_settings");
    if (needsApiKey(settings.transcriptionModel, settings.hasOpenaiApiKey)) {
      throw new Error("Set your OpenAI API key before transcribing saved audio");
    }
    if (getModelCapabilities(settings.transcriptionModel).streamsText) {
      throw new Error("Choose Local, Fast or Accurate transcription for saved audio files");
    }
    const audio = await invoke<RecordingAudio>("recording_read", {
      recordingId: recording.id,
    });
    await transcribeStoredAudio(
      recording.id,
      fromBase64(audio.fileBase64),
      audio.mimeType,
      "recording-retry",
    );
  } catch (error) {
    await handleTranscriptionFailure(error);
    cleanup();
    setState("idle");
  }
}

async function transcribeStoredAudio(
  recordingId: string,
  bytes: ArrayBuffer,
  mimeType: string,
  profileId: string,
) {
  currentRecordingCancellation = null;
  currentRecordingId = recordingId;
  lastFailedRecordingId = null;
  sessionFailure = "";
  pollFailureCount = 0;
  shouldPasteFinalTranscript = false;
  finalTranscript = "";
  liveTranscript = "";
  pastedTranscript = "";
  targetWriteQueue = Promise.resolve();
  await invoke("recording_finish", {
    recordingId,
    status: "transcribing",
    transcript: "",
    error: "",
  });
  setState("transcribing");
  closeMenu();

  const session = await invoke<{ session_id: string }>("worker_start_session", {
    profileId,
  });
  currentSessionId = session.session_id;
  startPolling();
  const chunkSize = 512 * 1024;
  for (let offset = 0; offset < bytes.byteLength; offset += chunkSize) {
    const chunk = bytes.slice(offset, Math.min(bytes.byteLength, offset + chunkSize));
    await invoke("worker_append_audio_chunk", {
      sessionId: currentSessionId,
      chunkBase64: toBase64(chunk),
    });
  }
  await invoke("worker_finalize_session_audio", {
    sessionId: currentSessionId,
    mimeType,
  });
}

function mimeTypeForFileName(name: string): string {
  const lower = name.toLowerCase();
  if (lower.endsWith(".wav")) return "audio/wav";
  if (lower.endsWith(".mp3") || lower.endsWith(".mpeg") || lower.endsWith(".mpga")) {
    return "audio/mpeg";
  }
  if (lower.endsWith(".m4a")) return "audio/x-m4a";
  if (lower.endsWith(".mp4")) return "audio/mp4";
  if (lower.endsWith(".ogg")) return "audio/ogg";
  return "audio/webm";
}

function cleanup() {
  logDebug(`cleanup state=${state} session=${currentSessionId ?? "none"}`);
  stopPolling();
  stopRecordingVisualizer();
  stopVolumeMonitor();
  if (mediaRecorder && mediaRecorder.state !== "inactive") {
    mediaRecorder.stop();
  }
  if (pcmAudioContext) void stopPcmCapture();
  micStream?.getTracks().forEach((t) => t.stop());
  mediaRecorder = null;
  micStream = null;
  currentSessionId = null;
  backupAudioQueue = null;
  audioQueue.reset();
  pendingAudioParts = [];
  pendingAudioBytes = 0;
  liveTranscript = "";
  streamTextToTarget = false;
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------
function startPolling() {
  stopPolling();
  pollTimer = window.setInterval(() => void pollOnce(), 350);
}

function stopPolling() {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function pollOnce() {
  if (!currentSessionId || pollInFlight) return;
  const signal = currentRecordingCancellation?.signal;
  const sessionId = currentSessionId;
  if (signal?.aborted) return;
  pollInFlight = true;
  let lastDelivery: { inserted: boolean } | null = null;
  try {
    const resp = await invoke<PollResponse>("worker_poll_session_events", {
      sessionId,
    });
    if (signal?.aborted || currentSessionId !== sessionId) return;
    for (const ev of resp.events ?? []) {
      if (signal?.aborted || currentSessionId !== sessionId) return;
      if (ev.type === "final") {
        finalTranscript = `${finalTranscript} ${ev.text}`.trim();
        liveTranscript = ev.text;
        widget.dataset.liveTranscript = liveTranscript;
        widget.setAttribute("aria-label", `Transcription: ${liveTranscript}`);
      } else if (
        ev.type === "partial" &&
        ev.text !== "Listening..." &&
        ev.text !== "Refining transcript..." &&
        !ev.text.startsWith("Captured ") &&
        !ev.text.startsWith("Transcribing part ")
      ) {
        liveTranscript += ev.text;
        widget.dataset.liveTranscript = liveTranscript;
        widget.setAttribute("aria-label", `Live transcription: ${liveTranscript}`);
        if (streamTextToTarget) {
          const delta = ev.text;
          targetWriteQueue = targetWriteQueue.then(async () => {
            if (signal?.aborted || currentSessionId !== sessionId) return;
            const result = await invoke<PasteResult>("paste_to_target", { text: delta });
            if (signal?.aborted || currentSessionId !== sessionId) return;
            if (result?.pasted) pastedTranscript += delta;
          }).catch((err) => {
            logDebug(`live target write failed=${err instanceof Error ? err.message : String(err)}`);
          });
        }
      } else if (ev.type === "error") {
        sessionFailure = ev.text || "Transcription failed";
        await handleTranscriptionFailure(sessionFailure);
      } else if (ev.type === "warning") {
        showToast(ev.text, 8000);
      }
    }
    if (signal?.aborted || currentSessionId !== sessionId) return;
    pollFailureCount = 0;
    if (resp.done) {
      stopPolling();
      if (finalTranscript) {
        await targetWriteQueue;
        if (signal?.aborted || currentSessionId !== sessionId) return;
        const quillRewrite = shouldRewriteDictation({
          cleanupEffective: quillCleanupAtStart,
          copyOnly: !shouldPasteFinalTranscript,
          streaming: streamTextToTarget,
        })
          ? await rewriteFinishedDictation(finalTranscript)
          : null;
        if (signal?.aborted || currentSessionId !== sessionId) return;
        if (quillRewrite) {
          finalTranscript = quillRewrite.text;
          quillDictationPending = quillRewrite;
          await invoke<void>("quill_clipboard_hold").catch((error) => {
            logDebug(quillLogLine("clipboard_hold_failed", toQuillCommandError(error).code));
          });
        }
        lastTranscript = finalTranscript;
        localStorage.setItem("ow_last_transcript", lastTranscript);
        syncWidgetFrame();
        const delivery = await deliverTranscript({
          copyOnly: !shouldPasteFinalTranscript,
          isMac,
          isCurrent: () => !signal?.aborted && currentSessionId === sessionId,
          paste: () => pastedTranscript
            ? pastedTranscript.trim() === finalTranscript.trim()
              ? Promise.resolve({ pasted: true })
              : invoke<PasteResult>("replace_streamed_target", {
                  streamedText: pastedTranscript,
                  finalText: finalTranscript,
                })
            : invoke<PasteResult>("paste_to_target", { text: finalTranscript }),
          copy: () => invoke<void>("copy_to_clipboard", { text: finalTranscript }),
        });
        lastDelivery = delivery;
        if (!delivery || signal?.aborted || currentSessionId !== sessionId) return;
        await finishCurrentRecording("complete", finalTranscript, "");
        if (signal?.aborted || currentSessionId !== sessionId) return;
        await settleQuillDictation(delivery);
        if (delivery.notice) showToast(delivery.notice, 6000);
        if (delivery.needsPasteAccess && !pasteAccessRequested) {
          pasteAccessRequested = true;
          void invoke<boolean>("request_paste_access").catch((error) => {
            logDebug(`paste access request failed=${errorMessage(error)}`);
          });
        }
      } else if (sessionFailure) {
        await finishCurrentRecording("failed", "", sessionFailure);
        if (signal?.aborted || currentSessionId !== sessionId) return;
      }
      cleanup();
      widget.removeAttribute("data-live-transcript");
      widget.setAttribute("aria-label", "Click to start or stop transcription");
      setState("idle");
    }
  } catch (error) {
    if (signal?.aborted || currentSessionId !== sessionId) return;
    const message = errorMessage(error, "Could not read transcription status");
    logDebug(`poll failed=${message}`);
    pollFailureCount += 1;
    if (pollFailureCount >= 3 && (state === "recording" || state === "transcribing")) {
      await handleTranscriptionFailure(`Could not read transcription status: ${message}`);
      if (signal?.aborted || currentSessionId !== sessionId) return;
      cleanup();
      setState("idle");
    }
  } finally {
    pollInFlight = false;
    // A rewrite whose session ended after its delivery reports what was delivered.
    if (quillDictationPending && lastDelivery) void settleQuillDictation(lastDelivery);
    // A rewrite whose delivery was abandoned still ends its clipboard hold.
    if (quillDictationPending) void settleQuillDictation(null);
  }
}

// Native end-to-end smoke hook for development builds. This deliberately uses
// the same Tauri commands, polling loop, and target-field streaming as a
// microphone recording; only the PCM source is deterministic test audio.
if (import.meta.env.DEV) {
  (window as typeof window & {
    __openwhisperRealtimeSmoke?: (chunks: string[], intervalMs?: number) => Promise<void>;
  }).__openwhisperRealtimeSmoke = async (chunks, intervalMs = 100) => {
    if (state !== "idle") throw new Error(`Cannot start smoke test while state=${state}`);
    currentRecordingCancellation = null;
    await invoke("capture_paste_target");
    const result = await invoke<{ session_id: string }>("worker_start_session", {
      profileId: "realtime-smoke",
    });
    currentSessionId = result.session_id;
    finalTranscript = "";
    liveTranscript = "";
    streamTextToTarget = true;
    pastedTranscript = "";
    targetWriteQueue = Promise.resolve();
    setState("recording");
    startPolling();
    for (const chunkBase64 of chunks) {
      await invoke("worker_append_audio_chunk", {
        sessionId: currentSessionId,
        chunkBase64,
      });
      await new Promise((resolve) => window.setTimeout(resolve, intervalMs));
    }
    setState("transcribing");
    await invoke("worker_finalize_session_audio", {
      sessionId: currentSessionId,
      mimeType: "audio/pcm;rate=24000",
    });
  };
}

// ---------------------------------------------------------------------------
// Toggle
// ---------------------------------------------------------------------------
async function toggleRecording() {
  if (recordingStartBlocked(state)) return;
  if (state === "idle") {
    if (pressToTalkStarting) return;
    if (toggleRecordingStarting) {
      toggleStopRequestedDuringStart = !toggleStopRequestedDuringStart;
      return;
    }

    toggleRecordingStarting = true;
    toggleStopRequestedDuringStart = false;
    try {
      await startRecording();
      if (toggleStopRequestedDuringStart && state === "recording") {
        await stopRecording();
      }
    } finally {
      toggleRecordingStarting = false;
      toggleStopRequestedDuringStart = false;
    }
  } else if (state === "recording") {
    await stopRecording();
  }
}

async function handlePressToTalkStart() {
  logDebug(
    `pressToTalkStart event state=${state} held=${pressToTalkHeld} starting=${pressToTalkStarting}`,
  );
  if (recordingStartBlocked(state)) return;
  pressToTalkHeld = true;
  if (state !== "idle" || pressToTalkStarting || toggleRecordingStarting) return;

  pressToTalkStarting = true;
  try {
    await startRecording();
  } finally {
    pressToTalkStarting = false;
  }

  // If the user released the shortcut while startup was still in flight,
  // stop immediately once recording is actually active.
  if (!pressToTalkHeld && isRecording) {
    logDebug("pressToTalkStart detected release during startup");
    await stopRecording();
  }
}

async function handlePressToTalkStop() {
  logDebug(
    `pressToTalkStop event state=${state} held=${pressToTalkHeld} starting=${pressToTalkStarting}`,
  );
  pressToTalkHeld = false;
  if (state === "rewriting") return;
  if (currentRecordingCancellation?.signal.aborted) return;
  if (shouldCancelEmptyHold({ starting: pressToTalkStarting, state, capturedBytes: capturedPcmBytes })) {
    await handlePressToTalkCancel();
    return;
  }
  if (state === "recording") {
    await stopRecording();
  }
}

btnMic.addEventListener("click", (e) => {
  e.stopPropagation();
  void toggleRecording();
});

// ---------------------------------------------------------------------------
// Settings (API key)
// ---------------------------------------------------------------------------
function applyStoredKeyPreview(preview: string | null) {
  storedKeyPreview = preview ?? "";
  showingStoredKeyPreview = storedKeyPreview.length > 0;
  keyInputDirty = false;
  menuKeyInput.type = showingStoredKeyPreview ? "text" : "password";
  menuKeyInput.value = storedKeyPreview;
}

function selectStoredKeyPreview() {
  if (!showingStoredKeyPreview) return;
  requestAnimationFrame(() => menuKeyInput.select());
}

function restoreStoredKeyPreview() {
  if (keyInputDirty) return;
  applyStoredKeyPreview(storedKeyPreview || null);
}

const MODEL_HINTS: Record<TranscriptionModel, string> = {
  "whisper-base-local":
    "Recommended local model. Multilingual Whisper Base, quantized to 60 MB. Transcribes on this Mac after you stop recording. No API key or usage fees.",
  "whisper-tiny-local":
    "Smallest local model: 32 MB. Multilingual and fast, with lower accuracy than Base. Transcribes after you stop recording. No API key or usage fees.",
  "whisper-turbo-local":
    "Multilingual Whisper Large v3 Turbo. Runs locally on this Mac after you stop recording. No API key or usage fees.",
  "gpt-4o-mini-transcribe":
    "Lowest-cost everyday choice. Fast, accurate dictation. API input/output rates: $1.25/$5 per 1M tokens.",
  "gpt-4o-transcribe":
    "Best accuracy for accents, noise, and difficult audio. API input/output rates: $2.50/$10 per 1M tokens.",
  "gpt-realtime-transcribe":
    "Lowest-latency streaming model (official API model: GPT-Realtime-Whisper). $0.017 per audio minute.",
};

function syncModelHint() {
  const model = menuModelSelect.value as TranscriptionModel;
  menuModelHint.textContent = MODEL_HINTS[model] ?? "";
  menuCloudSettings.style.display = getModelCapabilities(model).requiresApiKey ? "" : "none";
}

function syncRecordingShortcutUi(error: string | null = null) {
  const label = formatRecordingShortcut(recordingShortcut);
  menuRecordingShortcut.value = recordingShortcut;
  menuHoldToTalkLabel.textContent = `Hold ${label} to talk`;
  menuHoldToTalkToggle.setAttribute("aria-label", `Hold ${label} to talk`);
  menuHoldToTalkSwitch.title = `When off, press ${label} once to start and again to finish`;
  menuShortcutHint.textContent = `Turn this off to press ${label} once to start listening and again to finish.`;
  menuShortcutError.textContent = error
    ? `${error} Choose another shortcut or free it in System Settings → Keyboard → Keyboard Shortcuts.`
    : "";
  menuShortcutError.style.display = error ? "" : "none";
  if (error && error !== recordingShortcutError) {
    showToast(`Recording shortcut unavailable: ${error}`, 8000);
  }
  recordingShortcutError = error;
}

function applySettingsUi(settings: AppSettings, configuredLabel = "Key configured") {
  const bubbleWasVisible = settingsLoaded && needsApiKey(transcriptionModel, hasOpenaiApiKey) && state === "idle";
  hasOpenaiApiKey = settings.hasOpenaiApiKey;
  transcriptionModel = settings.transcriptionModel ?? DEFAULT_TRANSCRIPTION_MODEL;
  settingsLoaded = true;
  const bubbleVisibilityChanged = bubbleWasVisible !== (needsApiKey(transcriptionModel, hasOpenaiApiKey) && state === "idle");
  syncApiKeyBubble();
  applyStoredKeyPreview(settings.openaiApiKeyPreview);
  menuKeyStatus.textContent = settings.hasOpenaiApiKey ? configuredLabel : "Not configured";
  menuKeyStatus.className = `menu-item ${settings.hasOpenaiApiKey ? "key-ok" : "key-missing"}`;
  menuModelSelect.value = transcriptionModel;
  showIdlePill = settings.showIdlePill ?? true;
  menuShowIdlePillToggle.checked = showIdlePill;
  menuHoldToTalkToggle.checked = settings.holdToTalk ?? true;
  recordingShortcut = settings.recordingShortcut ?? defaultRecordingShortcut;
  syncRecordingShortcutUi(settings.recordingShortcutError ?? null);
  syncModelHint();
  if (!promptDirty) {
    menuPromptInput.value = settings.transcriptionPrompt ?? "";
  }
  syncQuillSettingsUi(settings);
  if ((bubbleVisibilityChanged || lastAppliedLayout?.visible !== getWindowLayout().visible) && lastAppliedLayout !== null) {
    void applyOverlayLayout();
  }
}

async function persistKeyInput() {
  if (keySaveInFlight || !keyInputDirty) return;

  keySaveInFlight = true;
  const key = menuKeyInput.value.trim();
  try {
    const updated = await invoke<AppSettings>("app_save_settings", { openaiApiKey: key });
    applySettingsUi(updated, updated.hasOpenaiApiKey ? "Key saved" : "Key cleared");
    showToast(updated.hasOpenaiApiKey ? "API key saved" : "API key cleared");
  } catch (err) {
    keyInputDirty = true;
    showToast(err instanceof Error ? err.message : String(err));
  } finally {
    keySaveInFlight = false;
  }
}

async function revealStoredKeyForEdit() {
  if (!showingStoredKeyPreview || keyRevealInFlight) return;

  keyRevealInFlight = true;
  showingStoredKeyPreview = false;
  menuKeyInput.type = "text";
  requestAnimationFrame(() => menuKeyInput.select());

  try {
    const key = await invoke<string>("app_get_openai_api_key");
    if (keyInputDirty || document.activeElement !== menuKeyInput) {
      return;
    }

    menuKeyInput.value = key;
    requestAnimationFrame(() => menuKeyInput.select());
  } catch (err) {
    if (!keyInputDirty) {
      restoreStoredKeyPreview();
    }
    showToast(err instanceof Error ? err.message : String(err));
  } finally {
    keyRevealInFlight = false;
  }
}

async function loadSettings() {
  try {
    const settings = await invoke<AppSettings>("app_get_settings");
    applySettingsUi(settings);
  } catch {
    applySettingsUi({ hasOpenaiApiKey: false, openaiApiKeyPreview: null, transcriptionModel: DEFAULT_TRANSCRIPTION_MODEL, transcriptionPrompt: "", refineEnabled: false, refinePrompt: "", showIdlePill: true, holdToTalk: true, recordingShortcut: defaultRecordingShortcut });
  }
}

menuShowIdlePillToggle.addEventListener("change", async () => {
  if (showIdlePillSaveInFlight) return;

  showIdlePillSaveInFlight = true;
  try {
    const updated = await invoke<AppSettings>("app_save_settings", {
      showIdlePill: menuShowIdlePillToggle.checked,
    });
    applySettingsUi(updated);
    showToast(updated.showIdlePill ? "Idle pill shown" : "Idle pill hidden");
  } catch (err) {
    showToast(err instanceof Error ? err.message : String(err));
    await loadSettings();
  } finally {
    showIdlePillSaveInFlight = false;
  }
});

menuHoldToTalkToggle.addEventListener("change", async () => {
  if (holdToTalkSaveInFlight) return;

  holdToTalkSaveInFlight = true;
  try {
    const updated = await invoke<AppSettings>("app_save_settings", {
      holdToTalk: menuHoldToTalkToggle.checked,
    });
    applySettingsUi(updated);
    showToast(updated.holdToTalk ? "Hold to talk enabled" : "Press to start and stop enabled");
  } catch (err) {
    showToast(err instanceof Error ? err.message : String(err));
    await loadSettings();
  } finally {
    holdToTalkSaveInFlight = false;
  }
});

menuRecordingShortcut.addEventListener("change", async () => {
  if (recordingShortcutSaveInFlight) return;
  recordingShortcutSaveInFlight = true;
  menuRecordingShortcut.disabled = true;
  const result = await saveRecordingShortcut(
    recordingShortcut,
    menuRecordingShortcut.value,
    (shortcut) => invoke<AppSettings>("app_save_settings", {
      recordingShortcut: shortcut,
      ...(shortcut === "RightControl" ? { holdToTalk: true } : {}),
    }),
  );
  recordingShortcut = result.recordingShortcut;
  if (result.settings) {
    applySettingsUi(result.settings);
    showToast(`Recording shortcut set to ${formatRecordingShortcut(recordingShortcut)}`);
  } else {
    syncRecordingShortcutUi(result.error);
    if (menuOpen) void applyOverlayLayout();
  }
  recordingShortcutSaveInFlight = false;
  menuRecordingShortcut.disabled = false;
});

menuModelSelect.addEventListener("change", async () => {
  if (modelSaveInFlight) return;
  syncModelHint();
  modelSaveInFlight = true;
  try {
    const updated = await invoke<AppSettings>("app_save_settings", {
      transcriptionModel: menuModelSelect.value,
    });
    applySettingsUi(updated);
    showToast("Transcription model saved");
  } catch (err) {
    showToast(err instanceof Error ? err.message : String(err));
    await loadSettings();
  } finally {
    modelSaveInFlight = false;
  }
});

async function ensureAutostartEnabled() {
  try {
    if (!(await isAutostartEnabled())) {
      await enableAutostart();
      logDebug("autostart enabled");
    }
  } catch (err) {
    logDebug(`autostart enable failed=${err instanceof Error ? err.message : String(err)}`);
  }
}

menuKeyInput.addEventListener("focus", () => {
  selectStoredKeyPreview();
  void revealStoredKeyForEdit();
});

menuKeyInput.addEventListener("click", () => {
  selectStoredKeyPreview();
  void revealStoredKeyForEdit();
});

menuKeyInput.addEventListener("beforeinput", () => {
  if (showingStoredKeyPreview) {
    showingStoredKeyPreview = false;
    menuKeyInput.type = "text";
    keyInputDirty = true;
  }
});

menuKeyInput.addEventListener("input", () => {
  if (!showingStoredKeyPreview) {
    keyInputDirty = true;
  }
});

menuKeyInput.addEventListener("blur", () => {
  if (!keyInputDirty) {
    restoreStoredKeyPreview();
    return;
  }
  void persistKeyInput();
});

menuKeyInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    menuKeyInput.blur();
  }
});

menuKeyLink.addEventListener("click", async (event) => {
  event.preventDefault();
  try {
    await invoke("open_api_keys_page");
  } catch (err) {
    showToast(err instanceof Error ? err.message : String(err));
  }
});

// ---------------------------------------------------------------------------
// Settings (Transcription Prompt)
// ---------------------------------------------------------------------------
async function persistPromptInput() {
  if (promptSaveInFlight || !promptDirty) return;

  promptSaveInFlight = true;
  const prompt = menuPromptInput.value;
  try {
    const updated = await invoke<AppSettings>("app_save_settings", { transcriptionPrompt: prompt });
    promptDirty = false;
    applySettingsUi(updated, updated.hasOpenaiApiKey ? "Key configured" : "Not configured");
    showToast("Transcription prompt saved");
  } catch (err) {
    promptDirty = true;
    showToast(err instanceof Error ? err.message : String(err));
  } finally {
    promptSaveInFlight = false;
  }
}

menuPromptInput.addEventListener("input", () => {
  promptDirty = true;
});

menuPromptInput.addEventListener("blur", () => {
  if (!promptDirty) return;
  void persistPromptInput();
});

// ---------------------------------------------------------------------------
// Settings (Quill)
// ---------------------------------------------------------------------------
let quillModeSelectRequest = 0;

function syncQuillError() {
  const error = quillSaveError ?? quillSettings?.quillCommandError ?? null;
  menuQuillError.textContent = error ?? "";
  menuQuillError.style.display = error ? "" : "none";
}

function syncQuillSettingsUi(settings: AppSettings) {
  const previous = quillSettings;
  quillSettings = settings;
  menuQuillEnabledToggle.checked = settings.quillEnabled ?? false;
  menuQuillCleanupToggle.checked = settings.quillDictationCleanup ?? false;
  menuQuillCleanupToggle.disabled = !quillHookReady(settings);
  syncQuillError();
  if (!quillHookReady(settings)) {
    quillCatalog.clear();
    quillPicker.dismiss("picker_settings_close_failed");
    // The save that made the hook not ready also disarmed the revert hotkey; the load-failure
    // fallback says nothing about Rust, so it leaves an open slot alone.
    if (quillSettingsFromRust(settings)) quillSlot.close();
  } else if (!previous || !quillHookReady(previous) || previous.quillCommand !== settings.quillCommand) {
    quillCatalog.clear();
  }
  void syncQuillModeSelect(settings);
}

/** Fills the default-mode select only while the hook is on and valid (no engine call otherwise). */
async function syncQuillModeSelect(settings: AppSettings) {
  const request = ++quillModeSelectRequest;
  // The catalog answers repeat loads, so a settings save does not call the engine again.
  const select = await loadModeSelect(settings, (cmd, args) =>
    cmd === "quill_list_modes" ? quillCatalog.load().then((modes) => ({ modes })) : quillInvoke(cmd, args));
  if (request !== quillModeSelectRequest) return;
  menuQuillMode.innerHTML = select.options
    .map((option) => `<option value="${escapeHtml(option.value)}">${escapeHtml(option.label)}</option>`)
    .join("");
  menuQuillMode.disabled = select.disabled;
  if (select.selected) menuQuillMode.value = select.selected;
  menuQuillModeHint.textContent = select.hint ?? "";
  menuQuillModeHint.style.display = select.hint ? "" : "none";
  if (menuOpen) void applyOverlayLayout();
}

async function saveQuillSettings(update: Record<string, unknown>, success: string) {
  if (quillSaveInFlight) return;
  quillSaveInFlight = true;
  try {
    const updated = await invoke<AppSettings>("app_save_settings", update);
    quillSaveError = null;
    applySettingsUi(updated);
    showToast(success);
  } catch (err) {
    quillSaveError = err instanceof Error ? err.message : String(err);
    await loadSettings();
  } finally {
    quillSaveInFlight = false;
  }
}

menuQuillEnabledToggle.addEventListener("change", () => {
  const enabled = menuQuillEnabledToggle.checked;
  void saveQuillSettings({ quillEnabled: enabled }, enabled ? "Rewrite with quill on" : "Rewrite with quill off");
});

menuQuillCleanupToggle.addEventListener("change", () => {
  const cleanup = menuQuillCleanupToggle.checked;
  void saveQuillSettings({ quillDictationCleanup: cleanup }, cleanup ? "Dictation cleanup on" : "Dictation cleanup off");
});

menuQuillMode.addEventListener("change", () => {
  void saveQuillSettings({ quillDefaultMode: menuQuillMode.value }, "Default rewrite mode saved");
});

// ---------------------------------------------------------------------------
// Foreground app detection (polls while menu is open)
// ---------------------------------------------------------------------------
let appPollTimer: number | null = null;

async function pollForegroundApp() {
  try {
    const name = await invoke<string | null>("get_foreground_app");
    menuTargetAppName.textContent = name ?? "Unknown";
  } catch {
    menuTargetAppName.textContent = "Unknown";
  }
}

function startAppPoll() {
  void pollForegroundApp();
  stopAppPoll();
  appPollTimer = window.setInterval(() => void pollForegroundApp(), 500);
}

function stopAppPoll() {
  if (appPollTimer !== null) {
    clearInterval(appPollTimer);
    appPollTimer = null;
  }
}

// ---------------------------------------------------------------------------
// Tray / global shortcuts: listen for recording events emitted by Rust
// ---------------------------------------------------------------------------
void listen("toggle-recording", () => {
  logDebug("event toggle-recording");
  return void toggleRecording();
});
void listen("press-to-talk-start", () => {
  logDebug("event press-to-talk-start");
  return void handlePressToTalkStart();
});
void listen("press-to-talk-stop", () => {
  logDebug("event press-to-talk-stop");
  return void handlePressToTalkStop();
});
void listen("press-to-talk-cancel", () => {
  return void handlePressToTalkCancel();
});
void listen("recording-shortcut-status-changed", () => {
  void loadSettings();
});
void listen("quill-rewrite", () => {
  quillHotkey.press();
});
void listen<string>("quill-picker-key", (event) => {
  quillHotkey.pickerKey(event.payload);
});
void listen("quill-revert", () => {
  void quillSlot.revertFromHotkey();
});
void listen<boolean>("overlay-revealed", (event) => {
  manualOverlayReveal = event.payload;
  void collapseOverlay();
});
void listen("overlay-lost-focus", () => {
  manualOverlayReveal = false;
  void collapseOverlay();
});

// ---------------------------------------------------------------------------
// Auto-update
// ---------------------------------------------------------------------------
async function checkForUpdates() {
  if (!getModelCapabilities(transcriptionModel).allowsAutomaticUpdates) return;
  let updateAccepted = false;
  try {
    const update = await check();
    if (update) {
      updateAccepted = window.confirm(
        `OpenWhisper ${update.version} is available.\n\n` +
          "Update now? Your settings and API key will be kept.",
      );
      if (!updateAccepted) return;
      showToast("Downloading OpenWhisper update…", 30_000);
      await update.downloadAndInstall();
      await relaunch();
    }
  } catch {
    if (updateAccepted) {
      showToast("Update failed. Your current app and settings are unchanged.", 6000);
    }
    // Ignore background check failures, including development mode.
  }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
if (import.meta.env.DEV) {
  Object.defineProperty(window, "__openWhisperTest", {
    value: { openMenu, collapseOverlay },
    configurable: true,
  });
}

widget.dataset.state = state;
syncWidgetFrame();
syncApiKeyBubble();
void loadSettings().then(() => {
  void applyOverlayLayout(true);
  if (import.meta.env.PROD && getModelCapabilities(transcriptionModel).allowsAutomaticUpdates) {
    setTimeout(() => void checkForUpdates(), 4000);
  }
});
if (import.meta.env.PROD) {
  void ensureAutostartEnabled();
}
void refreshMicDevices();
void invoke("worker_ping").catch(() => {});

if (navigator.mediaDevices?.addEventListener) {
  navigator.mediaDevices.addEventListener("devicechange", () => void refreshMicDevices());
}
