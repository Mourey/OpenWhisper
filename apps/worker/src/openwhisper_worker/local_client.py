from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import wave
from pathlib import Path
from typing import Any, Callable


LOCAL_MODELS = {
    "whisper-base-local": "ggml-base-q5_1.bin",
    "whisper-tiny-local": "ggml-tiny-q5_1.bin",
    "whisper-turbo-local": "ggml-large-v3-turbo-q5_0.bin",
}
_MAX_AUDIO_BYTES = 512 * 1024 * 1024
_MAX_AUDIO_SECONDS = 2 * 60 * 60
_DECODE_TIMEOUT_SECONDS = 180
_TRANSCRIBE_TIMEOUT_SECONDS = 30 * 60


class OpenWhisperLocal:
    def __init__(self) -> None:
        self._model = os.getenv("OPENWHISPER_MODEL", "whisper-base-local").strip()
        if self._model not in LOCAL_MODELS:
            raise ValueError(f"Unsupported local transcription model: {self._model}")

    @property
    def model(self) -> str:
        return self._model

    def available_models(self) -> list[str]:
        return list(LOCAL_MODELS)

    def health(self) -> dict[str, Any]:
        self._runtime_paths()
        return {"status": "ok", "model": self._model, "backend": "local", "polish_model": None}

    def polish(self, text: str, custom_prompt: str = "") -> str:
        return text

    def transcribe_bytes(self, audio_bytes: bytes, mime_type: str, prompt: str = "") -> str:
        if not audio_bytes:
            raise RuntimeError("No audio bytes provided")
        if len(audio_bytes) > _MAX_AUDIO_BYTES:
            raise RuntimeError("This audio file is larger than the local 512 MB limit")
        with tempfile.TemporaryDirectory(prefix="openwhisper-local-input-") as directory:
            path = Path(directory) / "input.audio"
            path.write_bytes(audio_bytes)
            return self.transcribe_file(path, mime_type, prompt)

    def transcribe_file(
        self,
        path: str | Path,
        mime_type: str,
        prompt: str = "",
        on_progress: Callable[[int, int], None] | None = None,
    ) -> str:
        audio_path = Path(path).resolve()
        if not audio_path.is_file() or audio_path.stat().st_size == 0:
            raise RuntimeError("No audio bytes provided")
        if audio_path.stat().st_size > _MAX_AUDIO_BYTES:
            raise RuntimeError("This audio file is larger than the local 512 MB limit")
        whisper, ffmpeg, model = self._runtime_paths()
        with tempfile.TemporaryDirectory(prefix="openwhisper-local-") as directory:
            wav_path = Path(directory) / "audio.wav"
            output_path = Path(directory) / "transcript"
            command = [str(ffmpeg), "-nostdin", "-v", "error", "-y"]
            if mime_type.lower().startswith("audio/pcm"):
                if audio_path.stat().st_size % 2:
                    raise RuntimeError("PCM audio must contain complete 16-bit samples")
                command.extend(
                    ["-f", "s16le", "-ar", str(self._sample_rate_from_mime(mime_type)), "-ac", "1"]
                )
            command.extend(
                [
                    "-protocol_whitelist", "file,pipe", "-i", str(audio_path),
                    "-t", str(_MAX_AUDIO_SECONDS + 1), "-vn", "-ac", "1", "-ar", "16000",
                    "-c:a", "pcm_s16le", str(wav_path),
                ]
            )
            self._run(command, _DECODE_TIMEOUT_SECONDS, "Audio conversion")
            with wave.open(str(wav_path), "rb") as audio:
                if not audio.getnframes():
                    raise RuntimeError("No audio bytes provided")
                if audio.getnframes() / audio.getframerate() > _MAX_AUDIO_SECONDS:
                    raise RuntimeError("Local transcription supports audio up to two hours long")
            if on_progress:
                on_progress(1, 1)
            command = [
                str(whisper), "--model", str(model), "--file", str(wav_path),
                "--language", "auto", "--output-txt", "--output-file", str(output_path),
                "--no-prints", "--no-timestamps",
            ]
            if prompt.strip():
                command.extend(["--prompt", prompt[:4000]])
            self._run(command, _TRANSCRIBE_TIMEOUT_SECONDS, "Local transcription")
            transcript_path = output_path.with_suffix(".txt")
            if not transcript_path.is_file():
                raise RuntimeError("Local transcription did not produce a transcript")
            return transcript_path.read_text(encoding="utf-8").strip()

    @staticmethod
    def _sample_rate_from_mime(mime_type: str) -> int:
        match = re.search(r"(?:^|;)\s*(?:rate|samplerate)\s*=\s*(\d+)\s*(?:;|$)", mime_type, re.I)
        if not match:
            raise RuntimeError("PCM audio is missing its sample rate")
        rate = int(match.group(1))
        if not 8000 <= rate <= 192000:
            raise RuntimeError("PCM audio has an invalid sample rate")
        return rate

    @staticmethod
    def _asset_roots() -> list[Path]:
        executable_dir = Path(sys.executable).resolve().parent
        roots = [executable_dir, executable_dir.parent / "Resources"]
        if hasattr(sys, "_MEIPASS"):
            roots.append(Path(sys._MEIPASS))
        if not getattr(sys, "frozen", False):
            roots.append(Path(__file__).resolve().parents[4] / "target" / "local-model-eval")
        return roots

    @classmethod
    def _executable_path(cls, name: str, env_name: str) -> Path:
        override = os.getenv(env_name)
        if override:
            candidates = [Path(override).expanduser()]
        else:
            candidates = []
            for root in cls._asset_roots():
                candidates.extend([root / "whisper-runtime" / name, root / name])
            if not getattr(sys, "frozen", False):
                candidates.append(Path("/opt/homebrew/bin") / name)
                if installed := shutil.which(name):
                    candidates.append(Path(installed))
        for path in candidates:
            if path.is_file() and os.access(path, os.X_OK):
                return path.resolve()
        raise RuntimeError(f"Local {name} executable is missing; reinstall OpenWhisper")

    def _runtime_paths(self) -> tuple[Path, Path, Path]:
        whisper = self._executable_path("whisper-cli", "OPENWHISPER_WHISPER_CLI")
        ffmpeg = self._executable_path("ffmpeg", "OPENWHISPER_FFMPEG")
        override = os.getenv("OPENWHISPER_MODELS_DIR")
        roots = [Path(override).expanduser()] if override else self._asset_roots()
        for root in roots:
            for model in (root / "models" / LOCAL_MODELS[self._model], root / LOCAL_MODELS[self._model]):
                if model.is_file() and model.stat().st_size:
                    return whisper, ffmpeg, model.resolve()
        raise RuntimeError(f"Local model {LOCAL_MODELS[self._model]} is missing; reinstall OpenWhisper")

    @staticmethod
    def _run(command: list[str], timeout: int, operation: str) -> None:
        try:
            result = subprocess.run(
                command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                timeout=timeout, check=False, text=True, encoding="utf-8", errors="replace",
            )
        except subprocess.TimeoutExpired as exc:
            raise RuntimeError(f"{operation} timed out; try a shorter recording") from exc
        except OSError as exc:
            raise RuntimeError(f"{operation} could not start: {exc}") from exc
        if result.returncode:
            detail = result.stderr.strip()[-1200:]
            raise RuntimeError(f"{operation} failed" + (f": {detail}" if detail else ""))
