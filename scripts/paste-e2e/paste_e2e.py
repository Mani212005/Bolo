#!/usr/bin/env python3
"""Manual dev tool: check that Bolo's split pastes arrive verbatim in terminal agents.

Not run by `cargo test`. It starts Claude Code and/or Antigravity (agy) in a
private tmux server (`tmux -L bolo-paste-e2e`) inside a scratch directory, pastes
Bolo's pieces the way a terminal does on Cmd+V (`tmux paste-buffer -p`), then reads
the exact prompt buffer back through Ctrl+G ($EDITOR is a dump script).

It never submits a prompt (Enter is sent only to accept the folder-trust prompt)
and never touches any other tmux server, window, or the macOS GUI/clipboard.

    cargo build && python3 scripts/paste-e2e/paste_e2e.py [--agent claude|agy|all]
                                                          [--bolo target/debug/bolo]
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

SOCKET = "bolo-paste-e2e"
TMUX = ["tmux", "-L", SOCKET, "-f", "/dev/null"]
# 1x1 PNG, so agents have a real image to attach.
PNG = (
    b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00"
    b"\x00\x1f\x15\xc4\x89\x00\x00\x00\nIDATx\x9cc\x00\x01\x00\x00\x05\x00\x01\r\n-\xb4"
    b"\x00\x00\x00\x00IEND\xaeB`\x82"
)
PLACEHOLDER = "[Pasted text"


def tmux(*args):
    return subprocess.run(TMUX + list(args), capture_output=True, text=True)


class Lab:
    """A scratch directory, the dump 'editor', and one agent session."""

    def __init__(self, bolo):
        self.bolo = os.path.abspath(bolo)
        self.dir = tempfile.mkdtemp(prefix="bolo-paste-e2e-")
        self.work = os.path.join(self.dir, "lab")
        os.mkdir(self.work)
        self.dump_out = os.path.join(self.dir, "dump.out")
        self.dump_sh = os.path.join(self.dir, "dump.sh")
        with open(self.dump_sh, "w") as f:
            # Record the prompt buffer verbatim, then empty it so the next case starts clean.
            f.write(f'#!/bin/bash\ncp "$1" "{self.dump_out}"\n: > "$1"\n')
        os.chmod(self.dump_sh, 0o755)
        self.images = []
        for n in (1, 2):
            path = os.path.join(self.work, f"context-{n}.png")
            with open(path, "wb") as f:
                f.write(PNG)
            self.images.append(path)

    def close(self):
        tmux("kill-server")
        shutil.rmtree(self.dir, ignore_errors=True)

    # -- tmux helpers -------------------------------------------------------
    def screen(self, target):
        return tmux("capture-pane", "-p", "-J", "-t", target).stdout

    def keys(self, target, *keys):
        tmux("send-keys", "-t", target, *keys)

    def paste(self, target, text):
        """One terminal-style paste: bracketed if the app enabled it, LF becomes CR."""
        path = os.path.join(self.dir, "paste.buf")
        with open(path, "wb") as f:
            f.write(text.encode())
        tmux("load-buffer", "-b", "pb", path)
        tmux("paste-buffer", "-p", "-b", "pb", "-t", target)

    def wait_for(self, target, pattern, timeout=40):
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pattern, self.screen(target), re.I):
                return True
            time.sleep(0.3)
        return False

    def dump(self, target, timeout=8):
        """Ctrl+G opens $EDITOR (dump.sh): the exact prompt buffer, which is then emptied."""
        if os.path.exists(self.dump_out):
            os.unlink(self.dump_out)
        self.keys(target, "C-g")
        end = time.time() + timeout
        while time.time() < end and not os.path.exists(self.dump_out):
            time.sleep(0.1)
        time.sleep(0.8)
        if not os.path.exists(self.dump_out):
            return None
        with open(self.dump_out, encoding="utf-8") as f:
            return f.read()

    # -- agents -------------------------------------------------------------
    def start(self, agent):
        agent_bin = shutil.which(agent) or agent
        path_val = os.environ.get("PATH", "")
        cmd = f'env EDITOR="{self.dump_sh}" VISUAL="{self.dump_sh}" PATH="{path_val}" {agent_bin}'
        tmux("new-session", "-d", "-s", agent, "-x", "200", "-y", "50", "-c", self.work, cmd)
        target = agent
        if not self.wait_for(target, r"trust|❯|ask anything"):
            raise RuntimeError(f"{agent} did not start:\n{self.screen(target)}")
        if re.search(r"trust", self.screen(target), re.I):
            # Folder-trust prompt, once per scratch directory. The only Enter ever sent.
            if agent == "claude":
                self.keys(target, "Down")
                time.sleep(0.3)
            self.keys(target, "Enter")
            time.sleep(2)
        ready = r"❯" if agent == "claude" else r"ask|type|prompt|>"
        if not self.wait_for(target, ready, timeout=30):
            raise RuntimeError(f"{agent} prompt never appeared:\n{self.screen(target)}")
        time.sleep(1.5)
        return target

    def pieces(self, text):
        out = subprocess.run(
            [self.bolo, "split-preview"], input=text, capture_output=True, text=True, check=True
        ).stdout
        pieces = json.loads(out)
        assert "".join(pieces) == text, "chunker must rejoin to the exact text"
        return pieces


# Fixtures ------------------------------------------------------------------
SAMPLE = (
    "Here is what I need. First, the importer must keep accents like café and naïve, "
    "emoji like 🙂, quotes \"double\" and 'single', $HOME, and `backticks`. "
) * 6
LONG = "\n\n".join(
    [SAMPLE.strip()] * 3 + ["- first item\n- second item\n- third item", "That is everything for today."]
)
PARAGRAPHS = "\n\n".join(
    [
        "First paragraph: the login page fails after the redirect.",
        "Second paragraph: the token is still stored but the header is empty.",
        "Third paragraph: please find the cause and propose a fix.",
    ]
)


def run_agent(lab, agent):
    failures = []

    def check(name, ok, detail=""):
        print(f"  [{'ok' if ok else 'FAIL'}] {agent}: {name}")
        if not ok:
            failures.append(name)
            if detail:
                print("        " + detail.replace("\n", "\n        "))

    target = lab.start(agent)

    # Control: one paste of the same text must collapse, which proves the harness sees failure.
    lab.paste(target, PARAGRAPHS + "\n\n" + LONG)
    time.sleep(1)
    check("control: a single big paste collapses", PLACEHOLDER in lab.screen(target))
    lab.dump(target)

    for name, text in (("long dictation", LONG), ("three paragraphs", PARAGRAPHS)):
        pieces = lab.pieces(text)
        for piece in pieces:
            lab.paste(target, piece)
            time.sleep(0.05)
        time.sleep(1)
        shown = lab.screen(target)
        buf = lab.dump(target)
        check(f"{name}: {len(pieces)} pieces, no placeholder", PLACEHOLDER not in shown, shown)
        check(f"{name}: buffer is byte-exact", buf == text, f"got {buf!r}")

    # Text plus two screenshots: pieces of "text ", then one paste per quoted path.
    text = "Explain what is wrong in these two screenshots."
    pastes = lab.pieces(text + " ")
    pastes.append(f'"{lab.images[0]}"')
    pastes.append(f' "{lab.images[1]}"')
    for piece in pastes:
        lab.paste(target, piece)
        time.sleep(0.2)
    time.sleep(1.5)
    buf = lab.dump(target) or ""
    today = f'{text} "{lab.images[0]}" "{lab.images[1]}"'
    if agent == "claude":
        ok = re.fullmatch(re.escape(text) + r" \[Image #\d+\] \[Image #\d+\]", buf) is not None
        check("screenshots: two real attached images", ok, f"got {buf!r}")
    else:
        check("screenshots: quoted paths, same as before", buf == today, f"got {buf!r}")
    return failures


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--agent", choices=["claude", "agy", "all"], default="all")
    ap.add_argument("--bolo", default="target/debug/bolo", help="path to a built bolo binary")
    args = ap.parse_args()
    agents = ["claude", "agy"] if args.agent == "all" else [args.agent]

    failures = []
    for agent in agents:
        if shutil.which(agent) is None:
            print(f"skip {agent}: not installed")
            continue
        lab = Lab(args.bolo)
        try:
            print(f"{agent}:")
            failures += run_agent(lab, agent)
        finally:
            lab.close()
    print("FAILED: " + ", ".join(failures) if failures else "all checks passed")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
