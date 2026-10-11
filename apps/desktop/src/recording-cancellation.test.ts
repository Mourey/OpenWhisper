import { describe, expect, it, vi } from "vitest";
import { captureBeforeSession } from "./recording-pipeline";
import {
  RecordingCancelledError,
  cancellableRecordingStep,
  completeRecordingCancellation,
  throwIfRecordingCancelled,
} from "./recording-cancellation";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

describe("modifier chord recording cancellation", () => {
  it("does not start an operation after the modifier hold has been cancelled", async () => {
    const cancellation = new AbortController();
    cancellation.abort();
    const operation = vi.fn(async () => "microphone");
    await expect(cancellableRecordingStep(cancellation.signal, operation)).rejects.toBeInstanceOf(RecordingCancelledError);
    expect(operation).not.toHaveBeenCalled();
  });

  it("releases a pending microphone wait and stops a stream that arrives afterwards", async () => {
    const cancellation = new AbortController();
    const microphone = deferred<{ stop: () => void }>();
    const stop = vi.fn();
    const waiting = cancellableRecordingStep(cancellation.signal, () => microphone.promise, (stream) => stream.stop());
    const cancelled = expect(waiting).rejects.toBeInstanceOf(RecordingCancelledError);
    await Promise.resolve();
    cancellation.abort();
    await cancelled;
    microphone.resolve({ stop });
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(stop).toHaveBeenCalledOnce();
  });

  it("marks a late-created raw backup cancelled without removing it", async () => {
    const cancellation = new AbortController();
    const backup = deferred<{ id: string }>();
    const markCancelled = vi.fn();
    const waiting = cancellableRecordingStep(cancellation.signal, () => backup.promise, markCancelled);
    const cancelled = expect(waiting).rejects.toBeInstanceOf(RecordingCancelledError);
    await Promise.resolve();
    cancellation.abort();
    await cancelled;
    backup.resolve({ id: "raw-backup" });
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(markCancelled).toHaveBeenCalledWith({ id: "raw-backup" });
  });

  it("never starts a worker session when cancellation follows capture preparation", async () => {
    const cancellation = new AbortController();
    const startWorker = vi.fn(async () => "session");
    const recording = captureBeforeSession({
      startCapture: async () => "microphone",
      onCaptureStarted: () => cancellation.abort(),
      startSession: () => {
        throwIfRecordingCancelled(cancellation.signal);
        return startWorker();
      },
    });
    await expect(recording).rejects.toBeInstanceOf(RecordingCancelledError);
    expect(startWorker).not.toHaveBeenCalled();
  });

  it("tracks a worker already starting and cancels its returned session before cleanup", async () => {
    const cancellation = new AbortController();
    const worker = deferred<{ session_id: string }>();
    const workerStarting = deferred<void>();
    const events: string[] = [];
    const recording = captureBeforeSession({
      startCapture: async () => "microphone",
      onCaptureStarted: () => { events.push("capture ready"); },
      startSession: () => {
        throwIfRecordingCancelled(cancellation.signal);
        workerStarting.resolve();
        return worker.promise;
      },
    }).then(async (session) => {
      if (cancellation.signal.aborted) {
        await completeRecordingCancellation({
          stopCapture: async () => { events.push("stop microphone"); },
          flushBackup: async () => { events.push("save raw audio"); },
          stopSession: async () => { events.push(`stop ${session.session_id}`); },
          markCancelled: async () => { events.push("mark cancelled"); },
          cleanup: () => { events.push("cleanup"); },
        });
      }
    });
    await workerStarting.promise;
    cancellation.abort();
    expect(events).toEqual(["capture ready"]);
    worker.resolve({ session_id: "pending-worker-session" });
    await recording;
    expect(events).toEqual([
      "capture ready", "stop microphone", "save raw audio",
      "stop pending-worker-session", "mark cancelled", "cleanup",
    ]);
  });

  it("still stops the worker and marks cancellation when saving raw audio fails", async () => {
    const stopSession = vi.fn(async () => {});
    const markCancelled = vi.fn(async () => {});
    const cleanup = vi.fn();
    await expect(completeRecordingCancellation({
      stopCapture: async () => {},
      flushBackup: async () => { throw new Error("Backup unavailable"); },
      stopSession,
      markCancelled,
      cleanup,
    })).rejects.toThrow("Backup unavailable");
    expect(stopSession).toHaveBeenCalledOnce();
    expect(markCancelled).toHaveBeenCalledOnce();
    expect(cleanup).toHaveBeenCalledOnce();
  });
});
