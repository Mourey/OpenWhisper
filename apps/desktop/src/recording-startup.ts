export const RECORDING_STARTUP_TIMEOUT_MS = 15_000;
export const AUDIO_CONTEXT_STARTUP_TIMEOUT_MS = 5_000;

export function isRecordingWindowVisible(input: {
  showIdlePill: boolean;
  active: boolean;
  menuOpen: boolean;
  manuallyRevealed: boolean;
  starting: boolean;
}) {
  return input.showIdlePill || input.active || input.menuOpen || input.manuallyRevealed || input.starting;
}

export function waitForStartupStep<T>(
  stage: string,
  operation: () => Promise<T>,
  timeoutMs = RECORDING_STARTUP_TIMEOUT_MS,
  discardLateResult?: (value: T) => void,
): Promise<T> {
  return new Promise((resolve, reject) => {
    let expired = false;
    const timer = setTimeout(() => {
      expired = true;
      reject(new Error(`${stage} timed out. Check microphone access in System Settings and try again.`));
    }, timeoutMs);
    Promise.resolve().then(operation).then(
      (value) => {
        if (expired) {
          discardLateResult?.(value);
          return;
        }
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        if (!expired) reject(error);
      },
    );
  });
}

type RecordingVisibility = {
  visibilityState: string;
  addEventListener: (type: "visibilitychange", listener: () => void) => void;
  removeEventListener: (type: "visibilitychange", listener: () => void) => void;
};

export async function waitForRecordingVisibility(
  document: RecordingVisibility,
  timeoutMs = RECORDING_STARTUP_TIMEOUT_MS,
) {
  if (document.visibilityState === "visible") return;
  const visible = () => document.visibilityState === "visible";
  let onVisibilityChange: (() => void) | undefined;
  try {
    await waitForStartupStep("Recording window visibility", () => new Promise<void>((resolve) => {
      onVisibilityChange = () => { if (visible()) resolve(); };
      document.addEventListener("visibilitychange", onVisibilityChange);
      onVisibilityChange();
    }), timeoutMs);
  } finally {
    if (onVisibilityChange) document.removeEventListener("visibilitychange", onVisibilityChange);
  }
}

export async function acquireVisibleMicrophone<Microphone>(steps: {
  revealWindow: () => Promise<unknown>;
  document: RecordingVisibility;
  acquire: () => Promise<Microphone>;
  discard: (microphone: Microphone) => void;
  onStage?: (stage: string) => void;
  timeoutMs?: number;
}): Promise<Microphone> {
  const timeoutMs = steps.timeoutMs ?? RECORDING_STARTUP_TIMEOUT_MS;
  steps.onStage?.("window show begin");
  await waitForStartupStep("Showing recording window", steps.revealWindow, timeoutMs);
  steps.onStage?.("window show complete");
  await waitForRecordingVisibility(steps.document, timeoutMs);
  steps.onStage?.("microphone access begin");
  const microphone = await waitForStartupStep("Microphone access", steps.acquire, timeoutMs, steps.discard);
  steps.onStage?.("microphone access complete");
  return microphone;
}

export async function resumeRecordingAudioContext(
  context: Pick<AudioContext, "resume" | "close">,
  timeoutMs = AUDIO_CONTEXT_STARTUP_TIMEOUT_MS,
) {
  try {
    await waitForStartupStep("Audio capture startup", () => context.resume(), timeoutMs);
  } catch (error) {
    void context.close().catch(() => {});
    throw error;
  }
}
