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
  * the style changes live, with no daemon or pill restart: `pill-style large` draws the
    300x84 panel while recording and pausing, `hidden` stops the helper, `small` brings it
    back, `pill-idle off` removes the idle handle, and the choice is saved to config.toml;
  * one real click on the Large panel's Stop button stops a dictation without taking focus;
  * one real right-click opens the pill's menu without taking focus, and choosing a style
    in it reaches the daemon;
  * killing the daemon with SIGKILL makes the pill exit (no orphan);
  * `bolo-pill --selftest` passes and `--snapshot` renders every state of every style.

The daemon records from the real microphone. Banners are off in the test config, so the
state banners (Listening, Paused, Transcribing) that stand down while the pill runs are
covered by the Rust tests, not here. The test never uses your daemon, config,
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
function pillWindows(pids) {
  const all = ObjC.deepUnwrap(ObjC.castRefToObject(
    $.CGWindowListCopyWindowInfo($.kCGWindowListOptionAll, 0)));
  const pidSet = (pids && pids.length) ? pids.map(Number) : null;
  return all.filter(w => {
    if (w.kCGWindowOwnerName !== 'bolo-pill') return false;
    if (pidSet && !pidSet.includes(w.kCGWindowOwnerPID)) return false;
    return true;
  }).map(w => ({
    layer: w.kCGWindowLayer, onscreen: w.kCGWindowIsOnscreen,
    sharing: w.kCGWindowSharingState, bounds: w.kCGWindowBounds,
    pid: w.kCGWindowOwnerPID,
  }));
}
function post(type, x, y) {
  const e = $.CGEventCreateMouseEvent($(), type, {x: x, y: y}, 0);
  $.CGEventPost($.kCGHIDEventTap, e);
}
function run(argv) {
  if (argv[0] === 'windows') return JSON.stringify(pillWindows(argv.slice(1)));
  if (argv[0] === 'pointer') {
    const p = $.CGEventGetLocation($.CGEventCreate($()));
    return JSON.stringify({x: p.x, y: p.y});
  }
  if (argv[0] === 'move') { post($.kCGEventMouseMoved, +argv[1], +argv[2]); return 'ok'; }
  if (argv[0] === 'rclick') {
    const x = +argv[1], y = +argv[2];
    post($.kCGEventMouseMoved, x, y);
    delay(0.15);
    for (const type of [$.kCGEventRightMouseDown, $.kCGEventRightMouseUp]) {
      const e = $.CGEventCreateMouseEvent($(), type, {x: x, y: y}, 1);
      $.CGEventPost($.kCGHIDEventTap, e);
      delay(0.06);
    }
    return 'ok';
  }
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

    def windows(self, pids=None):
        if pids is None:
            pids = self.pill_pids()
            if not pids:
                return []
        elif len(pids) == 0:
            return []
        try:
            return json.loads(self.jxa_run("windows", *pids))
        except ValueError:
            return []

    def pill_window(self, timeout=8, pids=None):
        end = time.time() + timeout
        while time.time() < end:
            on = [w for w in self.windows(pids=pids) if w.get("onscreen")]
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

    run_style_checks(lab, before, before_name)

    print("no orphan after the daemon dies")
    pids = lab.pill_pids()
    check(len(pids) == 1, f"exactly one bolo-pill is running ({pids})")
    lab.daemon.kill()  # SIGKILL: no chance to clean up
    lab.daemon.wait()
    end = time.time() + 5
    while time.time() < end and lab.pill_pids():
        time.sleep(0.2)
    check(not lab.pill_pids(), "pill exits when the daemon is killed")
    check(not [w for w in lab.windows(pids=pids) if w.get("onscreen")], "and its window is gone")

    print("self test")
    out = run([lab.pill, "--selftest"])
    check(out.returncode == 0 and "selftest passed" in out.stdout, "bolo-pill --selftest passes")
    if out.returncode != 0:
        print(out.stdout)

    print("snapshots")
    snap = os.path.join(lab.home, "snap")
    out = run([lab.pill, "--snapshot", snap])
    pngs = sorted(os.listdir(snap)) if os.path.isdir(snap) else []
    check(out.returncode == 0 and "pill-states.png" in pngs, f"--snapshot renders the state sheet ({len(pngs)} files)")
    for state in (
        "idle", "recording-speech", "paused", "transcribing", "done-pasted", "error-no-speech",
        "transcribing-reduced-motion", "large-recording-speech", "large-recording-long",
        "large-stop-pressed", "large-paused", "large-transcribing", "large-transcribing-reduced-motion",
        "large-done-pasted",
    ):
        path = os.path.join(snap, f"pill-{state}.png")
        check(os.path.exists(path) and os.path.getsize(path) > 200, f"snapshot pill-{state}.png")


def size_of(win):
    return (win["bounds"]["Width"], win["bounds"]["Height"]) if win else None


def wait_size(lab, size, timeout=6):
    """Waits until the pill window has exactly `size`; returns the window (or the last seen)."""
    end = time.time() + timeout
    win = None
    while time.time() < end:
        win = lab.pill_window(timeout=0.5)
        if win and size_of(win) == size:
            return win
        time.sleep(0.15)
    return win


def wait_until(pred, timeout=6):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.15)
    return False


def run_style_checks(lab, before_front, before_name):
    print("styles change live, without a restart")
    pill_pids = lab.pill_pids()
    daemon_pid = lab.daemon.pid
    lab.backlog.clear()

    reply = lab.command("pill-style huge")
    check(reply.startswith("err usage"), f"an unknown style is refused ({reply!r})")
    reply = lab.command("pill-style large")
    check(reply == "ok pill large idle on", f"pill-style large is accepted ({reply!r})")
    cfg = lab.wait_event(lambda e: e.get("type") == "config")
    check(
        cfg == {"type": "config", "style": "large", "show_idle": True},
        f"subscribers get a config event ({cfg})",
    )
    with open(lab.config_path) as f:
        saved = f.read()
    check('style = "large"' in saved and '(waveform, timer, Pause, Stop)' in saved,
          "the style is saved to config.toml and its comment kept")
    idle = lab.pill_window()
    check(size_of(idle) == (44, 8), f"Large idles as the 44x8 handle ({size_of(idle)})")

    reply = lab.command("toggle")
    check(reply == "ok recording", f"toggle starts a dictation ({reply!r})")
    rec = wait_size(lab, (300, 84))
    mic_down = lab.wait_event(lambda e: e.get("type") == "outcome", timeout=0.8)
    if mic_down and mic_down.get("kind") == "mic-unavailable":
        print("  note  the microphone is not available to this terminal; skipping the live Large checks")
    else:
        check(size_of(rec) == (300, 84), f"Large recording is a 300x84 panel ({size_of(rec)})")
        check(rec and rec["layer"] == 1000 and rec["sharing"] == 0, "the Large panel keeps layer 1000 and capture exclusion")
        reply = lab.command("pause")
        check(reply == "ok paused", f"pause is accepted ({reply!r})")
        paused = wait_size(lab, (300, 84))
        check(size_of(paused) == (300, 84), f"Large stays 300x84 while paused ({size_of(paused)})")
        reply = lab.command("pause")
        check(reply == "ok recording", f"resume is accepted ({reply!r})")
        time.sleep(0.9)  # past the 800 ms start debounce
        # One real click on the Large panel's Stop button (bottom right of the panel).
        panel = lab.pill_window()
        wait_for_quiet()
        pointer = json.loads(lab.jxa_run("pointer"))
        pb = panel["bounds"]
        lab.jxa_run("click", pb["X"] + 259, pb["Y"] + 64)
        stopped = lab.wait_event(lambda e: e.get("type") == "phase" and e.get("phase") == "processing", timeout=3)
        lab.jxa_run("move", pointer["x"], pointer["y"])
        check(stopped is not None, "a real click on Stop stops the dictation and starts transcribing")
        check(front() == before_front, f"the Stop click does not take focus ({before_name} -> {front_name(front())})")
        if stopped is None:
            lab.command("toggle")
        proc = wait_size(lab, (300, 84), timeout=1.5)
        check(proc is not None, "the pill stays on screen while transcribing")
    lab.wait_event(lambda e: e.get("type") == "phase" and e.get("phase") == "idle", timeout=30)
    time.sleep(2.2)  # the result lingers up to 1.8 s
    check(lab.pill_pids() == pill_pids, f"same bolo-pill process after the switch ({lab.pill_pids()} vs {pill_pids})")
    check(lab.daemon.poll() is None and lab.daemon.pid == daemon_pid, "same daemon process")

    reply = lab.command("pill-style hidden")
    check(reply == "ok pill hidden idle on", f"pill-style hidden is accepted ({reply!r})")
    check(wait_until(lambda: not lab.pill_pids()), "hidden stops the helper")
    check(lab.daemon.poll() is None, "and the daemon keeps running")
    reply = lab.command("pill-style small")
    check(reply == "ok pill small idle on", f"pill-style small is accepted ({reply!r})")
    check(wait_until(lambda: len(lab.pill_pids()) == 1), "small starts the helper again")
    handle = wait_size(lab, (44, 8), timeout=8)
    check(size_of(handle) == (44, 8), f"the idle handle is back ({size_of(handle)})")

    reply = lab.command("pill-idle off")
    check(reply == "ok pill small idle off", f"pill-idle off is accepted ({reply!r})")
    check(wait_until(lambda: not [w for w in lab.windows() if w.get("onscreen")], timeout=4),
          "with the idle handle off nothing is on screen while idle")
    reply = lab.command("pill-idle on")
    check(wait_until(lambda: lab.pill_window(timeout=0.5) is not None), f"the idle handle comes back ({reply!r})")

    print("right-click menu")
    win = lab.pill_window()
    if not win:
        check(False, "pill window for the right-click")
        return
    wait_for_quiet()
    pointer = json.loads(lab.jxa_run("pointer"))
    b = win["bounds"]
    lab.jxa_run("rclick", b["X"] + b["Width"] / 2, b["Y"] + b["Height"] / 2)
    time.sleep(0.5)
    wins = [w for w in lab.windows() if w.get("onscreen")]
    menus = [w for w in wins if w["bounds"]["Height"] > 60 and w["bounds"]["Width"] > 60]
    check(len(menus) >= 1, f"a right-click opens a menu window ({[size_of(w) for w in wins]})")
    check(front() == before_front, f"the menu does not take focus ({before_name} -> {front_name(front())})")
    if menus:
        m = menus[0]["bounds"]
        # Rows are about 22 pt tall under a 5 pt margin: the second row is "Large".
        lab.jxa_run("click", m["X"] + 30, m["Y"] + 5 + 22 + 11)
        picked = lab.wait_event(lambda e: e.get("type") == "config" and e.get("style") == "large", timeout=4)
        check(picked is not None, "choosing Large in the menu reaches the daemon")
    lab.jxa_run("move", pointer["x"], pointer["y"])
    check(front() == before_front, "focus still unchanged after the menu")
    lab.command("pill-style small")
    lab.wait_event(lambda e: e.get("type") == "config" and e.get("style") == "small", timeout=4)


if __name__ == "__main__":
    main()
