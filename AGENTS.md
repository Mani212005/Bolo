# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Build & test: `cargo test` runs all unit tests including vision geometry and session context retention. Three macOS tests in `src/inject/macos.rs` are `#[ignore]`d because they send a real Cmd+V to the frontmost app and overwrite the clipboard; run them only with `cargo test -- --ignored` and a scratch text field in front.
- Architecture: `src/vision/` implements hover-only pointer-guided screen context capture (`CircleGestureDetector`, `capture_screen`, `write_context_bundle`, `prune_sessions`).
- Config: `[vision]` in `config.toml` manages `enabled` (default `true`) and `min_angle_degrees` (default `315.0`); `[formatting.jev]` manages real-time semantic predictive formatting; `[pill]` manages the macOS recording pill (`style`, `show_when_idle`).
- Formatting: `src/format.rs` formats each dictation locally (code detection, fences vs raw per app, paragraphs, spoken list cues) and batches anything it is unsure about into at most one Jev request (`src/jev.rs`, TypeSafe `jev-latest` by default, OpenRouter optional). On timeout or error the local result is used. `bolo eval-format [--jev]` measures accuracy on `src/format_eval_cases.json`.
- Inject: on macOS `src/inject/macos.rs` plans pastes (`plan_pastes`) and, for a terminal frontmost app or `[inject.terminal] extra_apps`, sends the dictation as several pastes under the agents' collapse limits (`src/inject/split.rs`); `scripts/paste-e2e/paste_e2e.py` checks that against the installed agents in a private tmux server.
- Recording pill: the daemon publishes phase, mic level and outcome on its socket (`subscribe`, `bolo events`, `src/events.rs`); on macOS `src/pill.rs` supervises `bolo-pill` (`src/ui/BoloPill.swift`, built by `install.sh` next to `bolo`), a non-activating panel that must never take focus (never call `NSApp.activate`; `isFloatingPanel` before `level`). `bolo-pill --snapshot <dir>` renders every state to PNG; `scripts/pill-e2e/pill_e2e.py` runs a private daemon (HOME under /tmp, `BOLO_NO_HOTKEYS=1`) and does one real pointer click, so run it while the Mac is idle and never against your own daemon.
- Session context: Completed dictation sessions write `context.md` + `context-N.png` into `~/.local/share/bolo/sessions/<session_id>/`, requiring Control modifier for circle capture, queuing screenshots for sequential paste or CLI quoted path fallback (macOS).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
