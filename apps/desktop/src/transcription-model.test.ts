import { describe, expect, it } from "vitest";
import {
  DEFAULT_TRANSCRIPTION_MODEL,
  TRANSCRIPTION_MODELS,
  getModelCapabilities,
  needsApiKey,
} from "./transcription-model";

describe("transcription model capabilities", () => {
  it("defaults to the small multilingual local model", () => {
    expect(DEFAULT_TRANSCRIPTION_MODEL).toBe("whisper-base-local");
    expect(TRANSCRIPTION_MODELS).toContain(DEFAULT_TRANSCRIPTION_MODEL);
    expect(TRANSCRIPTION_MODELS).toHaveLength(6);
  });

  it.each(["whisper-base-local", "whisper-tiny-local", "whisper-turbo-local"] as const)(
    "%s transcribes without an API key or cloud operations",
    (model) => {
      expect(needsApiKey(model, false)).toBe(false);
      expect(needsApiKey(model, true)).toBe(false);
      expect(getModelCapabilities(model)).toEqual({
        requiresApiKey: false,
        streamsText: false,
        allowsCloudRefinement: false,
        allowsAutomaticUpdates: false,
      });
    },
  );

  it.each([
    "gpt-4o-mini-transcribe",
    "gpt-4o-transcribe",
    "gpt-realtime-transcribe",
  ] as const)("%s still requires a configured API key", (model) => {
    expect(needsApiKey(model, false)).toBe(true);
    expect(needsApiKey(model, true)).toBe(false);
    expect(getModelCapabilities(model)).toMatchObject({
      requiresApiKey: true,
      allowsCloudRefinement: true,
      allowsAutomaticUpdates: true,
    });
  });

  it("streams text only for the realtime cloud model", () => {
    const streamingModels = TRANSCRIPTION_MODELS.filter(
      (model) => getModelCapabilities(model).streamsText,
    );
    expect(streamingModels).toEqual(["gpt-realtime-transcribe"]);
  });
});
