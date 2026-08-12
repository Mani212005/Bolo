"""Bolo faster-whisper sidecar: JSON lines over stdin/stdout.

Request:  {"wav": "/path/to/16k-mono.wav", "initial_prompt": "..." | null}
Reply:    {"text": "...", "latency_ms": 123}  or  {"error": "..."}
The model loads once at startup; "ready" is printed on stderr when done.
"""
import json
import os
import sys
import time

from faster_whisper import WhisperModel

model_name = sys.argv[1]
raw_threads = int(sys.argv[2])
# macOS Apple Silicon performs best with 4 compute threads to avoid thread contention across E/P cores
threads = min(4, max(1, raw_threads))
model = WhisperModel(model_name, device="cpu", compute_type="int8", cpu_threads=threads)
print("ready", file=sys.stderr, flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        t0 = time.monotonic()
        segments, _info = model.transcribe(
            req["wav"],
            language="en",
            beam_size=1,
            initial_prompt=req.get("initial_prompt"),
            vad_filter=False,
        )
        text = "".join(s.text for s in segments).strip()
        reply = {"text": text, "latency_ms": int((time.monotonic() - t0) * 1000)}
    except Exception as e:  # noqa: BLE001 - report everything to the daemon
        reply = {"error": str(e)}
    print(json.dumps(reply), flush=True)
