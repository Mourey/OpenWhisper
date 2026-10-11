import { afterEach, describe, expect, it, vi } from "vitest";
import {
  acquireVisibleMicrophone,
  isRecordingWindowVisible,
  resumeRecordingAudioContext,
  waitForRecordingVisibility,
  waitForStartupStep,
} from "./recording-startup";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

function visibilityDocument(initialState = "visible") {
  const listeners = new Set<() => void>();
  return {
    visibilityState: initialState,
    addEventListener(_type: "visibilitychange", listener: () => void) { listeners.add(listener); },
    removeEventListener(_type: "visibilitychange", listener: () => void) { listeners.delete(listener); },
    setVisibility(state: string) {
      this.visibilityState = state;
      listeners.forEach((listener) => listener());
    },
    get listenerCount() { return listeners.size; },
  };
}

afterEach(() => vi.useRealTimers());

describe("recording startup", () => {
  it("keeps a hidden idle window visible during startup after focus loss clears manual reveal", () => {
    const hiddenIdle = {
      showIdlePill: false,
      active: false,
      menuOpen: false,
      manuallyRevealed: false,
      starting: false,
    };
    expect(isRecordingWindowVisible(hiddenIdle)).toBe(false);
    expect(isRecordingWindowVisible({ ...hiddenIdle, starting: true })).toBe(true);
    expect(isRecordingWindowVisible({ ...hiddenIdle, active: true })).toBe(true);
    expect(isRecordingWindowVisible(hiddenIdle)).toBe(false);
  });

  it("awaits native reveal and document visibility before asking for the microphone", async () => {
    vi.useFakeTimers();
    const document = visibilityDocument("hidden");
    const reveal = deferred<void>();
    const events: string[] = [];
    const acquire = vi.fn(async () => { events.push("microphone"); return "stream"; });
    const started = acquireVisibleMicrophone({
      revealWindow: () => { events.push("reveal"); return reveal.promise; },
      document,
      acquire,
      discard: vi.fn(),
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(events).toEqual(["reveal"]);
    reveal.resolve();
    await vi.advanceTimersByTimeAsync(0);
    expect(acquire).not.toHaveBeenCalled();
    expect(document.listenerCount).toBe(1);
    document.setVisibility("visible");
    await expect(started).resolves.toBe("stream");
    expect(events).toEqual(["reveal", "microphone"]);
    expect(document.listenerCount).toBe(0);
  });

  it("fails a native reveal timeout without starting capture", async () => {
    vi.useFakeTimers();
    const acquire = vi.fn(async () => "stream");
    const started = acquireVisibleMicrophone({
      revealWindow: () => new Promise(() => {}),
      document: visibilityDocument(),
      acquire,
      discard: vi.fn(),
    });
    const failed = expect(started).rejects.toThrow("Showing recording window timed out");
    await vi.advanceTimersByTimeAsync(15_000);
    await failed;
    expect(acquire).not.toHaveBeenCalled();
  });

  it("removes visibility listeners when a hidden window never becomes visible", async () => {
    vi.useFakeTimers();
    const document = visibilityDocument("hidden");
    const waiting = waitForRecordingVisibility(document);
    const failed = expect(waiting).rejects.toThrow("Recording window visibility timed out");
    await vi.advanceTimersByTimeAsync(0);
    expect(document.listenerCount).toBe(1);
    await vi.advanceTimersByTimeAsync(15_000);
    await failed;
    expect(document.listenerCount).toBe(0);
  });

  it("allows retry after timeout and stops microphone tracks from the expired attempt", async () => {
    vi.useFakeTimers();
    const oldMicrophone = deferred<{ getTracks: () => { stop: () => void }[] }>();
    const stop = vi.fn();
    const oldStream = { getTracks: () => [{ stop }] };
    const newStop = vi.fn();
    const newStream = { getTracks: () => [{ stop: newStop }] };
    const discard = (stream: { getTracks: () => { stop: () => void }[] }) => stream.getTracks().forEach((track) => track.stop());
    const first = acquireVisibleMicrophone({
      revealWindow: async () => {},
      document: visibilityDocument(),
      acquire: () => oldMicrophone.promise,
      discard,
    });
    const failed = expect(first).rejects.toThrow("Microphone access timed out");
    await vi.advanceTimersByTimeAsync(15_000);
    await failed;

    const retry = acquireVisibleMicrophone({
      revealWindow: async () => {},
      document: visibilityDocument(),
      acquire: async () => newStream,
      discard,
    });
    await expect(retry).resolves.toBe(newStream);
    oldMicrophone.resolve(oldStream);
    await vi.advanceTimersByTimeAsync(0);
    expect(stop).toHaveBeenCalledOnce();
    expect(newStop).not.toHaveBeenCalled();
  });

  it("closes an AudioContext whose resume promise remains pending", async () => {
    vi.useFakeTimers();
    const resumed = deferred<void>();
    const context = {
      resume: vi.fn(() => resumed.promise),
      close: vi.fn(async () => {}),
    };
    const started = resumeRecordingAudioContext(context);
    const failed = expect(started).rejects.toThrow("Audio capture startup timed out");
    await vi.advanceTimersByTimeAsync(5_000);
    await failed;
    expect(context.close).toHaveBeenCalledOnce();
    resumed.resolve();
    await vi.advanceTimersByTimeAsync(0);
    expect(context.close).toHaveBeenCalledOnce();
  });

  it("closes an AudioContext when resume rejects", async () => {
    const context = {
      resume: vi.fn(async () => { throw new Error("Audio capture denied"); }),
      close: vi.fn(async () => {}),
    };
    await expect(resumeRecordingAudioContext(context)).rejects.toThrow("Audio capture denied");
    expect(context.close).toHaveBeenCalledOnce();
  });

  it("keeps a successfully resumed AudioContext open for the recording", async () => {
    const context = { resume: vi.fn(async () => {}), close: vi.fn(async () => {}) };
    await resumeRecordingAudioContext(context);
    expect(context.resume).toHaveBeenCalledOnce();
    expect(context.close).not.toHaveBeenCalled();
  });

  it("clears the startup deadline after success without limiting an active session", async () => {
    vi.useFakeTimers();
    await expect(waitForStartupStep("Microphone access", async () => "stream")).resolves.toBe("stream");
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(vi.getTimerCount()).toBe(0);
  });
});
