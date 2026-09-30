# Meeting Transcriber

A macOS and Windows app that records the microphone ("me") and system audio ("them") as separate streams and transcribes both live with Soniox. Design notes: `../notes/mac-meeting-transcriber-design.md`.

Current stage: **live transcription**. It records both sources to separate WAV files, streams each to its own Soniox session (`stt-rt-v5`), and shows one merged live transcript.

## Layout

| Path | What it is |
|---|---|
| `crates/capture` | Library. Mic and system-audio capture via cpal, one WAV writer thread per stream. No UI code, so it can later sit behind UniFFI or another front end. |
| `crates/soniox` | Soniox real-time WebSocket client. Encodes audio to 16 kHz mono `pcm_s16le` and reports final and provisional tokens. |
| `crates/app` | The egui desktop app (`meeting-transcriber` binary) and its `Info.plist`. `live.rs` wires capture to Soniox, and `transcript.rs` merges the two streams. |
| `scripts/bundle.sh` | Builds and signs `target/release/Meeting Transcriber.app`. |

## Run on Windows

Requires Windows 10 or 11 and Rust (MSVC toolchain, installed with [rustup](https://rustup.rs)).

```powershell
cargo build --release -p meeting-transcriber
.\target\release\meeting-transcriber.exe
```

System audio comes from WASAPI loopback on the default output device, and needs no permission. If the mic stays silent, turn on Settings › Privacy & security › Microphone › Let desktop apps access your microphone. The API key is saved in Windows Credential Manager. Recordings go to `%LOCALAPPDATA%\Meeting Transcriber\Recordings\<timestamp>\`.

### Cross-building the Windows app on macOS

`scripts/build-windows.sh` builds `dist/meeting-transcriber-windows-x64.zip` with the MinGW toolchain (the script lists the one-time setup). The script checks that the `.exe` only uses DLLs that ship with Windows. The binary is not code-signed, so SmartScreen warns the first time it runs.

## Run on macOS

Build and open the app bundle:

```bash
scripts/bundle.sh --run
```

Use the bundle for testing system audio. macOS grants the System Audio Recording permission to the app that asks for it. If you start the binary with `cargo run`, that app is your terminal, and without the permission the system track records silence with no error.

`cargo run -p meeting-transcriber` is fine for UI work and the microphone.

On first start, macOS asks for microphone access, and for system audio access when you start recording. If you missed a prompt, turn the app on under System Settings › Privacy & Security › Microphone, or › Screen & System Audio Recording.

Paste your Soniox API key under **Transcription settings** and click **Save to Keychain** (**Save to Credential Manager** on Windows). With `cargo run`, the app also reads `SONIOX_API_KEY` from the environment.

Recordings go to `~/Library/Application Support/Meeting Transcriber/Recordings/<timestamp>/`, as `mic.wav` and `system.wav` (32-bit float WAV at the device's native rate and channel count), plus `transcript.jsonl` with one finished segment per line.

To check rendering without recording, run a debug build with sample lines: `MEETING_TRANSCRIBER_DEMO=1 cargo run -p meeting-transcriber`.

To check the Soniox connection without the UI:

```bash
SONIOX_API_KEY=... cargo run -p soniox --example transcribe_wav -- some.wav
```

## How transcription works

- Mic ("me") and system audio ("them") are separate Soniox streams, so you never depend on diarization to find your own lines. This costs roughly twice the audio time.
- The mic stream uses endpoint detection (`<end>` tokens) to split lines, with no diarization.
- The system stream uses speaker diarization and no endpoint detection, because Soniox says endpoint detection lowers diarization accuracy. Its lines split on speaker changes and pauses of 1.5 s or more.
- Final tokens are appended. Provisional tokens are shown in grey italics and replaced on every update.
- Language hints and a terms list (sent as `context.terms`) are set in the UI. Language identification is on.
- If a connection drops, the stream reconnects on its own and resends the audio Soniox hadn't finalized yet, so no words are lost or repeated and timestamps stay continuous. Network errors, timeouts, rate limits, Soniox 5xx errors and the 300-minute stream limit are retried. A bad key, missing credit or bad request is reported instead. The status shows "reconnecting…" and later "live · reconnected N×". Hover over it to see why the connection dropped.
- Each recording has its own transcript. If you start a new recording before the previous one's streams finish, the old transcript still gets its last lines.

## Translation

Under **Transcription settings › Translation**, pick one of three modes:

| Mode | What you get |
|---|---|
| Off | Transcript only |
| One-way | Everything spoken, in any language, translated into the language you pick |
| Two-way | Between the two languages you pick: each one is translated into the other |

Each line shows its translation underneath, in the main window and the always-on-top window. **Show translations** hides or shows them, and works during a recording. The mode and languages apply from the next recording, because Soniox sets them when a stream starts. Translated text is billed as Soniox output text, adding roughly $0.06 per meeting hour.

Translations are saved as a `translation` field in `transcript.jsonl`. Translated text trails the speech and has no timestamps, so the app attaches it to the newest line from the same speaker, and writes each line once the next line from that stream has finished.

Settings other than the API key are saved to `settings.json` in the app data folder (`~/Library/Application Support/Meeting Transcriber/` or `%LOCALAPPDATA%\Meeting Transcriber\`).

## Always-on-top transcript

**Always-on-top transcript** opens a small window with the live transcript that stays above other apps, including full-screen Zoom or Teams calls. On macOS it also follows you across Spaces. Use the size slider next to the button to change its text size. The window keeps updating while the main window is minimized.

## Fonts

The UI loads system fonts that cover Vietnamese: SF and SF Mono on macOS, Segoe UI and Consolas on Windows. Arial Unicode (macOS) or Microsoft YaHei (Windows) is the fallback for CJK. egui's built-in fonts lack many Vietnamese letters.

## Requirements

- macOS 14.6 or later (Core Audio process taps), or Windows 10/11.
- Rust 1.88 or later (edition 2024, let-chains).

## Known limitations

- **Windows support has not been tested on a Windows machine yet.**
- **System audio comes from the default output device at the moment you press Start.** If you switch output mid-recording (for example, plugging in headphones), the system track stops receiving audio. Stop and start again.
- **Bluetooth headsets as the mic** (AirPods, Shokz…): recording from the headset's mic switches it to call mode, which lowers playback quality and changes its sample rate. The app warns about this and retries the system-audio tap for up to about 3 s while the headset settles. The built-in MacBook microphone avoids the problem.
- **Output devices that also have a microphone** (some USB headsets, macOS only): cpal records that device's microphone instead of tapping its output. The app shows a warning when this applies.
- **No echo cancellation or de-duplication yet.** On speakers, the mic track picks up the remote side, and those words show up twice (as "Me" and as "Them"). Use headphones for now.
- **Translation mode can't change mid-recording.** Stop and start to switch modes or languages. **Show translations** works any time.
- **Reconnecting needs the network back within the meeting.** While recording, the app keeps retrying (waits of 0.5 s growing to 15 s) and queues audio in memory, about 2 MB per minute for the two streams. After Stop it tries 3 more times to finish the queued audio, then gives up on the live transcript. The WAV files are never affected.
