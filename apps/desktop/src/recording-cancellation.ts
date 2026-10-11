export class RecordingCancelledError extends Error {
  constructor() {
    super("Recording cancelled by keyboard chord");
    this.name = "RecordingCancelledError";
  }
}

export function throwIfRecordingCancelled(signal: AbortSignal) {
  if (signal.aborted) throw new RecordingCancelledError();
}

export function cancellableRecordingStep<T>(
  signal: AbortSignal,
  operation: () => Promise<T>,
  discardLateResult?: (value: T) => void,
): Promise<T> {
  if (signal.aborted) return Promise.reject(new RecordingCancelledError());
  return new Promise((resolve, reject) => {
    const cancelled = () => reject(new RecordingCancelledError());
    signal.addEventListener("abort", cancelled, { once: true });
    Promise.resolve().then(() => {
      throwIfRecordingCancelled(signal);
      return operation();
    }).then(
      (value) => {
        signal.removeEventListener("abort", cancelled);
        if (signal.aborted) {
          discardLateResult?.(value);
          return;
        }
        resolve(value);
      },
      (error) => {
        signal.removeEventListener("abort", cancelled);
        reject(error);
      },
    );
  });
}

export async function completeRecordingCancellation(steps: {
  stopCapture: () => Promise<unknown>;
  flushBackup: () => Promise<unknown>;
  stopSession: () => Promise<unknown>;
  markCancelled: () => Promise<unknown>;
  cleanup: () => void;
}) {
  let firstError: unknown;
  try {
    for (const step of [steps.stopCapture, steps.flushBackup, steps.stopSession, steps.markCancelled]) {
      try {
        await step();
      } catch (error) {
        firstError ??= error;
      }
    }
  } finally {
    steps.cleanup();
  }
  if (firstError) throw firstError;
}
