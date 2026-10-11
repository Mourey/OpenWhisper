# Local macOS installation

This checkout includes local packaging settings for Apple Silicon Macs. The
installed app is `/Applications/OpenWhisper.app`.

Open the app and allow microphone access when macOS prompts. This Mac is
configured to use **Local — Whisper Large v3 Turbo**, a 574 MB quantized
multilingual model. No API key, account, or network connection is needed for
local transcription. The model menu also offers **Whisper Base** (60 MB) and
**Whisper Tiny** (32 MB). Base remains the source default for fresh settings.

Hold the **Right Control** key to record from any app, then release to transcribe.
Capture begins on button-down; using Control with another key cancels
the gesture so normal keyboard shortcuts can continue. Right Control always
uses hold-to-talk. Enable OpenWhisper under **System Settings → Privacy &
Security → Input Monitoring** when prompted; an existing Accessibility grant
can also permit this listener. The listener passes all keyboard events through
and does not store typed keys. While permission is missing, Right Control stays
selected and Settings shows the permission error. The app retries automatically
after access is granted.

The **Recording shortcut** menu also offers **Control+Option+Space** and
**Control+Option+W**; those choices support hold-to-talk or press-to-toggle. If
macOS rejects a combination shortcut at startup, OpenWhisper
tries the other choice; the active choice appears in Settings. If both fail,
Settings shows an error and the microphone button still works. A failed shortcut
change keeps the current binding. macOS can allow multiple apps to register the
same shortcut, so choose the other option if another app responds too.

You can also click the microphone to record. Transcription runs locally when
recording stops. On macOS, OpenWhisper inserts the finished transcript into the
focused text field captured when recording began, and shows a four-second
preview. Enable OpenWhisper under **System Settings → Privacy & Security →
Accessibility** to allow insertion. The preview offers **Enable Accessibility**
when permission is missing. It never restores focus to another app: if you move
to another field or hold another modifier, the transcript stays on the clipboard
and the preview says to use **Command+V**. Imported recordings are copy-only.
Windows uses Ctrl+Space and automatic insertion into another app. If the pill is
hidden, use the menu bar icon's **Show Pill** action.

Turn off **Show pill when idle** in Appearance to keep the orb hidden between
recordings. It becomes visible during recording startup, recording, and
transcription, and the brief transcript preview, then hides again. Starting
another recording dismisses the previous preview. Startup shows the window before requesting
microphone access: WebKit defers media acquisition while its document is hidden,
consistent with the [Media Capture visibility requirement](https://www.w3.org/TR/mediacapture-streams/#dom-mediadevices-getusermedia).
The microphone list no longer requests capture permission at app launch.

Local mode disables cloud text refinement and automatic update checks. The
OpenAI models remain optional menu choices and require a key if selected.
Transcription accuracy depends on language, accent, noise, and microphone
quality; the speed check below used a public English fixture, not your voice.

The app contains its Python worker, a standalone Metal-enabled whisper.cpp
1.9.2 executable, ffmpeg and its relocated libraries, and all three model files.
It does not need Homebrew, a terminal, or a virtual environment to run.
No API key or `.env` file was created during installation.

To rebuild from the repository root with Node.js, Rust, uv, and Xcode Command
Line Tools installed, first ensure the native assets under
`apps/desktop/src-tauri/binaries/` are present. They are already retained in
this checkout. If they are missing, recover them from the installed app:

```bash
ditto /Applications/OpenWhisper.app/Contents/Resources/binaries \
  apps/desktop/src-tauri/binaries
```

Then rebuild:

```bash
cd apps/worker
uv sync --locked
uv run --locked pyinstaller --noconfirm --onefile \
  --name openwhisper-worker --paths src --specpath build/macos \
  --distpath ../desktop/src-tauri/binaries \
  src/openwhisper_worker/__main__.py
cd ../desktop
npm ci
npx tauri build --config src-tauri/tauri.macos.conf.json --bundles app --ci
```

The app bundle is produced at
`apps/desktop/src-tauri/target/release/bundle/macos/OpenWhisper.app`.
For a fresh installation, sign the local bundle and copy it into Applications
from the repository root. Preserve an existing installation before replacing it.

```bash
codesign --force --sign - \
  --entitlements apps/desktop/src-tauri/Entitlements.plist \
  apps/desktop/src-tauri/target/release/bundle/macos/OpenWhisper.app
ditto apps/desktop/src-tauri/target/release/bundle/macos/OpenWhisper.app \
  /Applications/OpenWhisper.app
codesign --verify --deep --strict /Applications/OpenWhisper.app
open /Applications/OpenWhisper.app
```

`Info.plist` declares microphone access, `Entitlements.plist` enables audio
input, and `tauri.macos.conf.json` selects the app bundle and transparent window
support. The Tauri CLI adds `macos-private-api` to the Cargo dependency features.
Signed updater artifacts are disabled for this local build; rebuild locally to
update this Mac installation.

This bundle is ad hoc signed, so a rebuild changes its signing identity. macOS
can reject an old permission record even when OpenWhisper's switch is enabled.
If re-enabling access does not restore Right Control, reset only this app's
permissions using Apple's [protected-resource reset procedure](https://developer.apple.com/documentation/xcode/resetting-access-to-protected-resources-in-macos):

```bash
tccutil reset ListenEvent com.openwhisper.app
tccutil reset Microphone com.openwhisper.app
tccutil reset Accessibility com.openwhisper.app
```

Relaunch OpenWhisper, enable it in Privacy & Security > Input Monitoring, and
choose Quit & Reopen when prompted. Hold Right Control to start recording and
allow microphone access when requested. Confirm an enabled Core Graphics event
tap for the running app rather than relying on the Settings switch alone.
Grant Accessibility again for insertion if its previous permission record was
also rejected after rebuilding.

The packaging settings follow Tauri's [macOS app bundle documentation](https://v2.tauri.app/distribute/macos-application-bundle/)
and [transparent window configuration](https://v2.tauri.app/reference/config/#transparent).

Validation commands:

```bash
cd apps/worker
uv run --locked python -m unittest discover -s tests -v
cd ../desktop
npm test
cd src-tauri
cargo test --release
```

The local client and server tests cover missing-key operation, raw 24 kHz PCM
resampling, compressed imports, disabling refinement, frozen resource paths,
timeouts, and temporary-file cleanup. The installed worker was also checked
through its JSON protocol with Base and Tiny using a public 11-second WAV and
the app's 24 kHz PCM format. Quantized Tiny and Base completed the original
CLI sample in approximately 0.2–0.5 seconds on this M4 Pro; app conversion and
language detection add overhead. In the installed-worker checks, the Base WAV
case took about seven seconds, while the app's 24 kHz PCM case and compressed
FLAC case took about 0.24 seconds. Tiny's WAV and PCM cases took about
0.18–0.19 seconds. These are sample measurements, not latency guarantees.

Turbo was checked through the installed worker with network access blocked,
no API key or runtime-path overrides, and a working directory outside the
checkout. The same public 11-second clip completed in 1.78 seconds as WAV and
1.44 seconds as either the app's 24 kHz PCM format or FLAC. Base completed the
PCM case in 0.23 seconds in this comparison. Both models transcribed the words
correctly; this validates the packaged audio pipeline rather than an accuracy
improvement for your voice. Final events matched the transcript, sessions
completed without errors, and cloud refinement stayed disabled.

Desktop and worker tests, Rust release tests, production builds, and formatting
checks pass. `npx tsc --noEmit` still reports state-narrowing and missing Node
test-type errors; the same failures were reproduced in an isolated checkout of
unchanged upstream HEAD, so they were left outside this local-model change.

Shortcut tests cover platform labels and choices, repeated key presses,
hold/release and toggle modes, occupied-key fallback, and retaining the current
binding after a registration or save failure. Right Control tests distinguish
the physical left and right keys, start capture immediately, cancel keyboard
chords, and stop once on release. Releases before microphone readiness cancel
without transcribing an empty recording. Cancellation tests stop late microphone
streams and pending worker sessions, retain raw backups, and suppress transcript
output. Combination shortcuts on macOS use
the [Tauri global shortcut plugin](https://v2.tauri.app/plugin/global-shortcut/),
registered from Rust without granting frontend shortcut permissions. Right
Control uses a listen-only Core Graphics event tap on its own run-loop thread.

Cursor insertion first uses the captured field's writable Accessibility
selected-text attribute. For supported text fields without that attribute, it
sends a tagged Command+V only to the same focused process. It checks focus and
held modifiers again before insertion; generated paste events cannot cancel or
block the Right Control gesture. Native tests cover permission denial, changed
focus, Unicode, successive captures, modifier checks, CF ownership and result
serialization. Preview tests cover plaintext rendering, honest delivery status,
copy-only imports, cancellation, replacement and expiration.
A separate inactive NSTextView fixture verified native selected-text insertion
and exact Unicode preservation without modifying user apps. Keyboard-event
posting has no editor acknowledgement; a text field can decline the shortcut,
so cursor insertion still needs a check in the editor you use.

With the idle pill hidden, an automatic app-closing utility can mistake
OpenWhisper for an app with no remaining windows. In a repeated-recording check,
Vorssaint's Auto Quit feature sent OpenWhisper a Quit AppleEvent after the first
recording; the app exited normally instead of crashing. Add OpenWhisper
(`com.openwhisper.app`) to Auto Quit's exceptions so it can keep listening while
the pill is hidden. Vorssaint's Add App action refreshes the running service;
an exception written through `defaults` requires restarting Vorssaint because
the service caches its list. This exception preserves the Right Control shortcut and
does not require changing the model or microphone permissions.

Recording startup tests verify reveal-before-capture, visibility through focus
loss, timeouts and retry, stopping microphone streams that arrive after timeout,
and closing failed audio contexts. Backup creation runs alongside microphone
setup, so PCM capture starts as soon as the microphone and audio context are
ready. Separate queues retain opening audio until the raw backup and worker
session each have an ID; attaching either destination does not clear audio.
Onset regressions reproduce delayed backup and worker startup, then verify all
opening samples reach both destinations in order. They also cover cancellation
with a late backup and releases before the first PCM callback. A separate native WKWebView fixture on this
Mac verified hidden-to-visible transitions for an inactive floating window
without activating the app or capturing audio.

The model files are from the [whisper.cpp model repository](https://huggingface.co/ggerganov/whisper.cpp/tree/main):

| File | Size | SHA-256 |
| --- | --- | --- |
| `ggml-base-q5_1.bin` | 59,707,625 bytes | `422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898` |
| `ggml-tiny-q5_1.bin` | 32,152,673 bytes | `818710568da3ca15689e31a743197b520007872ff9576237bda97bd1b469c3d7` |
| `ggml-large-v3-turbo-q5_0.bin` | 574,041,195 bytes | `394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2` |

Development can use Homebrew's `whisper-cli` and `ffmpeg`, and model files in
`target/local-model-eval`. To select a local backend directly in the Python
worker, set `OPENWHISPER_MODEL=whisper-turbo-local`, `whisper-base-local`, or
`whisper-tiny-local`.
`OPENWHISPER_WHISPER_CLI`, `OPENWHISPER_FFMPEG`, and `OPENWHISPER_MODELS_DIR`
override the asset paths when needed.
