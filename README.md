# Bolo

Press a key, talk, and the text appears at your cursor — in any app.
Dictation for Linux with cloud (Groq) or fully-local, private transcription.

## Install

```sh
git clone <this-repo> && cd Bolo && ./install.sh
```

That's it. The installer sets up the Rust toolchain and system packages
(asks for sudo only if needed; falls back to a no-root install), builds the
binary, puts `bolo` on your PATH, registers the GNOME hotkeys, creates your
config, and starts the daemon as a login service.

**Requirements:** Linux with GNOME on Wayland (Ubuntu 22.04+ tested), a
microphone, and optionally a [Groq API key](https://console.groq.com) for the
fastest transcription. macOS is planned: the audio/transcription core is
portable, but text injection and hotkeys still need a macOS backend.

## Use

| Key | Action |
|---|---|
| `Ctrl+Space` | Start dictating / finish — the text pastes at your cursor and lands on the clipboard |
| `Alt+P` | Pause / resume; anything you copy while paused gets spliced into the transcript |
| `Alt+I` | While paused: insert the clipboard · when idle: re-type the last transcript at the cursor |
| *Enhance* button on the result notification | Rewrite the dictation into a clean LLM prompt (via Groq) |

The first dictation after the daemon starts shows a one-time GNOME
permission dialog — accept it so Bolo may type for you.

## Settings

```sh
bolo settings
```

A full-screen terminal UI (with Bo the cat 🐱): choose the speech engine and
model, toggle behavior, edit your vocabulary and enhance prompt, and run a
3-second mic test. Or edit the files directly:

- `~/.config/bolo/config.toml` — engine, models, hotkey behavior, insertion method
- `~/.config/bolo/vocabulary.txt` — words the recognizer should spell correctly (one per line, applies instantly)
- `~/.config/bolo/enhance_prompt.txt` — your own template for the Enhance rewrite (applies instantly)
- `GROQ_API_KEY` lives in `~/.env` — environment only, never in config, never committed

## Local & private transcription

Set `provider = "faster-whisper"` (or pick it in `bolo settings`) and audio
never leaves your machine. Measured on a 12-thread laptop CPU, transcribing
11s of speech: `base.en` 0.7s · `distil-small.en` 1.5s · `small.en` 1.9s
(Groq cloud: ~0.5s). Models download automatically on first use, or ahead of
time with `bolo model download <name>`.

## CLI

```
bolo daemon              run the daemon in the foreground
bolo toggle|pause        what the hotkeys call
bolo insert-last         re-type the last transcript
bolo enhance             enhance the last transcript
bolo settings            terminal settings UI
bolo status|quit         daemon control
bolo transcribe <wav>    transcribe a 16kHz mono WAV (benchmarking)
bolo model download [m]  pre-fetch a local model
```

Daemon logs: `journalctl --user -u bolo` (or `/tmp/bolo-daemon.log` when
started by hand).

## Troubleshooting

- **Nothing typed, no error** — your cursor wasn't in a text field when the
  text arrived; the transcript is always on the clipboard too (`Ctrl+V`), or
  press `Alt+I` to re-type it wherever your cursor is now.
- **Mic records silence after long idle** (some Intel sof-hda laptops):
  PulseAudio's idle-suspend wedges the mic. Fix: add
  `unload-module module-suspend-on-idle` to `~/.config/pulse/default.pa`
  and restart PulseAudio.
- **Terminals** paste with `Ctrl+Shift+V` — Bolo's synthesized `Ctrl+V`
  won't paste there; use the clipboard.
