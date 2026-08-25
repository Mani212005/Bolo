# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Build & test: `cargo test` runs all unit tests including vision geometry and session context retention.
- Architecture: `src/vision/` implements pointer-guided screen context capture (`CircleGestureDetector`, `capture_screen`, `write_context_bundle`, `prune_sessions`).
- Config: `[vision]` in `config.toml` manages `enabled` (default `true`) and `min_angle_degrees` (default `340.0`).
- Session context: Completed dictation sessions write `context.md` + `context-N.png` into `~/.local/share/bolo/sessions/<session_id>/`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
