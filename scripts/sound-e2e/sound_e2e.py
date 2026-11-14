#!/usr/bin/env python3
"""End-to-end check of Bolo's sound cues against a real, private daemon.

Not run by `cargo test`. It starts a daemon with its own HOME under /tmp (own config,
socket and sessions, hotkeys off, no banners) and drives it through its socket and
dashboard API exactly as the hotkey, `bolo capture` and the settings page do:

  * `toggle` (Control+Space) plays the start cue, within MAX_MS of the request;
  * `capture` takes a screenshot and plays the capture cue the moment the frame is
    grabbed (within MAX_MS of `trigger->frame_ms`), before the image is processed;
  * a failed capture (`screencapture` replaced by a stub that exits 1) plays nothing;
  * `toggle` again plays the stop cue;
  * `sounds = false` and `sound_volume` apply live, without a restart: nothing plays
    when off, the level in the log follows the volume, and a bad volume is rejected.

"Within MAX_MS" is the daemon's own measure: request until the first sample leaves the
output device (queueing + the stream's buffer). The daemon records from the real
microphone, so run it somewhere quiet; the recording is discarded, and nothing leaves
the machine (the groq provider has a placeholder key and every proxy variable points at
an unroutable local address).

    cargo build && python3 scripts/sound-e2e/sound_e2e.py [--bolo target/debug/bolo]
"""
import argparse
import json
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import urllib.request

MAX_MS = 30.0
CAPTURE_MAX_MS = 80.0
failures = 0


def check(ok, what):
    global failures
    print(("PASS  " if ok else "FAIL  ") + what)
    if not ok:
        failures += 1


class Daemon:
    def __init__(self, bolo, failing_capture=False):
        self.home = tempfile.mkdtemp(prefix="bse-", dir="/tmp")  # short: unix socket path limit
        self.conf = os.path.join(self.home, ".config", "bolo")
        os.makedirs(self.conf)
        self.sock = os.path.join(self.conf, "bolo.sock")
        self.work = os.path.join(self.home, "work")
        os.makedirs(self.work)
        self.bin = os.path.join(self.home, "bin")
        os.makedirs(self.bin)
        self.bolo = shutil.copy(bolo, os.path.join(self.bin, "bolo"))
        # `screencapture` stub for the failure case; only on PATH while it is wanted.
        self.stub = os.path.join(self.home, "stub")
        os.makedirs(self.stub)
        stub = os.path.join(self.stub, "screencapture")
        with open(stub, "w") as f:
            f.write("#!/bin/sh\nexit 1\n")
        os.chmod(stub, os.stat(stub).st_mode | stat.S_IEXEC)
        self.env = dict(os.environ, HOME=self.home, BOLO_NO_HOTKEYS="1")
        if failing_capture:
            # Force the `screencapture` path (the in-process grab would succeed), and make it fail.
            self.env["PATH"] = self.stub + ":" + self.env["PATH"]
            self.env["BOLO_CAPTURE_FALLBACK"] = "1"
        for key in ("GROQ_API_KEY", "TYPESAFE_API_KEY", "OPENROUTER_API_KEY", "NO_PROXY", "no_proxy"):
            self.env.pop(key, None)
        self.env["GROQ_API_KEY"] = "e2e-" + "placeholder"
        for var in ("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"):
            self.env[var] = "http://127.0.0.1:9"
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        self.port = s.getsockname()[1]
        s.close()
        with open("config.toml") as f:
            cfg = f.read()
        cfg = cfg.replace('provider = "faster-whisper"', 'provider = "groq"')
        cfg = cfg.replace("notifications = true", "notifications = false")
        cfg = re.sub(r"port = \d+", f"port = {self.port}", cfg, count=1)
        self.config_path = os.path.join(self.conf, "config.toml")
        with open(self.config_path, "w") as f:
            f.write(cfg)
        self.log_path = os.path.join(self.home, "daemon.log")
        self.log = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [self.bolo, "daemon", "--config", self.config_path],
            cwd=self.work, env=self.env, stdout=self.log, stderr=self.log,
        )

    def wait_ready(self):
        end = time.time() + 15
        while time.time() < end:
            try:
                c = socket.socket(socket.AF_UNIX)
                c.connect(self.sock)
                c.close()
                return True
            except OSError:
                time.sleep(0.1)
        return False

    def command(self, cmd):
        c = socket.socket(socket.AF_UNIX)
        c.settimeout(5)
        c.connect(self.sock)
        c.sendall((cmd + "\n").encode())
        reply = b""
        try:
            while not reply.endswith(b"\n"):
                chunk = c.recv(256)
                if not chunk:
                    break
                reply += chunk
        except OSError:
            pass
        c.close()
        return reply.decode().strip()

    def post(self, path, body):
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}{path}", json.dumps(body).encode(),
            {"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, timeout=5) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode()

    def lines(self):
        with open(self.log_path) as f:
            return f.read().splitlines()

    def since(self, mark):
        return self.lines()[mark:]

    def wait_for(self, pattern, mark, timeout=6):
        end = time.time() + timeout
        while time.time() < end:
            for line in self.since(mark):
                if re.search(pattern, line):
                    return line
            time.sleep(0.05)
        return None

    def quit(self):
        try:
            self.command("quit")
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.log.close()
        if os.environ.get("KEEP_E2E_HOME") is None:
            shutil.rmtree(self.home, ignore_errors=True)


def cue_ms(line):
    return float(re.search(r"= ([0-9.]+) ms from trigger", line).group(1))


def wait_idle(d, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        if d.command("status") == "idle":
            return True
        time.sleep(0.1)
    return False


def scenario_normal(bolo):
    d = Daemon(bolo)
    try:
        check(d.wait_ready(), "private daemon is up")
        check(d.wait_for(r"\[sound\] output", 0, 5) is not None, "the output stream opened at startup")

        # 1. Control+Space: start cue.
        mark = len(d.lines())
        check(d.command("toggle") == "ok recording", "toggle starts a recording")
        line = d.wait_for(r"\[sound\] start cue", mark)
        check(line is not None, "start cue played")
        if line:
            print("      " + line)
            check(cue_ms(line) < MAX_MS, f"start cue reached the speaker in under {MAX_MS:.0f} ms")
        time.sleep(1.0)  # past the 800 ms double-tap debounce

        # 2. Screenshot: cue the moment the frame is grabbed, before processing.
        mark = len(d.lines())
        check(d.command("capture") == "ok capturing", "capture starts")
        line = d.wait_for(r"\[sound\] capture cue", mark)
        check(line is not None, "capture cue played")
        if line:
            print("      " + line)
            check(cue_ms(line) < CAPTURE_MAX_MS, f"capture cue within {CAPTURE_MAX_MS:.0f} ms of the request (the screencapture fallback alone takes 110-170 ms)")
        check(d.wait_for(r"captured context image", mark) is not None, "the screenshot was saved")
        new = d.since(mark)
        frame = next((l for l in new if "trigger->frame_ms=" in l), None)
        if frame:
            print("      " + frame)
        check(frame is not None, "the cue is tied to the frame being grabbed (trigger->frame_ms logged)")
        # The cue fires when `screencapture` returns, before the PNG is decoded and the crop
        # encoded, so its latency is close to the frame time, not frame time + processing.
        if line and frame:
            frame_ms = float(re.search(r"frame_ms=(\d+)", frame).group(1))
            check(cue_ms(line) - frame_ms < MAX_MS, "cue reached the speaker within 30 ms of the frame being grabbed")

        # 3. Stop cue.
        mark = len(d.lines())
        check(d.command("toggle") == "ok stopping", "toggle stops the recording")
        line = d.wait_for(r"\[sound\] stop cue", mark)
        check(line is not None, "stop cue played")
        if line:
            print("      " + line)
            check(cue_ms(line) < MAX_MS + 30, f"stop cue reached the speaker in under {MAX_MS + 30:.0f} ms")
        check(wait_idle(d), "the session ended")

        # 4. Settings apply live.
        status, reply = d.post("/api/config", {"sounds": False})
        check(status == 200 and reply.get("needs_restart") is False, "sounds off saves without a restart")
        mark = len(d.lines())
        d.command("toggle")
        time.sleep(1.2)
        d.command("toggle")
        wait_idle(d)
        check(not any("[sound]" in l and "cue" in l for l in d.since(mark)), "no cue plays while sounds are off")

        status, reply = d.post("/api/config", {"sounds": True, "sound_volume": 0.3})
        check(status == 200, "sounds back on with volume 0.3")
        mark = len(d.lines())
        d.command("toggle")
        line = d.wait_for(r"\[sound\] start cue", mark)
        check(line is not None and re.search(r"at -21 dB", line) is not None,
              "the next cue follows the new volume (0.3 squared = -21 dB)")
        if line:
            print("      " + line)
        time.sleep(1.0)
        d.command("toggle")
        wait_idle(d)

        before = open(d.config_path).read()
        status, _ = d.post("/api/config", {"sound_volume": 7})
        check(status == 500 and open(d.config_path).read() == before, "a volume outside 0..1 is rejected and saves nothing")
    finally:
        d.quit()


def scenario_failed_capture(bolo):
    d = Daemon(bolo, failing_capture=True)
    try:
        check(d.wait_ready(), "daemon with a failing screencapture is up")
        d.command("toggle")
        time.sleep(1.0)
        mark = len(d.lines())
        check(d.command("capture") == "ok capturing", "capture starts")
        check(d.wait_for(r"screen capture failed", mark) is not None, "the capture failed")
        time.sleep(0.5)
        check(not any("[sound] capture cue" in l for l in d.since(mark)), "a failed capture plays no cue")
        d.command("toggle")
        wait_idle(d)
    finally:
        d.quit()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bolo", default="target/debug/bolo")
    args = ap.parse_args()
    bolo = os.path.abspath(args.bolo)
    scenario_normal(bolo)
    scenario_failed_capture(bolo)
    print(f"\n{failures} failure(s)" if failures else "\nall checks passed")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
