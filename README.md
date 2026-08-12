# Bolo 🎙️

**Private, lightning-fast voice-to-text dictation and clipboard splicing for macOS & Linux.**

Press a key, speak naturally, and high-accuracy transcribed text appears directly at your cursor in any application. Supports fully local, private models (whisper.cpp / faster-whisper) and blazing fast cloud transcription (Groq Whisper-Large in ~0.3s).

---

## ✨ Features

- ⚡ **Global Push-to-Talk / Toggle**: Press `Ctrl + Space` anywhere to start speaking. Press again to finish — transcribed text instantly pastes at your cursor and copies to your clipboard.
- ✂️ **Mid-Dictation Clipboard Splicing (`Option + V` / `Alt + V`)**: Copy text, links, or code mid-speech; press `Option + V` and keep talking without pausing — Bolo seamlessly stitches your speech, clipboard text, and subsequent voice input into a single chronological message.
- 🔊 **Native History & Audio Playback Dashboard (`bolo`)**: A native macOS Cocoa popup (and Linux dashboard) displaying all your past transcriptions with timestamps, duration, search filtering, and **"Hear Voice"** audio playback of your original voice captures.
- 🔒 **100% Local & Private Option**: Run speech recognition completely on-device with zero internet connection using local Whisper models.
- 🪄 **One-Click AI Enhancement**: Transform messy dictation, speech-to-text rambles, or meeting notes into structured LLM prompts or professional summaries via LLaMA-3.3-70B.
- 🎯 **Domain Vocabulary & Custom Prompts**: Add custom terms, tech jargon, and acronyms in `~/.config/bolo/vocabulary.txt` for perfect phonetic spelling.
- 🎧 **Audio File Drag & Drop**: Drop any audio or voice memo file directly into the Bolo dashboard for instant transcription.

---

## ⌨️ Global Shortcuts

| Shortcut (macOS) | Shortcut (Linux) | Action |
|---|---|---|
| `Ctrl + Space` | `Ctrl + Space` | **Start / Stop Dictation** — Pastes text at cursor and copies to clipboard |
| `Option + V` (`⌥V`) | `Alt + V` | **Quick-Splice Clipboard** — Injects clipboard text mid-speech without stopping audio |
| `Option + P` (`⌥P`) | `Alt + P` | **Pause / Resume** — Holds recording session |
| `Option + I` (`⌥I`) | `Alt + I` | **Re-Type Last** — Re-types the last dictation at your current cursor position |
| — | Notification Button | **Enhance** — Rewrites the dictation with AI and puts it on your clipboard |

---

## 🚀 Installation & Quickstart (macOS & Linux)

One command installs everything, compiles the native binaries, and sets up your environment:

```bash
git clone https://github.com/Mani212005/Bolo.git
cd Bolo
./install.sh
```

### 1. Launch
```bash
bolo
```
*Typing `bolo` ensures the background daemon is running and opens the native popup dashboard. Type `bolo exit` anytime to cleanly shut down.*

### 2. macOS Permissions (One-Time Setup)
Grant the following permissions in **System Settings > Privacy & Security**:
- **Microphone**: For voice capture.
- **Accessibility**: To paste transcribed text directly at your cursor (`Cmd+V`).
- **Input Monitoring**: For global background push-to-talk hotkeys (`Ctrl+Space`).

### 3. Linux Integration
On Linux, `install.sh` automatically configures your desktop service, GNOME hotkeys, and XDG Desktop Portals for Wayland / X11. You can access the dashboard at `http://127.0.0.1:4525` or via `bolo`.

---

## 🛠️ CLI Commands

```bash
bolo                     # Start daemon (if not running) & open native popup dashboard
bolo exit                # Cleanly terminate daemon and popup window
bolo daemon              # Run the background engine in foreground (for logs/debugging)
bolo toggle              # Toggle dictation recording start / stop
bolo quick-splice        # Splice clipboard into ongoing recording
bolo pause               # Pause or resume ongoing recording
bolo insert-last         # Re-type the most recent transcript at cursor
bolo enhance             # Enhance the last transcript with AI
bolo history             # View history directly in terminal
bolo transcribe <file>   # Transcribe a local WAV file
```

---

## ⚙️ Configuration

Bolo stores configuration files in standard `~/.config/bolo/`:

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
- **`~/.config/bolo/enhance_prompt.txt`**: Prompt template for the AI Enhance feature.
- **`~/.env`**: Optional `GROQ_API_KEY=gsk_...` for cloud transcription and LLaMA enhancement.

---

## 🧩 Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      macOS / Linux Desktop                  │
│   (Any App: VS Code, Slack, Browser, Terminal, Notes)       │
└──────────────────────────────┬──────────────────────────────┘
                               │
            ┌──────────────────┴──────────────────┐
            ▼                                     ▼
   [ Native Hotkey Engine ]              [ Native Popup UI ]
    • macOS: CGEventTap / Carbon          • Swift Cocoa + WebKit (Mac)
    • Linux: Desktop Portal Keybind       • Web Dashboard (Linux)
            │                                     │
            └──────────────────┬──────────────────┘
                               ▼
                    [ Bolo Core Daemon ]
                 • Audio Stream (16kHz cpal)
                 • Silero VAD (Speech Detection)
                 • Mid-Speech Splicing Engine
                 • Wave Capture Storage
                               │
            ┌──────────────────┴──────────────────┐
            ▼                                     ▼
   [ Local Private STT ]                 [ Cloud Fast STT ]
    • faster-whisper (CTranslate2)        • Groq Whisper-Large-v3
    • whisper.cpp (Metal/CPU)             • ~300ms ultra-low latency
            │                                     │
            └──────────────────┬──────────────────┘
                               ▼
                  [ Injector & Clipboard ]
                • Native macOS Quartz Keystrokes
                • Linux Wayland Portal / X11
```

---

## 📜 License

MIT License. Designed with speed, privacy, and ergonomics in mind.
