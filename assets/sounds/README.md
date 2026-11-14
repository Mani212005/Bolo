# Sound cues

`start.wav` and `capture.wav` are original works, synthesised from
scratch by [`scripts/sounds/generate.py`](../../scripts/sounds/generate.py): sine
partials, band-passed noise and exponential envelopes, written with Python's
standard library only. No samples, recordings or third-party audio are used, so
there is nothing to license. They are committed so the build needs no Python.

Regenerate with `python3 scripts/sounds/generate.py`; `--check` fails if the
committed files differ from what the script produces (the output is
deterministic). Everything is 48 kHz, 16-bit, mono.

| cue | what it is | length | peak | loudness (active RMS) |
| --- | --- | --- | --- | --- |
| `start` | rising perfect fifth, E5 then B5, marimba-like partials | 232 ms | -11.3 dBFS | -22 dBFS |
| `capture` | camera shutter: a soft tick and thock, then a lower, quieter closing, with a faint high glint | 150 ms | -8.6 dBFS | -22 dBFS |

Design rules, enforced by the script and again by the tests in `src/sound.rs`:
80-250 ms, starts and ends at exactly zero with raised-cosine ramps (no clicks),
sound within 1.5 ms of the first sample (playback adds no silence), peak at most
-6 dBFS, start and capture loudness-matched. Playback applies a fixed gain of
0.36 (0.6 squared) on top of that.

The capture cue shares no pitch material with the dictation cues, so the two
are never confused by ear.
