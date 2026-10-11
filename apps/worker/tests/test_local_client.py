from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

from openwhisper_worker.local_client import LOCAL_MODELS, OpenWhisperLocal


class OpenWhisperLocalTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="openwhisper-local-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.trace = self.root / "trace.json"
        self.whisper = self.make_executable(
            "whisper-cli",
            """import json, os, pathlib, sys, wave
args = sys.argv[1:]
source = pathlib.Path(args[args.index('--file') + 1])
with wave.open(str(source), 'rb') as audio:
    details = dict(rate=audio.getframerate(), channels=audio.getnchannels(),
                   width=audio.getsampwidth(), frames=audio.getnframes())
details.update(args=args, input=str(source))
pathlib.Path(os.environ['LOCAL_TEST_TRACE']).write_text(json.dumps(details))
print('CLI diagnostics on stdout')
print('CLI diagnostics on stderr', file=sys.stderr)
pathlib.Path(args[args.index('--output-file') + 1] + '.txt').write_text('  Hello local world.\\n')
""",
        )
        self.ffmpeg = self.make_executable(
            "ffmpeg",
            """import sys, wave
with wave.open(sys.argv[-1], 'wb') as audio:
    audio.setnchannels(1)
    audio.setsampwidth(2)
    audio.setframerate(16000)
    audio.writeframes(b'\\x00\\x00' * 320)
""",
        )
        for name in LOCAL_MODELS.values():
            (self.root / name).write_bytes(b"test model")
        environment = {
            "OPENWHISPER_MODEL": "whisper-base-local",
            "OPENWHISPER_WHISPER_CLI": str(self.whisper),
            "OPENWHISPER_FFMPEG": str(self.ffmpeg),
            "OPENWHISPER_MODELS_DIR": str(self.root),
            "LOCAL_TEST_TRACE": str(self.trace),
        }
        self.environ = patch.dict(os.environ, environment, clear=True)
        self.environ.start()
        self.addCleanup(self.environ.stop)
        self.client = OpenWhisperLocal()

    def make_executable(self, name: str, script: str) -> Path:
        path = self.root / name
        path.write_text(f"#!{sys.executable}\n{script}", encoding="utf-8")
        path.chmod(0o755)
        return path

    def test_pcm_transcription_uses_text_file_and_captures_all_child_output(self) -> None:
        progress: list[tuple[int, int]] = []
        path = self.root / "input.pcm"
        path.write_bytes(b"\x00\x00" * 24000)
        stdout, stderr = io.StringIO(), io.StringIO()
        prompt = 'Names: $(touch unexpected) "quoted"; OpenWhisper'
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            result = self.client.transcribe_file(
                path, "audio/pcm;rate=24000", prompt,
                on_progress=lambda current, total: progress.append((current, total)),
            )
        self.assertEqual(result, "Hello local world.")
        self.assertEqual(progress, [(1, 1)])
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(stderr.getvalue(), "")
        trace = json.loads(self.trace.read_text())
        self.assertEqual((trace["rate"], trace["channels"], trace["width"]), (16000, 1, 2))
        self.assertEqual(trace["args"][trace["args"].index("--prompt") + 1], prompt)
        self.assertEqual(trace["args"][trace["args"].index("--language") + 1], "auto")
        self.assertFalse(Path(trace["input"]).parent.exists())
        self.assertTrue(path.exists())

    def test_each_transcription_has_a_unique_temporary_output(self) -> None:
        self.client.transcribe_bytes(b"\x00\x00" * 10, "audio/pcm;rate=24000")
        first = json.loads(self.trace.read_text())["input"]
        self.client.transcribe_bytes(b"\x00\x00" * 10, "audio/pcm;rate=24000")
        second = json.loads(self.trace.read_text())["input"]
        self.assertNotEqual(first, second)
        self.assertFalse(Path(first).exists())
        self.assertFalse(Path(second).exists())

    def test_invalid_pcm_is_rejected_before_whisper_runs(self) -> None:
        for data, mime, message in (
            (b"a", "audio/pcm;rate=24000", "complete 16-bit"),
            (b"ab", "audio/pcm", "missing its sample rate"),
            (b"ab", "audio/pcm;rate=0", "invalid sample rate"),
            (b"ab", "audio/pcm;rate=9999999", "invalid sample rate"),
        ):
            with self.subTest(mime=mime), self.assertRaisesRegex(RuntimeError, message):
                self.client.transcribe_bytes(data, mime)
        self.assertFalse(self.trace.exists())

    def test_empty_missing_and_oversize_inputs_are_rejected(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "No audio bytes"):
            self.client.transcribe_bytes(b"", "audio/wav")
        with self.assertRaisesRegex(RuntimeError, "No audio bytes"):
            self.client.transcribe_file(self.root / "missing", "audio/wav")
        with patch("openwhisper_worker.local_client._MAX_AUDIO_BYTES", 2):
            with self.assertRaisesRegex(RuntimeError, "512 MB"):
                self.client.transcribe_bytes(b"abcd", "audio/wav")

    def test_decoder_cannot_open_network_protocols(self) -> None:
        with patch("openwhisper_worker.local_client.subprocess.run") as run:
            run.return_value.returncode = 1
            run.return_value.stderr = "Protocol https not on whitelist"
            with self.assertRaisesRegex(RuntimeError, "Audio conversion failed"):
                self.client.transcribe_bytes(b"https://example.invalid/audio", "audio/wav")
        args = run.call_args.args[0]
        self.assertEqual(args[args.index("-protocol_whitelist") + 1], "file,pipe")
        self.assertFalse(run.call_args.kwargs.get("shell", False))

    def test_timeout_terminates_cli_and_removes_temporary_audio(self) -> None:
        self.make_executable(
            "whisper-cli",
            """import json, os, pathlib, sys, time
args = sys.argv[1:]
pathlib.Path(os.environ['LOCAL_TEST_TRACE']).write_text(json.dumps(
    dict(pid=os.getpid(), input=args[args.index('--file') + 1])))
time.sleep(30)
""",
        )
        with patch("openwhisper_worker.local_client._TRANSCRIBE_TIMEOUT_SECONDS", 1):
            with self.assertRaisesRegex(RuntimeError, "Local transcription timed out"):
                self.client.transcribe_bytes(b"\x00\x00" * 10, "audio/pcm;rate=24000")
        trace = json.loads(self.trace.read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(trace["pid"], 0)
        self.assertFalse(Path(trace["input"]).parent.exists())

    def test_missing_cli_and_model_return_actionable_errors(self) -> None:
        with patch.dict(os.environ, {"OPENWHISPER_WHISPER_CLI": str(self.root / "missing")}):
            with self.assertRaisesRegex(RuntimeError, "whisper-cli executable is missing"):
                self.client.health()
        (self.root / LOCAL_MODELS[self.client.model]).unlink()
        with self.assertRaisesRegex(RuntimeError, "ggml-base-q5_1.bin is missing"):
            self.client.health()

    def test_frozen_worker_discovers_only_bundled_assets_without_overrides(self) -> None:
        assets = self.root / "bundle"
        assets.mkdir()
        for executable in (self.whisper, self.ffmpeg):
            shutil.copy2(executable, assets / executable.name)
        (assets / "models").mkdir()
        shutil.copy2(self.root / LOCAL_MODELS[self.client.model], assets / "models")
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-base-local"}, clear=True):
            with patch.object(sys, "frozen", True, create=True), patch.object(
                sys, "executable", str(assets / "openwhisper-worker")
            ):
                whisper, ffmpeg, model = self.client._runtime_paths()
        self.assertEqual(whisper, (assets / "whisper-cli").resolve())
        self.assertEqual(ffmpeg, (assets / "ffmpeg").resolve())
        self.assertEqual(model, (assets / "models" / LOCAL_MODELS[self.client.model]).resolve())

    def test_supported_local_models_health_and_polish_do_not_need_a_key(self) -> None:
        self.assertEqual(self.client.available_models(), list(LOCAL_MODELS))
        self.assertEqual(self.client.health()["backend"], "local")
        self.assertIsNone(self.client.health()["polish_model"])
        self.assertEqual(self.client.polish("Hello.", "ignored"), "Hello.")
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-tiny-local"}):
            self.assertEqual(OpenWhisperLocal().model, "whisper-tiny-local")
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "unknown"}):
            with self.assertRaisesRegex(ValueError, "Unsupported local"):
                OpenWhisperLocal()

    def test_base_remains_the_default_when_no_local_model_is_selected(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(OpenWhisperLocal().model, "whisper-base-local")

    def test_frozen_worker_resolves_turbo_from_its_bundled_models_directory(self) -> None:
        assets = self.root / "turbo-bundle"
        assets.mkdir()
        for executable in (self.whisper, self.ffmpeg):
            shutil.copy2(executable, assets / executable.name)
        models = assets / "models"
        models.mkdir()
        expected = models / "ggml-large-v3-turbo-q5_0.bin"
        expected.write_bytes(b"turbo test model")
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-turbo-local"}, clear=True):
            with patch.object(sys, "frozen", True, create=True), patch.object(
                sys, "executable", str(assets / "openwhisper-worker")
            ):
                client = OpenWhisperLocal()
                whisper, ffmpeg, model = client._runtime_paths()
                self.assertEqual(client.health()["model"], "whisper-turbo-local")
        self.assertEqual(whisper, (assets / "whisper-cli").resolve())
        self.assertEqual(ffmpeg, (assets / "ffmpeg").resolve())
        self.assertEqual(model, expected.resolve())

    def test_turbo_resamples_recorded_pcm_and_invokes_only_its_local_model(self) -> None:
        ffmpeg = shutil.which("ffmpeg", path=os.defpath + ":/opt/homebrew/bin")
        if not ffmpeg:
            self.skipTest("ffmpeg is not installed")
        with patch.dict(os.environ, {
            "OPENWHISPER_MODEL": "whisper-turbo-local",
            "OPENWHISPER_FFMPEG": ffmpeg,
        }):
            with patch("openwhisper_worker.openai_client.OpenAI") as remote:
                client = OpenWhisperLocal()
                text = client.transcribe_bytes(b"\x01\x00" * 24000, "audio/pcm;rate=24000")
        remote.assert_not_called()
        self.assertEqual(text, "Hello local world.")
        trace = json.loads(self.trace.read_text())
        self.assertEqual(
            (trace["rate"], trace["channels"], trace["width"], trace["frames"]),
            (16000, 1, 2, 16000),
        )
        self.assertEqual(
            Path(trace["args"][trace["args"].index("--model") + 1]),
            (self.root / "ggml-large-v3-turbo-q5_0.bin").resolve(),
        )
        self.assertFalse(Path(trace["input"]).parent.exists())

    def test_real_ffmpeg_resamples_recording_and_decodes_imported_flac(self) -> None:
        ffmpeg = shutil.which("ffmpeg", path=os.defpath + ":/opt/homebrew/bin")
        if not ffmpeg:
            self.skipTest("ffmpeg is not installed")
        pcm = b"\x01\x00" * 24000
        with patch.dict(os.environ, {"OPENWHISPER_FFMPEG": ffmpeg}):
            self.client.transcribe_bytes(pcm, "audio/pcm;rate=24000")
            trace = json.loads(self.trace.read_text())
            self.assertEqual(trace["frames"], 16000)
            source = self.root / "stereo.wav"
            with wave.open(str(source), "wb") as audio:
                audio.setnchannels(2)
                audio.setsampwidth(2)
                audio.setframerate(48000)
                audio.writeframes(b"\x01\x00\x02\x00" * 48000)
            compressed = self.root / "import.flac"
            subprocess.run(
                [ffmpeg, "-nostdin", "-v", "error", "-i", str(source), str(compressed)],
                check=True, capture_output=True,
            )
            self.assertEqual(self.client.transcribe_file(compressed, "audio/flac"), "Hello local world.")
            trace = json.loads(self.trace.read_text())
            self.assertEqual((trace["frames"], trace["channels"], trace["rate"]), (16000, 1, 16000))


if __name__ == "__main__":
    unittest.main()
