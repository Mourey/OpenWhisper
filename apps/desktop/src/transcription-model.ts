export const TRANSCRIPTION_MODELS = [
  "whisper-base-local",
  "whisper-tiny-local",
  "whisper-turbo-local",
  "gpt-4o-mini-transcribe",
  "gpt-4o-transcribe",
  "gpt-realtime-transcribe",
] as const;

export type TranscriptionModel = (typeof TRANSCRIPTION_MODELS)[number];

export const DEFAULT_TRANSCRIPTION_MODEL: TranscriptionModel = "whisper-base-local";

export function getModelCapabilities(model: TranscriptionModel) {
  const local = model === "whisper-base-local" || model === "whisper-tiny-local" || model === "whisper-turbo-local";
  return {
    requiresApiKey: !local,
    streamsText: model === "gpt-realtime-transcribe",
    allowsCloudRefinement: !local,
    allowsAutomaticUpdates: !local,
  };
}

export function needsApiKey(model: TranscriptionModel, hasApiKey: boolean) {
  return getModelCapabilities(model).requiresApiKey && !hasApiKey;
}
