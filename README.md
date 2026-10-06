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

The window title shows the build time ("built Sep 30 16:41"), so you can tell when an older build is still running.

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
- If a connection drops, or Soniox sends nothing for 10 s, the stream reconnects on its own. Soniox normally reports progress about once a second, even in silence, so 10 s of quiet means a stalled connection. Retried: network errors, timeouts, rate limits, Soniox 5xx errors and the 300-minute stream limit. A bad key, missing credit or bad request is reported instead.
- After reconnecting, the stream resends up to the last 60 s of audio Soniox hadn't finalized, so no words are lost or repeated, and timestamps stay continuous. It resends no more than that because Soniox works through a backlog at only about 1.1× real time: a minute of backlog takes about 10 minutes to catch up. Anything older is recorded as a gap, to fill afterwards (see below).
- The status shows "reconnecting…" and later "live · reconnected N×". Hover over it to see why the connection dropped. The transcript gets a line like "— Them: connection dropped 15:11, reconnected after 4 s —". After 2 minutes without Soniox, a notice says the live transcript is paused while recording continues.
- After Stop, a stream that's still reconnecting keeps trying for up to 2 minutes in the background.
- Each recording has a `soniox.log` with connections, drops (with reasons), gaps and the finish, with timestamps and no transcript text or key.
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

## Filling gaps

**Recordings** lists past recordings, newest first, with their length and any missing time. **View** shows a past transcript, and **Fill gaps…** transcribes only the missing parts from the WAV files with Soniox's file API (`stt-async-v5`, about $0.10 per hour of audio). The cost is shown before anything is sent. **Fill untranscribed parts automatically after Stop**, under Transcription settings, does it without asking.

- **What counts as missing:** gaps the live stream recorded (a stream that gave up, or an outage longer than the 60 s it resends), plus untranscribed stretches that contain speech: at least 20 s of speech in 60 s or more, or 10 s of speech after a track's last line. A stretch like that only counts if the other track also has almost no transcript at the time: a stall stops both streams, while speech on one track only is usually the other side leaking into the mic.
- **How:** each gap is cut per track with 3 s of overlap at each edge, sent as a 16 kHz mono WAV, and only words starting inside the gap are kept. The clip and the job are deleted on Soniox afterwards, and if deleting fails the app retries at next launch (`pending-deletes.json`). The recording's languages, terms and translation come from `recording.json`, which is written when recording starts.
- **Result:** the filled lines are merged into `transcript.jsonl` in time order, marked `"filled": true`, with speakers labelled "Them · F1", "F2"… (the file model numbers speakers separately). The live version is kept as `transcript.live.jsonl`.
- **Command line:** `meeting-transcriber --fill-gaps <recording folder>` lists what's missing and the cost. Add `--yes` to fill. The API key needs **Speech-to-text, async: Write** and **Files: Write**.

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
- **Long outages aren't transcribed live.** Beyond the last 60 s, missed audio is left for gap-filling. The WAV files are never affected.
