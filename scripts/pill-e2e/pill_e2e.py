#!/usr/bin/env python3
"""End-to-end check of the macOS recording pill (`bolo-pill`) against a real daemon.

Not run by `cargo test`. It starts a private daemon (its own HOME under /tmp, its own
config and socket, hotkeys off, no banners, no chime), lets the daemon
start `bolo-pill`, and checks through the window server and `bolo events` that:

  * the pill window is on screen at layer 1000 (above full-screen apps) and excluded
    from screen capture (sharing state 0), and a hover grows the idle handle;
  * ONE real pointer click on the pill starts a dictation, and the frontmost app is the
    same before and after (the pill never takes focus);
  * the event stream shows phase recording, live mic levels, processing, an outcome
    and idle, in that order, and the pill returns to its idle handle;
  * killing the daemon with SIGKILL makes the pill exit (no orphan);
  * `bolo-pill --snapshot` renders every state.

The daemon records from the real microphone. The test never uses your daemon, config,
history or recordings. It moves the pointer to the pill for one click and puts it back.
The click needs Accessibility permission for the terminal running this script.

Nothing leaves the machine: the throwaway HOME cannot use a local STT provider without
downloading a model, so the test keeps the groq provider with a placeholder key and sets
HTTPS_PROXY, HTTP_PROXY and ALL_PROXY to an unroutable local address (http://127.0.0.1:9)
while dropping NO_PROXY, so any upload fails locally with a connection error.

    cargo build && swiftc -O src/ui/BoloPill.swift -o target/release/bolo-pill \\
        -framework Cocoa -framework QuartzCore
    python3 scripts/pill-e2e/pill_e2e.py [--bolo target/debug/bolo] [--pill target/release/bolo-pill]
"""
import argparse
import json
import os
import queue
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

# Window-server queries and the one real click, through JXA (osascript), which needs no
# third-party Python packages.
JXA = r"""
ObjC.import('CoreGraphics');
function pillWindows() {
  const all = ObjC.deepUnwrap(ObjC.castRefToObject(
    $.CGWindowListCopyWindowInfo($.kCGWindowListOptionAll, 0)));
  return all.filter(w => w.kCGWindowOwnerName === 'bolo-pill').map(w => ({
    layer: w.kCGWindowLayer, onscreen: w.kCGWindowIsOnscreen,
    sharing: w.kCGWindowSharingState, bounds: w.kCGWindowBounds,
  }));
}
function post(type, x, y) {
  const e = $.CGEventCreateMouseEvent($(), type, {x: x, y: y}, 0);
  $.CGEventPost($.kCGHIDEventTap, e);
}
function run(argv) {
  if (argv[0] === 'windows') return JSON.stringify(pillWindows());
  if (argv[0] === 'pointer') {
    const p = $.CGEventGetLocation($.CGEventCreate($()));
    return JSON.stringify({x: p.x, y: p.y});
  }
  if (argv[0] === 'move') { post($.kCGEventMouseMoved, +argv[1], +argv[2]); return 'ok'; }
  if (argv[0] === 'click') {
    const x = +argv[1], y = +argv[2];
    post($.kCGEventMouseMoved, x, y);
    delay(0.15);
    post($.kCGEventLeftMouseDown, x, y);
    delay(0.06);
    post($.kCGEventLeftMouseUp, x, y);
    return 'ok';
  }
}
"""

failures = []


def check(ok, what):
    print(("  ok    " if ok else "  FAIL  ") + what, flush=True)
    if not ok:
        failures.append(what)
    return ok


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


class Lab:
    def __init__(self, bolo, pill):
        # A short path: unix socket paths are limited to ~100 bytes on macOS.
        self.home = tempfile.mkdtemp(prefix="bpe-", dir="/tmp")
        self.bin = os.path.join(self.home, "bin")
        os.makedirs(self.bin)
        # The daemon looks for bolo-pill next to its own executable.
        self.bolo = shutil.copy(bolo, os.path.join(self.bin, "bolo"))
        self.pill = shutil.copy(pill, os.path.join(self.bin, "bolo-pill"))
        self.jxa = os.path.join(self.home, "pill.js")
        with open(self.jxa, "w") as f:
            f.write(JXA)
        self.conf = os.path.join(self.home, ".config", "bolo")
        os.makedirs(self.conf)
        self.sock = os.path.join(self.conf, "bolo.sock")
        self.work = os.path.join(self.home, "work")  # cwd without a ./config.toml
        os.makedirs(self.work)
        self.env = dict(
            os.environ,
            HOME=self.home,
            BOLO_NO_HOTKEYS="1",
        )
        for key in ("GROQ_API_KEY", "TYPESAFE_API_KEY", "OPENROUTER_API_KEY", "NO_PROXY", "no_proxy"):
            self.env.pop(key, None)
        self.env["GROQ_API_KEY"] = "e2e-" + "placeholder"
        for proxy_var in ("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"):
            self.env[proxy_var] = "http://127.0.0.1:9"
        self.daemon = None
        self.events_proc = None
        self.events = queue.Queue()
        self.backlog = []
        self.write_config()

    def write_config(self):
        with open("config.toml") as f:
            cfg = f.read()
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        cfg = cfg.replace('provider = "faster-whisper"', 'provider = "groq"')
        cfg = cfg.replace("notifications = true", "notifications = false")
        cfg = cfg.replace("sounds = true", "sounds = false")
        cfg = re.sub(r"(\[vision\]\n[^\n]*\n)enabled = true", r"\1enabled = false", cfg)
        cfg = re.sub(r"port = \d+", f"port = {port}", cfg, count=1)
        self.config_path = os.path.join(self.conf, "config.toml")
        with open(self.config_path, "w") as f:
            f.write(cfg)

    def start_daemon(self):
        self.log = open(os.path.join(self.home, "daemon.log"), "w")
        self.daemon = subprocess.Popen(
            [self.bolo, "daemon", "--config", self.config_path],
            cwd=self.work, env=self.env, stdout=self.log, stderr=self.log,
        )
        end = time.time() + 15
        while time.time() < end:
            if os.path.exists(self.sock):
                try:
                    c = socket.socket(socket.AF_UNIX)
                    c.connect(self.sock)
                    c.close()
                    return True
                except OSError:
                    pass
            time.sleep(0.1)
        return False

    def start_events(self):
        self.events_proc = subprocess.Popen(
            [self.bolo, "events"], cwd=self.work, env=self.env,
            stdout=subprocess.PIPE, text=True,
        )

        def pump():
            for line in self.events_proc.stdout:
                try:
                    self.events.put(json.loads(line))
                except ValueError:
                    pass

        threading.Thread(target=pump, daemon=True).start()

    def next_event(self, timeout=8):
        try:
            ev = self.events.get(timeout=timeout)
        except queue.Empty:
            return None
        self.backlog.append(ev)
        return ev

    def wait_event(self, pred, timeout=8):
        """Reads events until one matches; returns it (or None)."""
        end = time.time() + timeout
        while time.time() < end:
            ev = self.next_event(max(0.05, end - time.time()))
            if ev and pred(ev):
                return ev
        return None

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

    def jxa_run(self, *args):
        out = run(["osascript", "-l", "JavaScript", self.jxa, *map(str, args)])
        return (out.stdout + out.stderr).strip()

    def windows(self):
        try:
            return json.loads(self.jxa_run("windows"))
        except ValueError:
            return []

    def pill_window(self, timeout=8):
        end = time.time() + timeout
        while time.time() < end:
            on = [w for w in self.windows() if w["onscreen"]]
            if on:
                return on[0]
            time.sleep(0.2)
        return None

    def pill_pids(self):
        out = run(["pgrep", "-x", "bolo-pill"]).stdout.split()
        mine = []
        for pid in out:
            path = run(["ps", "-o", "command=", "-p", pid]).stdout.strip()
            if path.startswith(self.pill):
                mine.append(int(pid))
        return mine

    def close(self):
        for pid in self.pill_pids():
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        for proc in (self.events_proc, self.daemon):
            if proc and proc.poll() is None:
                proc.kill()
                proc.wait()
        shutil.rmtree(self.home, ignore_errors=True)


def hid_idle_seconds():
    """Seconds since the last real keyboard or pointer input."""
    out = run(["ioreg", "-c", "IOHIDSystem", "-d", "4"]).stdout
    for line in out.splitlines():
        if "HIDIdleTime" in line:
            return int(line.split("=")[-1]) / 1e9
    return 99.0


def wait_for_quiet(seconds=2.0, timeout=90):
    """Do not fight someone who is using the Mac: wait for a quiet moment to click."""
    end = time.time() + timeout
    while time.time() < end:
        if hid_idle_seconds() >= seconds:
            return True
        time.sleep(0.25)
    print("  note  the Mac was never idle; clicking anyway (results may be noise)")
    return False


def front():
    return run(["lsappinfo", "front"]).stdout.strip()


def front_name(asn):
    return run(["lsappinfo", "info", "-only", "name", asn]).stdout.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bolo", default="target/debug/bolo")
    ap.add_argument("--pill", default="target/release/bolo-pill")
    args = ap.parse_args()
    if sys.platform != "darwin":
        sys.exit("macOS only")
    for path in (args.bolo, args.pill):
        if not os.path.exists(path):
            sys.exit(f"missing {path}; see the usage at the top of this file")

    lab = Lab(args.bolo, args.pill)
    try:
        run_checks(lab, args)
    finally:
        lab.close()
    if failures:
        print(f"\n{len(failures)} check(s) failed:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print("\nall pill checks passed")


def run_checks(lab, args):
    print("daemon and pill")
    if not check(lab.start_daemon(), "private daemon is up"):
        print(open(os.path.join(lab.home, "daemon.log")).read())
        return
    lab.start_events()
    hello = lab.next_event()
    check(
        hello and hello.get("type") == "hello" and hello.get("v") == 1
        and hello.get("phase") == "idle" and hello.get("style") == "small",
        f"events start with a hello snapshot ({hello})",
    )
    win = lab.pill_window()
    if not check(win is not None, "daemon started bolo-pill and its window is on screen"):
        print(open(os.path.join(lab.home, "daemon.log")).read())
        return
    check(win["layer"] == 1000, f"window layer is 1000 (screen-saver level), got {win['layer']}")
    check(win["sharing"] == 0, f"window is hidden from capture (sharing state {win['sharing']})")
    b = win["bounds"]
    check((b["Width"], b["Height"]) == (44, 8), f"idle handle is 44x8, got {b['Width']}x{b['Height']}")

    print("real click starts a dictation without taking focus")
    wait_for_quiet()
    before = front()
    before_name = front_name(before)
    pointer = json.loads(lab.jxa_run("pointer"))
    cx, cy = b["X"] + b["Width"] / 2, b["Y"] + b["Height"] / 2
    grown = None
    for _ in range(3):  # a posted pointer move can occasionally be coalesced away
        lab.jxa_run("move", cx + 6, cy)
        time.sleep(0.15)
        lab.jxa_run("move", cx, cy)
        time.sleep(0.4)
        grown = lab.pill_window()
        if grown and grown["bounds"]["Height"] > 8:
            break
    check(
        grown and grown["bounds"]["Height"] > 8,
        f"hovering the handle grows it ({grown and (grown['bounds']['Width'], grown['bounds']['Height'])})",
    )
    if not (grown and grown["bounds"]["Height"] > 8):
        now = json.loads(lab.jxa_run("pointer"))
        if abs(now["x"] - cx) > 3 or abs(now["y"] - cy) > 3:
            print(f"  note  the pointer is at {now}, not on the pill at ({cx}, {cy}): someone moved it")
    if not grown:
        return
    gb = grown["bounds"]
    lab.jxa_run("click", gb["X"] + gb["Width"] / 2, gb["Y"] + gb["Height"] / 2)
    started = lab.wait_event(lambda e: e.get("type") == "phase" and e.get("phase") == "recording")
    # Put the pointer back before anything else.
    lab.jxa_run("move", pointer["x"], pointer["y"])
    if not check(started is not None, "the click made the daemon start recording"):
        print("  (the click needs Accessibility permission for this terminal)")
        return
    time.sleep(0.4)
    after = front()
    check(after == before, f"frontmost app unchanged by the click ({before_name} -> {front_name(after)})")
    rec = lab.pill_window()
    check(
        rec and rec["bounds"]["Height"] == 32 and rec["bounds"]["Width"] > 90,
        f"pill shows the recording state ({rec and rec['bounds']})",
    )
    check(rec and rec["layer"] == 1000, "layer stays 1000 while recording")

    print("event stream")
    levels = 0
    mic_failed = None
    end = time.time() + 1.5
    while time.time() < end:
        ev = lab.next_event(0.3)
        if ev and ev.get("type") == "level":
            levels += 1
        if ev and ev.get("type") == "outcome" and ev.get("kind") == "mic-unavailable":
            mic_failed = ev
            break
    if mic_failed:
        print("  note  the microphone is not available to this terminal; checking that path")
        check(True, "mic failure is reported as outcome mic-unavailable")
        idle = lab.wait_event(lambda e: e.get("type") == "phase" and e.get("phase") == "idle")
        check(idle is not None, "and the daemon goes idle")
    else:
        check(levels >= 8, f"live mic levels stream while recording ({levels} in 1.5 s)")
        time.sleep(0.2)
        reply = lab.command("toggle")
        check(reply == "ok stopping", f"toggle stops the recording ({reply!r})")
        processing = lab.wait_event(lambda e: e.get("type") == "phase" and e.get("phase") == "processing")
        check(processing is not None, "phase processing follows the stop")
        proc = lab.pill_window()
        check(proc is not None, "pill still on screen while transcribing")
        outcome = lab.wait_event(lambda e: e.get("type") == "outcome", timeout=30)
        check(
            outcome is not None
            and outcome.get("kind") in ("done", "no-speech", "error", "max-length", "paste-interrupted"),
            f"an outcome is reported ({outcome})",
        )
        idle = lab.wait_event(lambda e: e.get("type") == "phase" and e.get("phase") == "idle")
        check(idle is not None, "then the daemon goes idle")
        order = [
            e["phase"] if e["type"] == "phase" else e["type"]
            for e in lab.backlog
            if e["type"] in ("phase", "outcome")
        ]
        check(
            order == ["recording", "processing", "outcome", "idle"],
            f"event order is recording, processing, outcome, idle ({order})",
        )

    # The result lingers for up to 1.8 s, then the pill is the idle handle again.
    time.sleep(2.4)
    handle = lab.pill_window()
    check(
        handle and (handle["bounds"]["Width"], handle["bounds"]["Height"]) == (44, 8),
        f"pill returns to the idle handle ({handle and handle['bounds']})",
    )
    end_front = front()
    check(
        end_front == before,
        f"frontmost app still unchanged at the end ({before_name} -> {front_name(end_front)})",
    )

    print("no orphan after the daemon dies")
    pids = lab.pill_pids()
    check(len(pids) == 1, f"exactly one bolo-pill is running ({pids})")
    lab.daemon.kill()  # SIGKILL: no chance to clean up
    lab.daemon.wait()
    end = time.time() + 5
    while time.time() < end and lab.pill_pids():
        time.sleep(0.2)
    check(not lab.pill_pids(), "pill exits when the daemon is killed")
    check(not [w for w in lab.windows() if w["onscreen"]], "and its window is gone")

    print("snapshots")
    snap = os.path.join(lab.home, "snap")
    out = run([lab.pill, "--snapshot", snap])
    pngs = sorted(os.listdir(snap)) if os.path.isdir(snap) else []
    check(out.returncode == 0 and "pill-states.png" in pngs, f"--snapshot renders the state sheet ({len(pngs)} files)")
    for state in ("idle", "recording-speech", "paused", "transcribing", "done-pasted", "error-no-speech"):
        path = os.path.join(snap, f"pill-{state}.png")
        check(os.path.exists(path) and os.path.getsize(path) > 200, f"snapshot pill-{state}.png")


if __name__ == "__main__":
    main()
