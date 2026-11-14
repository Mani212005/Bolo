#!/usr/bin/env python3
"""Synthesises Bolo's two sound cues into assets/sounds/*.wav.

Pure standard library (no numpy, no samples, no third-party audio), so the
cues are original work and the output is bit-for-bit reproducible:

    python3 scripts/sounds/generate.py            # rewrite the WAVs
    python3 scripts/sounds/generate.py --check    # fail if the committed WAVs differ

Every cue is 48 kHz, 16-bit, mono. Design rules, all enforced by `check()`
below and again by the unit tests in src/sound.rs:

  * 80-250 ms long, first sample exactly 0 and sound within ~1 ms of it, so
    the playback path adds no silence of its own
  * every partial starts with a raised-cosine ramp and the tail fades with a
    raised cosine to exact zero, so there are no clicks or pops
  * peak at most -6 dBFS before the fixed playback gain is applied
  * loudness matched: start and capture share one RMS target

Voices:
  start    soft rising perfect fifth (E5 -> B5), marimba-like partials
  capture  camera shutter: a soft "tick + thock" for the opening, a lower,
           quieter one for the closing, with a faint high glint on top; it
           shares no pitch material with the dictation cues so the two
           can never be confused by ear
"""
import math
import random
import struct
import sys
import wave
from pathlib import Path

SR = 48_000
OUT_DIR = Path(__file__).resolve().parents[2] / "assets" / "sounds"

PEAK_LIMIT = 0.5          # -6 dBFS
RMS_TARGET_DB = -22.0     # RMS of the active part of start and capture
ACTIVE_FLOOR = 0.02       # samples under 2% of peak do not count as "active"
MIN_MS, MAX_MS = 80, 250


def n(ms):
    return int(round(ms * SR / 1000))


def ramp_in(i, samples):
    """Raised-cosine fade-in; 1.0 once `i` passes `samples`."""
    if i >= samples:
        return 1.0
    return 0.5 - 0.5 * math.cos(math.pi * i / samples)


def tone(freq, length_ms, tau_ms, partials, attack_ms=2.0, glide_to=None, glide_ms=0.0):
    """Exponentially decaying sum of harmonics; optional pitch glide."""
    out = []
    phase = [0.0] * len(partials)
    for i in range(n(length_ms)):
        t = i / SR
        f = freq
        if glide_to is not None and glide_ms > 0:
            g = min(1.0, t * 1000 / glide_ms)
            f = freq + (glide_to - freq) * (1 - (1 - g) ** 2)
        env = math.exp(-t * 1000 / tau_ms) * ramp_in(i, n(attack_ms))
        s = 0.0
        for k, (mult, amp) in enumerate(partials):
            phase[k] += 2 * math.pi * f * mult / SR
            s += amp * math.sin(phase[k])
        out.append(s * env)
    return out


def bandpass(samples, centre, q):
    """RBJ constant-0-dB-peak band-pass biquad."""
    w0 = 2 * math.pi * centre / SR
    alpha = math.sin(w0) / (2 * q)
    b0, b1, b2 = alpha, 0.0, -alpha
    a0, a1, a2 = 1 + alpha, -2 * math.cos(w0), 1 - alpha
    x1 = x2 = y1 = y2 = 0.0
    out = []
    for x in samples:
        y = (b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2) / a0
        x2, x1, y2, y1 = x1, x, y1, y
        out.append(y)
    return out


def noise_tick(length_ms, tau_ms, centre, q, rng, attack_ms=0.6):
    raw = [rng.uniform(-1, 1) for _ in range(n(length_ms))]
    shaped = bandpass(raw, centre, q)
    return [
        s * math.exp(-(i / SR) * 1000 / tau_ms) * ramp_in(i, n(attack_ms))
        for i, s in enumerate(shaped)
    ]


def mix_into(dst, src, at_ms, gain=1.0):
    start = n(at_ms)
    if start + len(src) > len(dst):
        dst.extend([0.0] * (start + len(src) - len(dst)))
    for i, s in enumerate(src):
        dst[start + i] += s * gain


def finish(samples, total_ms, fade_ms):
    """Trim/pad to the length and fade the tail to exact zero."""
    samples = samples[: n(total_ms)] + [0.0] * max(0, n(total_ms) - len(samples))
    fade = n(fade_ms)
    for i in range(fade):
        samples[len(samples) - 1 - i] *= 0.5 - 0.5 * math.cos(math.pi * i / fade)
    samples[0] = 0.0
    samples[-1] = 0.0
    return samples


# Marimba-like: strong fundamental, a little 2nd and 3rd harmonic. Warm, not shrill.
WARM = [(1, 1.0), (2, 0.28), (3, 0.08)]
E5, B5 = 659.255, 987.767


def cue_start():
    out = []
    mix_into(out, tone(E5, 150, 55, WARM), 0, 0.85)
    mix_into(out, tone(B5, 190, 70, WARM), 68, 1.0)
    return finish(out, 232, 45)


def cue_capture():
    rng = random.Random(20260514)
    out = []
    # opening: tick (band-passed noise) + thock (short falling sine) + glint
    mix_into(out, noise_tick(30, 6.0, 3200, 0.9, rng), 0, 1.0)
    mix_into(out, tone(460, 60, 16, [(1, 1.0), (2, 0.2)], attack_ms=1.2,
                       glide_to=190, glide_ms=34), 0, 1.15)
    mix_into(out, tone(2637, 80, 22, [(1, 1.0)], attack_ms=1.0), 0, 0.10)
    # closing: lower and softer
    mix_into(out, noise_tick(30, 5.0, 2300, 0.9, rng), 52, 0.55)
    mix_into(out, tone(330, 60, 14, [(1, 1.0), (2, 0.2)], attack_ms=1.2,
                       glide_to=140, glide_ms=30), 52, 0.80)
    return finish(out, 150, 35)


CUES = {"start": cue_start, "capture": cue_capture}


def active_rms(samples):
    peak = max(abs(s) for s in samples)
    active = [s for s in samples if abs(s) >= ACTIVE_FLOOR * peak]
    return math.sqrt(sum(s * s for s in active) / len(active))


def db(x):
    return 20 * math.log10(x)


def render(name):
    samples = CUES[name]()
    gain = 10 ** (RMS_TARGET_DB / 20) / active_rms(samples)
    samples = [s * gain for s in samples]
    return [max(-32768, min(32767, int(round(s * 32767)))) for s in samples]


def to_wav_bytes(ints):
    import io

    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(struct.pack(f"<{len(ints)}h", *ints))
    return buf.getvalue()


def check(name, ints):
    floats = [v / 32768 for v in ints]
    peak = max(abs(v) for v in floats)
    ms = len(ints) * 1000 / SR
    problems = []
    if not MIN_MS <= ms <= MAX_MS:
        problems.append(f"length {ms:.0f} ms outside {MIN_MS}-{MAX_MS}")
    if peak > PEAK_LIMIT:
        problems.append(f"peak {db(peak):.1f} dBFS over {db(PEAK_LIMIT):.0f}")
    if ints[0] != 0 or ints[-1] != 0:
        problems.append("does not start and end at zero")
    # largest sample-to-sample step: a click shows up as a step far above the signal's own slope
    step = max(abs(floats[i + 1] - floats[i]) for i in range(len(floats) - 1))
    if step > 0.20:
        problems.append(f"max sample step {step:.3f} (click risk)")
    first_audible = next(i for i, v in enumerate(floats) if abs(v) > 0.01 * peak)
    if first_audible > n(1.5):
        problems.append(f"first audible sample at {first_audible * 1000 / SR:.2f} ms")
    return peak, step, first_audible, problems


def main():
    check_only = "--check" in sys.argv
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    failed = False
    for name in CUES:
        ints = render(name)
        peak, step, first, problems = check(name, ints)
        rms = db(active_rms([v / 32768 for v in ints]))
        print(f"{name:8s} {len(ints) * 1000 / SR:5.0f} ms  peak {db(peak):6.1f} dBFS  "
              f"active RMS {rms:6.1f} dBFS  max step {step:.3f}  first audible {first * 1000 / SR:.2f} ms")
        for p in problems:
            print(f"  PROBLEM: {p}")
            failed = True
        data = to_wav_bytes(ints)
        path = OUT_DIR / f"{name}.wav"
        if check_only:
            if not path.exists() or path.read_bytes() != data:
                print(f"  DIFFERS from committed {path}")
                failed = True
        else:
            path.write_bytes(data)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
