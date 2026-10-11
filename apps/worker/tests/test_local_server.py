from __future__ import annotations

import base64
import os
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from openwhisper_worker.local_client import OpenWhisperLocal
from openwhisper_worker.protocol import WorkerRequest
from openwhisper_worker.server import WorkerServer


def request(method: str, **params) -> WorkerRequest:
    return WorkerRequest(id=method, method=method, params=params)


class LocalWorkerServerTests(unittest.TestCase):
    def test_local_backend_is_selected_without_constructing_openai(self) -> None:
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-base-local"}, clear=True):
            with patch("openwhisper_worker.server.OpenWhisperOpenAI") as remote:
                server = WorkerServer()
        self.assertIsInstance(server._openai, OpenWhisperLocal)
        remote.assert_not_called()
        self.assertEqual(server.handle(request("list_models")).result["models"], [
            "whisper-base-local", "whisper-tiny-local", "whisper-turbo-local",
        ])

    def test_turbo_transcribes_captured_pcm_without_openai_or_cloud_refinement(self) -> None:
        pcm = b"\x01\x00" * 240

        def transcribe(path, *, mime_type, prompt, on_progress):
            self.assertEqual(Path(path).read_bytes(), pcm)
            self.assertEqual(mime_type, "audio/pcm;rate=24000")
            self.assertEqual(prompt, "OpenWhisper")
            on_progress(1, 1)
            return "Turbo transcript."

        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-turbo-local"}, clear=True):
            with patch("openwhisper_worker.server.OpenWhisperOpenAI") as remote:
                server = WorkerServer()
                self.assertIsInstance(server._openai, OpenWhisperLocal)
                self.assertEqual(server._openai.model, "whisper-turbo-local")
                with patch.object(server._openai, "transcribe_file", side_effect=transcribe), patch.object(
                    server._openai, "polish"
                ) as polish:
                    started = server.handle(request(
                        "start_session", refine_enabled=True, refine_prompt="polish", prompt="OpenWhisper",
                    ))
                    self.assertTrue(started.ok)
                    session_id = started.result["session_id"]
                    path = Path(server._sessions[session_id].audio_path)
                    self.addCleanup(path.unlink, missing_ok=True)
                    self.assertFalse(server._sessions[session_id].refine_enabled)
                    appended = server.handle(request(
                        "append_audio_chunk", session_id=session_id,
                        chunk_base64=base64.b64encode(pcm).decode("ascii"),
                    ))
                    self.assertTrue(appended.ok)
                    finalized = server.handle(request(
                        "finalize_session_audio", session_id=session_id, mime_type="audio/pcm;rate=24000",
                    ))
                    self.assertEqual(finalized.result["final_text"], "Turbo transcript.")
                    polish.assert_not_called()
                remote.assert_not_called()
        events = server.handle(request("poll_session_events", session_id=session_id))
        self.assertIn({"type": "final", "text": "Turbo transcript."}, events.result["events"])
        self.assertTrue(events.result["done"])
        self.assertFalse(path.exists())

    def test_saved_refinement_setting_is_ignored_for_local_sessions(self) -> None:
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-tiny-local"}, clear=True):
            server = WorkerServer()
        server._openai.transcribe_file = Mock(return_value="Local transcript.")
        server._openai.polish = Mock(side_effect=AssertionError("Must not refine local audio"))
        started = server.handle(request("start_session", refine_enabled=True, refine_prompt="polish"))
        self.assertTrue(started.ok)
        session_id = started.result["session_id"]
        path = Path(server._sessions[session_id].audio_path)
        self.addCleanup(path.unlink, missing_ok=True)
        self.assertFalse(server._sessions[session_id].refine_enabled)
        appended = server.handle(request(
            "append_audio_chunk", session_id=session_id,
            chunk_base64=base64.b64encode(b"\x00\x00").decode("ascii"),
        ))
        self.assertTrue(appended.ok)
        result = server.handle(request(
            "finalize_session_audio", session_id=session_id, mime_type="audio/pcm;rate=24000",
        ))
        self.assertEqual(result.result["final_text"], "Local transcript.")
        server._openai.polish.assert_not_called()
        events = server.handle(request("poll_session_events", session_id=session_id))
        self.assertIn({"type": "final", "text": "Local transcript."}, events.result["events"])
        self.assertTrue(events.result["done"])
        self.assertFalse(path.exists())

    def test_local_transcription_error_is_pollable_and_audio_is_removed(self) -> None:
        with patch.dict(os.environ, {"OPENWHISPER_MODEL": "whisper-base-local"}, clear=True):
            server = WorkerServer()
        server._openai.transcribe_file = Mock(side_effect=RuntimeError("Local model missing"))
        started = server.handle(request("start_session"))
        session_id = started.result["session_id"]
        path = Path(server._sessions[session_id].audio_path)
        self.addCleanup(path.unlink, missing_ok=True)
        server.handle(request(
            "append_audio_chunk", session_id=session_id,
            chunk_base64=base64.b64encode(b"\x00\x00").decode("ascii"),
        ))
        result = server.handle(request("finalize_session_audio", session_id=session_id))
        self.assertEqual(result.result["final_text"], "")
        events = server.handle(request("poll_session_events", session_id=session_id))
        self.assertIn({"type": "error", "text": "Local model missing"}, events.result["events"])
        self.assertFalse(path.exists())


if __name__ == "__main__":
    unittest.main()
