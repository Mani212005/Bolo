#!/usr/bin/env python3
"""End-to-end check of spoken-list formatting against a real, private daemon.

Not run by `cargo test`. It starts a daemon with its own HOME under /tmp (own config,
socket, history; hotkeys off, no banners, no sounds, no pill) and drives whole
dictations through its socket exactly as the hotkey does: `toggle` starts recording
from the real microphone, `toggle` stops it, and the daemon transcribes, formats
(asking Jev at most once) and pastes.

Only the speech engine and the system clipboard are stand-ins, so the test needs no
speaking and never touches your apps or clipboard:

  * STT: the faster-whisper provider runs `$HOME/.local/share/bolo/fw-venv/bin/python`;
    in the private HOME that is a stub that answers each request with the next
    synthetic transcript. `speech_threshold = 0` makes the quiet room count as speech,
    so every recording reaches it.
  * Paste: `pbcopy` and the Cmd+V `osascript` are PATH stubs that record what would
    have been pasted (all pieces, when a terminal in front gets several pastes); other `osascript` calls (frontmost app) run for real.
    `restore_clipboard = false`, so the real pasteboard is never written.

Every transcript is synthetic. With TYPESAFE_API_KEY set, list candidates go to the
live Jev API; without it (or with --no-jev) the run checks the Jev-off fallback.

    cargo build && python3 scripts/list-e2e/list_e2e.py [--bolo target/debug/bolo]
                                                        [--no-jev] [--timeout-ms N]
                                                        [--report out.md]
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

CASES = [
    {
        "name": "counted list with a lead-in and a closing remark",
        "spoken": "Okay, a couple of things here. Firstly, the voiceover is not working. "
        "Secondly, I cannot drag clips on the timeline. Thirdly, undo does not work. "
        "Fix these and tell me when done.",
        "want": "Okay, a couple of things here.\n\n1. The voiceover is not working\n"
        "2. I cannot drag clips on the timeline\n3. Undo does not work\n\n"
        "Fix these and tell me when done.",
    },
    {
        "name": "one is, the second one is, in one sentence",
        "spoken": "There are three things. One is B2B, the second one is B2C and third one is both.",
        "want": "There are three things.\n\n1. B2B\n2. B2C\n3. Both",
    },
    {
        "name": "uncounted to-do list",
        "spoken": "Things to do today. Call the bank. Then book the flights. Also reply to Sam. "
        "And finally pick up the kids.",
        "want": "Things to do today.\n\n- Call the bank\n- Book the flights\n- Reply to Sam\n"
        "- Pick up the kids",
        "alts": [
            "Things to do today.\n\n1. Call the bank\n2. Book the flights\n3. Reply to Sam\n"
            "4. Pick up the kids"
        ],
    },
    {
        "name": "the issues are, one, two, three",
        "spoken": "The issues are, one, the login fails, two, the page is slow, three, the logo is blurry.",
        "want": "The issues are:\n\n1. The login fails\n2. The page is slow\n3. The logo is blurry",
    },
    {
        "name": "narrated steps stay prose",
        "spoken": "First I opened the app, then I clicked the settings, and finally the page crashed.",
        "want": None,
    },
    {
        "name": "pointing at objects stays prose",
        "spoken": "Use the first reference image for the colors. The second reference image is "
        "for the layout. Ignore the third image.",
        "want": None,
    },
    {
        "name": "stray leading dash from whisper",
        "spoken": "- Okay, I'm testing Bolo with a quick note.",
        "want": "Okay, I'm testing Bolo with a quick note.",
    },
]

STT_STUB = r"""#!/usr/bin/env python3
# Stand-in faster-whisper sidecar: answers each request with the next transcript.
import json, os, sys
queue = os.path.join(os.environ["HOME"], "stt-next.txt")
for line in sys.stdin:
    text = open(queue).read() if os.path.exists(queue) else ""
    print(json.dumps({"text": text, "latency_ms": 1}), flush=True)
"""

# A terminal in front gets the dictation as several pastes; together they are the text.
PBCOPY_STUB = """#!/bin/sh
cat >> "$HOME/pasted.txt"
"""

PBPASTE_STUB = """#!/bin/sh
exit 0
"""

OSASCRIPT_STUB = """#!/bin/sh
case "$*" in
  *keystroke*|*"key code"*) echo pasted >> "$HOME/paste-chords.txt"; exit 0 ;;
esac
exec /usr/bin/osascript "$@"
"""


def write_exec(path, body):
    with open(path, "w") as f:
        f.write(body)
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


class Daemon:
    def __init__(self, bolo, use_jev, timeout_ms, config_src):
        self.home = tempfile.mkdtemp(prefix="ble-", dir="/tmp")  # short: unix socket path limit
        self.conf = os.path.join(self.home, ".config", "bolo")
        os.makedirs(self.conf)
        self.sock = os.path.join(self.conf, "bolo.sock")
        work = os.path.join(self.home, "work")
        os.makedirs(work)
        bin_dir = os.path.join(self.home, "bin")
        os.makedirs(bin_dir)
        self.bolo = shutil.copy(bolo, os.path.join(bin_dir, "bolo"))
        venv_bin = os.path.join(self.home, ".local", "share", "bolo", "fw-venv", "bin")
        os.makedirs(venv_bin)
        write_exec(os.path.join(venv_bin, "python"), STT_STUB)
        write_exec(os.path.join(bin_dir, "pbcopy"), PBCOPY_STUB)
        write_exec(os.path.join(bin_dir, "pbpaste"), PBPASTE_STUB)
        write_exec(os.path.join(bin_dir, "osascript"), OSASCRIPT_STUB)

        self.env = dict(os.environ, HOME=self.home, BOLO_NO_HOTKEYS="1")
        self.env["PATH"] = bin_dir + ":" + self.env["PATH"]
        for key in ("GROQ_API_KEY", "OPENROUTER_API_KEY"):
            self.env.pop(key, None)
        if not use_jev:
            self.env.pop("TYPESAFE_API_KEY", None)

        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        with open(config_src) as f:
            cfg = f.read()
        cfg = re.sub(r'(?m)^provider = "[\w-]+"', 'provider = "faster-whisper"', cfg, count=1)
        cfg = cfg.replace("notifications = true", "notifications = false")
        cfg = cfg.replace("sounds = true", "sounds = false")
        cfg = re.sub(r"(?m)^speech_threshold = [0-9.]+", "speech_threshold = 0.0", cfg)
        cfg = re.sub(r'(?m)^style = "\w+"', 'style = "hidden"', cfg)
        cfg = re.sub(r"(?m)^show_when_idle = \w+", "show_when_idle = false", cfg)
        cfg = re.sub(r"(?m)^port = \d+", f"port = {port}", cfg, count=1)
        cfg = re.sub(r"(?m)^timeout_ms = \d+", f"timeout_ms = {timeout_ms}", cfg)
        cfg = re.sub(r"(?m)^type_delay_ms = (\d+)", r"type_delay_ms = \1\nrestore_clipboard = false", cfg)
        config_path = os.path.join(self.conf, "config.toml")
        with open(config_path, "w") as f:
            f.write(cfg)
        self.log_path = os.path.join(self.home, "daemon.log")
        self.log = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [self.bolo, "daemon", "--config", config_path],
            cwd=work, env=self.env, stdout=self.log, stderr=self.log,
        )

    def wait_ready(self):
        end = time.time() + 30
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

    def lines(self):
        with open(self.log_path) as f:
            return f.read().splitlines()

    def wait_for(self, pattern, mark, timeout=15):
        end = time.time() + timeout
        while time.time() < end:
            for line in self.lines()[mark:]:
                if re.search(pattern, line):
                    return line
            time.sleep(0.05)
        return None

    def dictate(self, spoken):
        """One dictation through the socket; returns (pasted text, jev log line)."""
        with open(os.path.join(self.home, "stt-next.txt"), "w") as f:
            f.write(spoken)
        pasted = os.path.join(self.home, "pasted.txt")
        if os.path.exists(pasted):
            os.remove(pasted)
        mark = len(self.lines())
        self.command("toggle")
        if not self.wait_for(r"toggle→capture_ms", mark):
            return None, "recording never started"
        time.sleep(1.5)
        self.command("toggle")
        if not self.wait_for(r"^\[inject\]", mark, timeout=30):
            return None, "\n".join(self.lines()[mark:][-15:])
        jev = self.wait_for(r"^\[jev\]", mark, timeout=1) or ""
        with open(pasted) as f:
            return f.read(), jev

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


def same_text(a, b):
    return a.split() == b.split()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bolo", default="target/debug/bolo")
    ap.add_argument("--config", default="config.toml", help="config to start from")
    ap.add_argument("--no-jev", action="store_true", help="run without a Jev key")
    ap.add_argument("--timeout-ms", type=int, default=2500, help="Jev timeout")
    ap.add_argument("--report", help="write each case's spoken and pasted text here")
    args = ap.parse_args()
    use_jev = not args.no_jev and bool(os.environ.get("TYPESAFE_API_KEY"))
    print(f"Jev: {'live' if use_jev else 'off'}, timeout {args.timeout_ms}ms")

    d = Daemon(args.bolo, use_jev, args.timeout_ms, args.config)
    failures = 0
    report = []
    try:
        if not d.wait_ready():
            print("daemon did not start:\n" + "\n".join(d.lines()[-20:]))
            return 1
        for case in CASES:
            pasted, jev = d.dictate(case["spoken"])
            if pasted is None:
                print(f"FAIL {case['name']}: no paste\n{jev}")
                failures += 1
                continue
            # Without Jev answers, only local formatting applies: every list stays prose.
            jev_answered = use_jev and "answers=" in jev
            want = case["want"] if (jev_answered or case["want"] is None or "- Okay" in case["spoken"]) else None
            if want is None:
                expected = case["spoken"] if not case["spoken"].startswith("- ") else case["spoken"][2:]
                ok = same_text(pasted, expected)
            else:
                ok = pasted == want or pasted in case.get("alts", [])
            failures += not ok
            latency = re.search(r"latency=(\d+ms)", jev)
            print(f"{'ok  ' if ok else 'FAIL'} {case['name']}"
                  f" (jev {'calls=0' if 'calls=0' in jev else latency.group(1) if latency else jev[:80]})")
            if not ok:
                print(f"  pasted:\n{pasted}\n  want:\n{want or case['spoken']}")
            report.append((case["name"], case["spoken"], pasted, jev))
    finally:
        d.quit()

    if args.report:
        with open(args.report, "w") as f:
            for name, spoken, pasted, jev in report:
                f.write(f"### {name}\n\nTranscript:\n\n```text\n{spoken}\n```\n\n"
                        f"Pasted:\n\n```text\n{pasted}\n```\n\n")
    print(f"\n{len(CASES) - failures}/{len(CASES)} cases ok")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
