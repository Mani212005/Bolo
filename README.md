<p align="center">
  <img src="assets/Bolo.png" alt="Bolo Logo" width="120" />
</p>

# Bolo: Open-Source Voice Dictation for macOS & Linux

Fast, private voice dictation and speech-to-text for macOS and Linux. An open-source alternative to Wispr Flow and Superwhisper with on-device local Whisper and ultra-fast Groq Whisper STT.

Press a global hotkey, speak naturally, and high-accuracy transcribed text appears directly at your cursor in any application.



https://github.com/user-attachments/assets/4111acdf-83bc-4493-a096-9a30876a1bbb



---

## Bolo vs. Wispr Flow vs. Superwhisper

| Feature | Bolo | Wispr Flow | Superwhisper |
|---|---|---|---|
| **License & Price** | **100% Free & Open Source (MIT)** | Paid subscription ($12+/mo) | Paid subscription ($8+/mo) |
| **Privacy & Local STT** | **Yes** (whisper.cpp & faster-whisper) | No (Cloud only) | Yes (Local models) |
| **Ultra-Fast Cloud STT** | **Yes** (Groq Whisper in ~250ms) | Yes (Proprietary cloud) | Yes (Cloud credits) |
| **Linux Support** | **Yes** (Wayland & X11) | No (macOS & Windows only) | No (macOS only) |
| **macOS Support** | **Yes** (Native Cocoa & Apple Silicon) | Yes | Yes |
| **Mid-Speech Clipboard Splice** | **Yes** (`Option+V` without stopping) | No | No |
| **Audio Playback & History** | **Yes** (Built-in playback engine) | Limited | Limited |
| **AI Prompt Enhancement** | **Yes** (Groq LLaMA-3.3-70B) | Yes | Yes (Paid tier) |
| **Custom Vocabulary Biasing** | **Yes** (Plain text & UI chips) | Yes | Yes |

---

## Features

- **Global Push-to-Talk Hotkey**: Press `Ctrl + Space` anywhere to start and stop speech-to-text dictation at your cursor.
- **Mid-Dictation Clipboard Splicing**: Press `Option + V` mid-speech to insert links, code, or copied text without pausing audio.
- **On-Device Privacy & Local Models**: Run speech recognition completely offline with whisper.cpp and faster-whisper.
- **Ultra-Fast Cloud Transcription**: Transcribe long voice notes in ~250ms using Groq Whisper-Large-v3.
- **Native Popup & History Dashboard**: Search past voice dictations and listen back with the built-in audio playback engine.
- **One-Click AI Prompt Enhancement**: Refine raw speech and rambles into structured prompts using LLaMA-3.3-70B.
- **Custom Vocabulary Biasing**: Add technical terms, proper nouns, and acronyms for accurate phonetic transcription.
- **Audio File Drag and Drop**: Drop any audio file directly into the dashboard for immediate speech-to-text transcription.

---

## Global Shortcuts

| Shortcut (macOS) | Shortcut (Linux) | Action |
|---|---|---|
| `Ctrl + Space` | `Ctrl + Space` | **Start / Stop Dictation**: Pastes text at cursor and copies to clipboard |
| `Option + V` (`⌥V`) | `Alt + V` | **Quick-Splice Clipboard**: Injects clipboard text mid-speech without stopping audio |
| `Option + P` (`⌥P`) | `Alt + P` | **Pause / Resume**: Temporarily freezes ongoing voice recording |
| `Option + I` (`⌥I`) | `Alt + I` | **Re-Type Last**: Types the most recent transcription at cursor position |
| `Option + C` (`⌥C`) | `Alt + C` | **Copy Selection & Splice**: Copies selected text from frontmost app and splices |

---

## Quickstart & Installation

One command installs dependencies, compiles native binaries, and sets up the background daemon:

```bash
git clone https://github.com/Mani212005/Bolo.git
cd Bolo
./install.sh
```

### 1. Launch

```bash
bolo
```

*Running `bolo` verifies the background daemon and opens the native popup dashboard. Run `bolo exit` to shut down.*

### 2. macOS Permissions (One-Time Setup)

Grant the following permissions in **System Settings > Privacy & Security**:
- **Microphone**: For voice dictation audio capture.
- **Accessibility**: To paste transcribed text directly at your cursor (`Cmd+V`).
- **Input Monitoring**: For global push-to-talk hotkeys (`Ctrl+Space`).

### 3. Linux Integration

On Linux, `install.sh` configures your systemd user service, GNOME hotkeys, and XDG Desktop Portals for Wayland and X11. Access the web dashboard at `http://127.0.0.1:4525` or via `bolo`.

---

## CLI Commands

```bash
bolo                     # Start daemon & open native popup dashboard
bolo exit                # Cleanly terminate daemon and popup window
bolo daemon              # Run background engine in foreground for logs
bolo toggle              # Toggle voice dictation start / stop
bolo quick-splice        # Splice clipboard into ongoing recording
bolo pause               # Pause or resume ongoing voice recording
bolo insert-last         # Re-type the most recent transcript at cursor
bolo enhance             # Enhance the last transcript with AI
bolo history             # View transcription history in terminal
bolo transcribe <file>   # Transcribe a local audio WAV file
```

---

## Configuration

Bolo configuration files live in `~/.config/bolo/`:

- **`~/.config/bolo/config.toml`**: Speech engine, models, hotkeys, and VAD parameters.
  ```toml
  [stt]
  provider = "faster-whisper" # "faster-whisper", "whisper", or "groq"

  [stt.whisper]
  model = "small.en"          # "tiny.en", "base.en", "small.en", "large-v3-turbo"

  [groq]
  model = "whisper-large-v3"
  language = "en"
  temperature = 0.0

  [vad]
  auto_endpoint = false       # false = manual push-to-talk toggle
  max_utterance_ms = 1800000  # 30-minute maximum recording cap
  ```

- **`~/.config/bolo/vocabulary.txt`**: Custom word prompts (names, brand terms, acronyms).
- **`~/.config/bolo/enhance_prompt.txt`**: Prompt template for AI enhancement.
- **`~/.env`**: Optional `GROQ_API_KEY=gsk_...` for cloud transcription and LLaMA enhancement.

---

## Architecture

```
+-------------------------------------------------------------+
|                    macOS / Linux Desktop                    |
|   (Any App: VS Code, Slack, Browser, Terminal, Notes)       |
+------------------------------+------------------------------+
                               |
            +------------------+------------------+
            |                                     |
            v                                     v
   [ Native Hotkey Engine ]              [ Native Popup UI ]
    * macOS: CGEventTap / Carbon          * Swift Cocoa + WebKit (Mac)
    * Linux: Desktop Portal Keybind       * Web Dashboard (Linux)
            |                                     |
            +------------------+------------------+
                               v
                    [ Bolo Core Daemon ]
                 * Audio Stream (16kHz cpal)
                 * Silero VAD (Speech Detection)
                 * Mid-Speech Splicing Engine
                 * WAV Capture Storage
                               |
            +------------------+------------------+
            |                                     |
            v                                     v
   [ Local Private STT ]                 [ Cloud Fast STT ]
    * faster-whisper (CTranslate2)        * Groq Whisper-Large-v3
    * whisper.cpp (Metal/CPU)             * ~250ms ultra-low latency
            |                                     |
            +------------------+------------------+
                               v
                   [ Injector & Clipboard ]
                 * Native macOS Quartz Keystrokes
                 * Linux Wayland Portal / X11
```

---

## License

[MIT License](file:///Users/manijoshi/firstmate/projects/Bolo/LICENSE). Free and open source.
