# Changelog

All notable changes to Bolo will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-08-13

### Added
- Universal cross-platform `install.sh` for macOS and Linux.
- Native macOS Cocoa popup interface built with Swift and WebKit.
- Web Audio API playback engine for voice recording verification.
- Groq LLaMA-3.3-70B AI prompt enhancement pipeline with clipboard sync.
- Interactive Scratchpad with auto-save and one-click AI prompt enhancement.
- Dynamic port discovery via `~/.local/share/bolo/port.txt`.
- Custom vocabulary biasing manager for proper nouns and technical terms.
- Mid-dictation clipboard splicing hotkeys (`Option+V`, `Option+C`).

### Changed
- Refactored UI buttons to clean icon-only layout for copy, playback, and delete actions.
- Replaced glassmorphic dropdowns with flat, minimalist dark theme selects.
- Made Scratchpad full-width with responsive line height and padding.
- Removed unnecessary emoji decor across UI and documentation.

### Fixed
- Fixed search bar text overlap by refining input padding specificity.
- Fixed WebKit audio playback stall on macOS Cocoa webview.
- Fixed Groq API key resolution to support `~/.env` fallback.

## [0.1.0] - 2026-08-01

### Added
- Initial release of Bolo: fast, private voice dictation daemon in Rust.
- Global push-to-talk hotkey (`Ctrl+Space`) with kernel-level autorepeat filtering.
- Local Whisper STT and Groq Whisper Cloud STT integration.
- Direct active-window text injection for Wayland, X11, and macOS.
- Web dashboard for transcription history, audio clips, and settings.
