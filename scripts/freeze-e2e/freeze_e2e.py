#!/usr/bin/env python3
"""End-to-end check that the macOS Control-press freeze never strands the pointer.

Not run by `cargo test`. While dictating, pressing Control grabs the screen and the daemon's
event tap keeps plain pointer moves from the apps until Control comes up, so a hover stays on
screen. This test starts a private daemon (its own HOME under /tmp, its own config and socket,
banners and chime off, hotkeys ON so its real event tap runs), starts a dictation through the
socket, plays Control and the pointer by posting HID events, and counts the pointer moves that
get past the daemon's tap with a listen-only tap placed after it. It checks that:

  * a Control-held circle is held back from the apps, makes a capture cut from the frame
    grabbed at the press, and moves reach the apps again once Control is up;
  * a missed Control release (moves without Control, no key-up) lets moves through at once;
  * a hold longer than 10 s lets moves through from then on;
  * a daemon that stops answering (SIGSTOP) gets its tap disabled by the system, moves reach
    the apps, and after SIGCONT the hold is forgotten;
  * a daemon killed with SIGKILL mid-hold leaves nothing held back.

It moves the real pointer and briefly holds the Control key, so run it while the Mac is idle,
and never against your own daemon: a real mouse move during a hold carries no Control flag and
ends the hold like a missed release (the test reports that instead of a pass). It needs Accessibility and Screen Recording permission for
the terminal running it. The test never uses your daemon, config or recordings, and deletes its
HOME (with the capture it made) at the end. Nothing leaves the machine: STT is pointed at an
unroutable proxy as in scripts/pill-e2e.

    cargo build --release && swiftc -O src/ui/BoloPill.swift -o target/release/bolo-pill \\
        -framework Cocoa -framework QuartzCore
    python3 scripts/freeze-e2e/freeze_e2e.py [--bolo target/release/bolo] [--pill target/release/bolo-pill]
"""
import argparse
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

# Posts HID events and watches what gets past the daemon's tap.
HELPER = r"""
import Foundation
import CoreGraphics

let src = CGEventSource(stateID: .hidSystemState)
let t0 = Date()
func move(_ x: Double, _ y: Double, _ ctrl: Bool) {
    let e = CGEvent(mouseEventSource: src, mouseType: .mouseMoved,
                    mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: .left)!
    e.flags = ctrl ? .maskControl : []
    e.post(tap: .cghidEventTap)
}
func ctrl(_ down: Bool) {
    let e = CGEvent(keyboardEventSource: src, virtualKey: 59, keyDown: down)!
    e.type = .flagsChanged
    e.flags = down ? .maskControl : []
    e.post(tap: .cghidEventTap)
}
let a = CommandLine.arguments
switch a[1] {
case "pointer":
    let p = CGEvent(source: nil)!.location
    print("\(p.x) \(p.y)")
case "ctrl":
    ctrl(a[2] == "down")
case "move":
    move(Double(a[2])!, Double(a[3])!, a.count > 4 && a[4] == "ctrl")
case "circle":
    // circle cx cy r ctrl|plain: one and a bit turns, 60 Hz
    let cx = Double(a[2])!, cy = Double(a[3])!, r = Double(a[4])!
    for i in 0...56 {
        let ang = Double(i) / 48.0 * 2 * Double.pi
        move(cx + r * cos(ang), cy + r * sin(ang), a[5] == "ctrl")
        usleep(16_000)
    }
case "wiggle":
    // wiggle cx cy seconds ctrl|plain: small moves every 100 ms
    let cx = Double(a[2])!, cy = Double(a[3])!, secs = Double(a[4])!
    let end = Date().addingTimeInterval(secs)
    var i = 0
    while Date() < end {
        move(cx + Double(i % 2) * 20, cy, a[5] == "ctrl")
        i += 1
        usleep(100_000)
    }
case "watch":
    // watch: logs "ms ctrl" for each pointer move that reaches the apps, until stdin closes
    let cb: CGEventTapCallBack = { _, type, event, _ in
        if type == .mouseMoved {
            let ms = Int(Date().timeIntervalSince(t0) * 1000)
            print("\(ms) \(event.flags.contains(.maskControl) ? 1 : 0)")
            fflush(stdout)
        }
        return Unmanaged.passUnretained(event)
    }
    guard let port = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .tailAppendEventTap,
        options: .listenOnly, eventsOfInterest: CGEventMask(1 << CGEventType.mouseMoved.rawValue),
        callback: cb, userInfo: nil) else { print("tap failed"); exit(1) }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), CFMachPortCreateRunLoopSource(nil, port, 0), .commonModes)
    CGEvent.tapEnable(tap: port, enable: true)
    print("ready"); fflush(stdout)
    FileHandle.standardInput.readabilityHandler = { h in if h.availableData.isEmpty { exit(0) } }
    CFRunLoopRun()
default:
    break
}
"""

failures = []


def check(ok, what):
    print(("  ok    " if ok else "  FAIL  ") + what, flush=True)
    if not ok:
        failures.append(what)
    return ok


class Lab:
    def __init__(self, bolo, pill):
        # A short path: unix socket paths are limited to ~100 bytes on macOS.
        self.home = tempfile.mkdtemp(prefix="bfe-", dir="/tmp")
        self.bin = os.path.join(self.home, "bin")
        os.makedirs(self.bin)
        self.bolo = shutil.copy(bolo, os.path.join(self.bin, "bolo"))
        shutil.copy(pill, os.path.join(self.bin, "bolo-pill"))
        helper_src = os.path.join(self.home, "helper.swift")
        with open(helper_src, "w") as f:
            f.write(HELPER)
        self.helper = os.path.join(self.bin, "helper")
        subprocess.run(["swiftc", "-O", helper_src, "-o", self.helper], check=True)
        self.conf = os.path.join(self.home, ".config", "bolo")
        os.makedirs(self.conf)
        self.sock = os.path.join(self.conf, "bolo.sock")
        self.work = os.path.join(self.home, "work")
        os.makedirs(self.work)
        self.env = dict(os.environ, HOME=self.home)
        for key in ("GROQ_API_KEY", "TYPESAFE_API_KEY", "OPENROUTER_API_KEY", "NO_PROXY",
                    "no_proxy", "BOLO_NO_HOTKEYS"):
            self.env.pop(key, None)
        self.env["GROQ_API_KEY"] = "e2e-" + "placeholder"
        for proxy_var in ("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy",
                          "all_proxy"):
            self.env[proxy_var] = "http://127.0.0.1:9"
        with open("config.toml") as f:
            cfg = f.read()
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        cfg = cfg.replace('provider = "faster-whisper"', 'provider = "groq"')
        cfg = cfg.replace("notifications = true", "notifications = false")
        cfg = cfg.replace("sounds = true", "sounds = false")
        cfg = re.sub(r"port = \d+", f"port = {port}", cfg, count=1)
        self.config_path = os.path.join(self.conf, "config.toml")
        with open(self.config_path, "w") as f:
            f.write(cfg)
        self.log_path = os.path.join(self.home, "daemon.log")
        self.daemon = None

    def start_daemon(self):
        self.log = open(self.log_path, "a")
        self.daemon = subprocess.Popen(
            [self.bolo, "daemon", "--config", self.config_path],
            cwd=self.work, env=self.env, stdout=self.log, stderr=self.log,
        )
        end = time.time() + 15
        while time.time() < end:
            if "CGEventTap active" in self.daemon_log():
                try:
                    self.command("")
                    return True
                except OSError:
                    pass
            time.sleep(0.1)
        return False

    def stop_daemon(self):
        if self.daemon and self.daemon.poll() is None:
            try:
                self.command("quit")
            except OSError:
                pass
            try:
                self.daemon.wait(5)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
                self.daemon.wait()
        self.log.close()

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

    def daemon_log(self):
        with open(self.log_path) as f:
            return f.read()

    def helper_run(self, *args):
        out = subprocess.run([self.helper, *map(str, args)], capture_output=True, text=True)
        return out.stdout.strip()

    def watch(self):
        """Starts the listen-only tap; returns a function that stops it and gives the moves seen."""
        p = subprocess.Popen([self.helper, "watch"], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, text=True)
        assert p.stdout.readline().strip() == "ready"
        t0 = time.time()

        def stop():
            time.sleep(0.3)
            p.stdin.close()
            lines = p.stdout.read().split()
            p.wait()
            return [(int(ms) / 1000 + t0, ctrl == "1") for ms, ctrl in zip(lines[::2], lines[1::2])]

        return stop

    def recording(self):
        return check(self.command("toggle") == "ok recording", "dictation started")

    def end_recording(self):
        time.sleep(0.9)  # past the toggle debounce
        self.command("toggle")
        end = time.time() + 20
        while time.time() < end and self.command("status") != "idle":
            time.sleep(0.2)


def held_moves(seen, start, stop):
    return [t for t, ctrl in seen if ctrl and start <= t <= stop]


def undisturbed(seen, start, stop):
    """The test posts only Control-held moves during a hold, so a plain move then is a person
    using the mouse, which (rightly) thaws the hold as a missed Control release."""
    stray = [t for t, ctrl in seen if not ctrl and start <= t <= stop]
    return check(not stray, "nobody used the mouse during the hold (else rerun while idle)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bolo", default="target/release/bolo")
    ap.add_argument("--pill", default="target/release/bolo-pill")
    args = ap.parse_args()
    lab = Lab(args.bolo, args.pill)
    x0, y0 = map(float, lab.helper_run("pointer").split())
    cx, cy = 600.0, 400.0
    try:
        check(lab.start_daemon(), "private daemon up with its event tap")

        print("Control-held circle while dictating", flush=True)
        lab.recording()
        time.sleep(1.0)
        lab.helper_run("move", cx, cy)
        stop = lab.watch()
        lab.helper_run("ctrl", "down")
        t_press = time.time()
        lab.helper_run("circle", cx, cy, 120, "ctrl")
        t_circle = time.time()
        time.sleep(0.4)
        lab.helper_run("ctrl", "up")
        time.sleep(0.2)
        t_up = time.time()
        lab.helper_run("circle", cx, cy, 60, "plain")
        seen = stop()
        undisturbed(seen, t_press, t_circle)
        check(not held_moves(seen, t_press, t_circle), "the apps saw none of the circle's moves")
        after_up = sum(1 for t, ctrl in seen if not ctrl and t >= t_up)
        check(after_up >= 50, f"moves reach the apps after Control up ({after_up} of 57 seen)")
        log = lab.daemon_log()
        check("froze the screen at the Control press" in log, "the screen was grabbed at the press")
        check("captured context image" in log and "cut from the frame frozen" in log,
              "the capture was cut from the frozen frame")
        check(log.count("froze the screen at the Control press") == 1, "one grab for one hold")

        print("missed Control release", flush=True)
        lab.helper_run("move", cx, cy)
        stop = lab.watch()
        lab.helper_run("ctrl", "down")
        lab.helper_run("circle", cx, cy, 60, "ctrl")
        t_plain = time.time()
        lab.helper_run("circle", cx, cy, 60, "plain")  # Control flag gone, no key-up seen
        seen = stop()
        lab.helper_run("ctrl", "up")
        check(sum(1 for t, ctrl in seen if not ctrl and t >= t_plain) >= 50,
              "moves without Control reach the apps at once")

        print("hold longer than 10 s", flush=True)
        stop = lab.watch()
        lab.helper_run("ctrl", "down")
        t_press = time.time()
        lab.helper_run("wiggle", cx, cy, 12, "ctrl")
        seen = stop()
        lab.helper_run("ctrl", "up")
        undisturbed(seen, t_press, t_press + 12)
        before = held_moves(seen, t_press, t_press + 9.5)
        after = held_moves(seen, t_press + 10.5, t_press + 12)
        check(not before, f"held back for the first 10 s ({len(before)} seen)")
        check(len(after) >= 10, f"moves reach the apps after 10 s ({len(after)} seen)")
        lab.end_recording()

        print("daemon stops answering mid-hold (tap disabled by the system)", flush=True)
        lab.recording()
        time.sleep(1.0)
        lab.helper_run("move", cx, cy)
        lab.helper_run("ctrl", "down")
        time.sleep(0.3)
        os.kill(lab.daemon.pid, signal.SIGSTOP)
        stop = lab.watch()
        t_stop = time.time()
        lab.helper_run("wiggle", cx, cy, 4, "ctrl")
        seen_stopped = stop()
        os.kill(lab.daemon.pid, signal.SIGCONT)
        time.sleep(0.5)
        stop = lab.watch()
        t_cont = time.time()
        lab.helper_run("wiggle", cx, cy, 1.5, "ctrl")
        seen_cont = stop()
        lab.helper_run("ctrl", "up")
        check(len(held_moves(seen_stopped, t_stop, t_stop + 5)) >= 10,
              "with the daemon stopped, moves reach the apps")
        check(len(held_moves(seen_cont, t_cont, t_cont + 2)) >= 10,
              "after it resumes, the hold is forgotten and moves still pass")
        lab.end_recording()

        print("daemon killed mid-hold", flush=True)
        lab.recording()
        time.sleep(1.0)
        lab.helper_run("move", cx, cy)
        lab.helper_run("ctrl", "down")
        lab.helper_run("circle", cx, cy, 60, "ctrl")
        os.kill(lab.daemon.pid, signal.SIGKILL)
        lab.daemon.wait()
        time.sleep(0.3)
        stop = lab.watch()
        t_kill = time.time()
        lab.helper_run("wiggle", cx, cy, 1.5, "ctrl")
        seen = stop()
        lab.helper_run("ctrl", "up")
        check(len(held_moves(seen, t_kill, t_kill + 2)) >= 10,
              "with the daemon dead, Control-held moves reach the apps")
    finally:
        lab.helper_run("ctrl", "up")
        lab.helper_run("move", x0, y0)
        lab.stop_daemon()
        if failures:
            print(f"daemon log kept at {lab.log_path}")
        else:
            shutil.rmtree(lab.home, ignore_errors=True)
    print(f"\n{'FAILED: ' + str(len(failures)) if failures else 'all checks passed'}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
