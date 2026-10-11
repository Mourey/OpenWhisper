import { afterEach, describe, expect, it, vi } from "vitest";
import { prepareRecordingCapture, SessionAudioQueue, shouldCancelEmptyHold } from "./recording-pipeline";
import {
  cancellableRecordingStep,
  completeRecordingCancellation,
  RecordingCancelledError,
  throwIfRecordingCancelled,
} from "./recording-cancellation";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

function microphone() {
  let receiver: ((buffer: ArrayBuffer) => void) | undefined;
  return {
    connect(callback: (buffer: ArrayBuffer) => void) { receiver = callback; },
    emit(...samples: number[]) { receiver?.(Int16Array.from(samples).buffer); },
    stop: vi.fn(() => { receiver = undefined; }),
  };
}
type Microphone = ReturnType<typeof microphone>;

function audioDestinations() {
  const rawSamples: number[] = [];
  const workerSamples: number[] = [];
  const raw = new SessionAudioQueue(async (_recordingId, buffer) => {
    rawSamples.push(...new Int16Array(buffer));
  });
  const worker = new SessionAudioQueue(async (_sessionId, buffer) => {
    workerSamples.push(...new Int16Array(buffer));
  });
  return {
    raw, worker, rawSamples, workerSamples,
    accept(buffer: ArrayBuffer) { raw.append(buffer); worker.append(buffer); },
  };
}

afterEach(() => vi.useRealTimers());

describe("recording onset at the production preparation boundary", () => {
  it("retains the opening PCM in both raw backup and worker while backup and session startup are blocked", async () => {
    vi.useFakeTimers();
    const stream = microphone();
    const audio = audioDestinations();
    const backup = deferred<{ id: string }>();
    const worker = deferred<{ session_id: string }>();
    const creatingBackup = vi.fn(() => backup.promise);
    const started = prepareRecordingCapture({
      acquireMicrophone: async () => stream,
      createBackup: creatingBackup,
      onBackupReady: (recording) => audio.raw.attachSession(recording.id),
      startCapture: async (microphone) => { microphone.connect(audio.accept); return microphone; },
      onCaptureStarted: () => {},
      startSession: () => worker.promise,
    }).then((session) => { audio.worker.attachSession(session.session_id); });
    await vi.advanceTimersByTimeAsync(0);
    expect(creatingBackup).toHaveBeenCalledOnce();

    stream.emit(1234, -4321);
    backup.resolve({ id: "raw-recording" });
    await vi.advanceTimersByTimeAsync(0);
    stream.emit(2222, -2222);
    worker.resolve({ session_id: "worker-session" });
    await started;
    stream.emit(3333);
    await Promise.all([audio.raw.drain(), audio.worker.drain()]);

    expect.soft(audio.rawSamples).toEqual([1234, -4321, 2222, -2222, 3333]);
    expect.soft(audio.workerSamples).toEqual([1234, -4321, 2222, -2222, 3333]);
  });

  it("cleans up an active capture cancelled while backup creation is pending and discards its late backup", async () => {
    vi.useFakeTimers();
    const cancellation = new AbortController();
    const stream = microphone();
    const audio = audioDestinations();
    const backup = deferred<{ id: string }>();
    const markLateBackupCancelled = vi.fn((recording: { id: string }) => {
      audio.raw.attachSession(recording.id);
    });
    const startCapture = vi.fn(async (microphone: Microphone) => {
      microphone.connect(audio.accept);
      return microphone;
    });
    const startWorker = vi.fn(async () => "session");
    const started = prepareRecordingCapture({
      acquireMicrophone: async () => stream,
      createBackup: () => cancellableRecordingStep(cancellation.signal, () => backup.promise, markLateBackupCancelled),
      onBackupReady: (recording) => {
        throwIfRecordingCancelled(cancellation.signal);
        audio.raw.attachSession(recording.id);
      },
      startCapture,
      onCaptureStarted: () => { throwIfRecordingCancelled(cancellation.signal); },
      startSession: () => { throwIfRecordingCancelled(cancellation.signal); return startWorker(); },
    }).catch(async (error) => {
      await completeRecordingCancellation({
        stopCapture: async () => stream.stop(),
        flushBackup: async () => {},
        stopSession: async () => {},
        markCancelled: async () => {},
        cleanup: () => { audio.worker.reset(); },
      });
      throw error;
    });
    const cancelled = expect(started).rejects.toBeInstanceOf(RecordingCancelledError);
    await vi.advanceTimersByTimeAsync(0);
    expect.soft(startCapture).toHaveBeenCalledOnce();
    stream.emit(1234);
    cancellation.abort();
    await cancelled;
    backup.resolve({ id: "late-backup" });
    await vi.advanceTimersByTimeAsync(0);
    stream.emit(9999);
    await audio.raw.drain();
    expect(stream.stop).toHaveBeenCalledOnce();
    expect(markLateBackupCancelled).toHaveBeenCalledWith({ id: "late-backup" });
    expect(startWorker).not.toHaveBeenCalled();
    expect(audio.raw.bufferedChunkCount).toBe(0);
    expect(audio.worker.bufferedChunkCount).toBe(0);
    expect(audio.rawSamples).toEqual([1234]);
    expect(audio.workerSamples).toEqual([]);
  });

  it("stops immediately if the hold was released before microphone readiness", async () => {
    vi.useFakeTimers();
    const stream = microphone();
    const ready = deferred<Microphone>();
    const audio = audioDestinations();
    const cancellation = new AbortController();
    const startCapture = vi.fn(async (microphone: Microphone) => {
      microphone.connect(audio.accept);
      return microphone;
    });
    const startWorker = vi.fn(async () => "worker-session");
    const createBackup = vi.fn(async () => ({ id: "raw-recording" }));
    const discardBackup = vi.fn();
    const started = prepareRecordingCapture({
      acquireMicrophone: () => cancellableRecordingStep(cancellation.signal, () => ready.promise, (stream) => stream.stop()),
      createBackup,
      discardBackup,
      onBackupReady: (recording) => audio.raw.attachSession(recording.id),
      startCapture,
      onCaptureStarted: () => { throwIfRecordingCancelled(cancellation.signal); },
      startSession: startWorker,
    }).then((session) => { audio.worker.attachSession(session); });
    const cancelled = expect(started).rejects.toBeInstanceOf(RecordingCancelledError);
    await vi.advanceTimersByTimeAsync(0);
    expect(createBackup).toHaveBeenCalledOnce();
    if (shouldCancelEmptyHold({ starting: true, state: "idle", capturedBytes: 0 })) cancellation.abort();
    expect(startCapture).not.toHaveBeenCalled();
    await cancelled;
    ready.resolve(stream);
    await vi.advanceTimersByTimeAsync(0);
    stream.emit(1234);
    expect(stream.stop).toHaveBeenCalledOnce();
    expect(startWorker).not.toHaveBeenCalled();
    expect(discardBackup).toHaveBeenCalledWith({ id: "raw-recording" });
    await Promise.all([audio.raw.drain(), audio.worker.drain()]);
    expect(audio.rawSamples).toEqual([]);
    expect(audio.workerSamples).toEqual([]);
  });

  it("quietly cancels a released hold after graph connection but before the first PCM callback", async () => {
    vi.useFakeTimers();
    const stream = microphone();
    const audio = audioDestinations();
    const worker = deferred<{ session_id: string }>();
    const cancellation = new AbortController();
    let state: "idle" | "recording" = "idle";
    let capturedBytes = 0;
    const finalize = vi.fn();
    const stopWorker = vi.fn();
    const started = prepareRecordingCapture({
      acquireMicrophone: async () => stream,
      createBackup: async () => ({ id: "raw-recording" }),
      onBackupReady: (recording) => audio.raw.attachSession(recording.id),
      startCapture: async (microphone) => {
        microphone.connect((buffer) => { capturedBytes += buffer.byteLength; audio.accept(buffer); });
        return microphone;
      },
      onCaptureStarted: () => { state = "recording"; },
      startSession: () => worker.promise,
    }).then(async (session) => {
      if (cancellation.signal.aborted) {
        await completeRecordingCancellation({
          stopCapture: async () => stream.stop(), flushBackup: () => audio.raw.drain(),
          stopSession: async () => { stopWorker(session.session_id); }, markCancelled: async () => {},
          cleanup: () => audio.worker.reset(),
        });
      } else {
        audio.worker.attachSession(session.session_id);
        finalize();
      }
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(state).toBe("recording");
    if (shouldCancelEmptyHold({ starting: true, state, capturedBytes })) {
      cancellation.abort();
      stream.stop();
    }
    worker.resolve({ session_id: "starting-worker" });
    await started;
    expect(finalize).not.toHaveBeenCalled();
    expect(stopWorker).toHaveBeenCalledWith("starting-worker");
    expect(audio.rawSamples).toEqual([]);
    expect(audio.workerSamples).toEqual([]);
  });

  it("keeps a released hold with captured PCM on the normal finalization path", () => {
    expect(shouldCancelEmptyHold({ starting: true, state: "recording", capturedBytes: 2 })).toBe(false);
    expect(shouldCancelEmptyHold({ starting: false, state: "transcribing", capturedBytes: 0 })).toBe(false);
  });
});
