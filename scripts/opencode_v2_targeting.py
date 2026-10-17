#!/usr/bin/env python3
"""Two real TUIs; isolated profile; no credentials or production plugins.

Run through heavy-job.py --wait --. Use --bin CANDIDATE or --source CHECKOUT.
The fixture observes actual UI route/location and registers a no-audio speech.pause.
It never implements selection or command routing itself.
"""
import argparse
import base64
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import time
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    candidate = parser.add_mutually_exclusive_group(required=True)
    candidate.add_argument("--source", type=Path)
    candidate.add_argument("--bin", type=Path)
    parser.add_argument("--timeout", type=int, default=95)
    args = parser.parse_args()
    assert 10 <= args.timeout <= 110
    cli = args.source.resolve() / "packages/cli" if args.source else None
    if cli:
        assert (cli / "src/index.ts").is_file()
        bun = shutil.which("bun")
        assert bun, "bun is required"
        command = [bun, "run", "src/index.ts"]
    else:
        command = [str(args.bin.resolve())]
    children, masters, outputs = [], [], {}
    started = time.monotonic()
    deadline = started + args.timeout
    with tempfile.TemporaryDirectory(prefix="opencode-v2-two-tui-") as tmp:
        root = Path(tmp)
        env = {k: v for k, v in os.environ.items() if k in (
            "PATH", "TMPDIR", "LANG", "LC_ALL", "SHELL", "USER")}
        for key, folder in {"HOME": "home", "XDG_CONFIG_HOME": "config",
                            "XDG_DATA_HOME": "data", "XDG_CACHE_HOME": "cache",
                            "XDG_STATE_HOME": "state", "OPENCODE_CONFIG_DIR": "config/opencode"}.items():
            directory = root / folder
            directory.mkdir(parents=True, exist_ok=True)
            env[key] = str(directory)
        env.update(OPENCODE_DB=str(root / "opencode.db"),
                   OPENCODE_DISABLE_MODELS_FETCH="1", OPENCODE_DISABLE_AUTOUPDATE="1",
                   OPENCODE_DISABLE_PROJECT_CONFIG="1", OPENCODE_DISABLE_CLAUDE_CODE="1",
                   OPENCODE_DISABLE_FFF="1", OPENCODE_DISABLE_FILEWATCHER="1",
                   OPENCODE_SERVER_PASSWORD="isolated-targeting-proof",
                   TERM="xterm-256color", COLORTERM="truecolor", NO_COLOR="1")
        plugin = root / "config/opencode/plugins/targeting-proof"
        plugin.mkdir(parents=True)
        (plugin / "tui.ts").write_text('''import { appendFileSync } from "node:fs";
export default { id: "isolated-targeting-proof", setup(ctx) {
  const file = process.env.TARGETING_PROOF_LOG;
  const emit = (kind) => { const route = ctx.ui.router.current();
    const session = route.type === "session" ? ctx.data.session.get(route.sessionID) : undefined;
    appendFileSync(file, JSON.stringify({kind, route, location: ctx.location,
      title: session?.title}) + "\\n"); };
  ctx.keymap.layer(() => ({mode: "global", commands: [{id: "speech.pause",
    title: "Fixture pause (no audio)", run: () => emit("pause")}]}));
  let previous;
  const timer = setInterval(() => { const value = JSON.stringify([ctx.ui.router.current(), ctx.location,
    ctx.ui.router.current().type === "session" ? ctx.data.session.get(ctx.ui.router.current().sessionID)?.title : null]);
    if (value !== previous) { previous = value; emit("state"); } }, 50);
  emit("ready"); return () => clearInterval(timer);
}};
''')
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        auth = base64.b64encode(b"opencode:isolated-targeting-proof").decode()

        def api(path, body=None):
            req = urllib.request.Request(url + path,
                data=None if body is None else json.dumps(body).encode(),
                headers={"Authorization": "Basic " + auth, "Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=2) as response:
                return json.load(response)

        def spawn(argv, name, extra=None, terminal=False):
            child_env = env | (extra or {})
            if terminal:
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 150, 0, 0))
                try:
                    child = subprocess.Popen([*command, *argv], cwd=cli or root,
                        env=child_env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
                finally:
                    os.close(slave)
                masters.append((name, master))
                outputs[name] = bytearray()
            else:
                log = (root / (name + ".log")).open("wb")
                try:
                    child = subprocess.Popen([*command, *argv], cwd=cli or root,
                        env=child_env, stdout=log, stderr=log, start_new_session=True)
                finally:
                    log.close()
            children.append((name, child))

        def pump():
            for name, fd in masters:
                while select.select([fd], [], [], 0)[0]:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    outputs[name].extend(chunk)
                    # Minimal headless terminal replies for OpenTUI startup negotiation.
                    for query, reply in [(b"\x1b[6n", b"\x1b[1;1R"),
                                         (b"\x1b[c", b"\x1b[?1;2c"),
                                         (b"\x1b[>c", b"\x1b[>0;0;0c")]:
                        if query in chunk:
                            os.write(fd, reply)

        def wait(predicate, label):
            last = None
            while time.monotonic() < deadline:
                pump()
                for name, child in children:
                    assert child.poll() is None, f"{name} exited {child.returncode}"
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, ValueError) as error:
                    last = str(error)
                time.sleep(.05)
            raise AssertionError(f"timeout waiting for {label}: {last}")

        def records(name):
            path = root / (name + ".jsonl")
            return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

        def state(name):
            rows = records(name)
            return rows[-1] if rows else {}

        def matches(name, session, directory, title):
            row = state(name)
            return (row.get("route", {}).get("sessionID") == session and
                    row.get("location", {}).get("directory") == str(directory) and
                    row.get("title") == title)

        def terminal_titles(name):
            return re.findall(rb"\x1b\](?:0|2);([^\x07\x1b]*)(?:\x07|\x1b\\)", outputs[name])

        try:
            spawn(["serve", "--hostname", "127.0.0.1", "--port", str(port)], "server")
            info = wait(lambda: api("/api/info"), "server readiness")
            assert info["capabilities"]["tuiSessionSelect"] is True
            assert info["capabilities"]["tuiExecuteCommand"] is True
            sessions = []
            for index in range(3):
                directory = root / f"project-{index}"
                directory.mkdir()
                title = f"Targeting proof {index}"
                created = api("/api/session", {"location": {"directory": str(directory)}, "title": title})
                sessions.append((created["data"]["id"], directory, title))
            for name, (session, directory, _) in zip(("a", "b"), sessions):
                spawn(["--server", url, "--session", session, str(directory)], name,
                      {"OPENCODE_TUI_CLIENT_ID": "proof-" + name,
                       "TARGETING_PROOF_LOG": str(root / (name + ".jsonl"))}, terminal=True)
            wait(lambda: matches("a", *sessions[0]) and matches("b", *sessions[1]), "both live UI initial states")
            for target, other, destination in [("a", "b", sessions[2]), ("b", "a", sessions[0])]:
                before = state(other)
                before_rows = len(records(other))
                other_titles = terminal_titles(other)
                session, directory, title = destination
                assert api("/api/tui/select-session", {"client": "proof-" + target,
                    "sessionID": session, "targetDirectory": str(directory)}) is True
                wait(lambda: matches(target, session, directory, title), f"{target} cross-directory switch")
                # Observe the non-target for a full second after target convergence.
                until = time.monotonic() + 1
                wait(lambda: time.monotonic() >= until, "non-target observation window")
                assert state(other) == before and len(records(other)) == before_rows
                pump()
                assert terminal_titles(other) == other_titles, "non-target terminal title changed"
                titles = terminal_titles(target)
                assert any(title.encode() in item for item in titles), f"no real terminal title for {target}: {titles}"
                print(json.dumps({"proof": "exact-client-select", "target": target,
                    "session": session, "directory": str(directory), "title": title,
                    "other_unchanged": True}), flush=True)
            for target, other in [("a", "b"), ("b", "a")]:
                before = sum(row["kind"] == "pause" for row in records(target))
                other_before = sum(row["kind"] == "pause" for row in records(other))
                assert api("/api/tui/execute-command", {"command": "speech.pause", "client": "proof-" + target}) is True
                wait(lambda: sum(row["kind"] == "pause" for row in records(target)) == before + 1,
                     f"{target} fixture pause dispatch")
                until = time.monotonic() + 1
                wait(lambda: time.monotonic() >= until, "non-target command observation window")
                assert sum(row["kind"] == "pause" for row in records(other)) == other_before
                print(json.dumps({"proof": "fixture-speech.pause", "target": target,
                                  "other_unchanged": True, "audio_tested": False}), flush=True)
            print(f"PASS two actual PTYs; elapsed={time.monotonic() - started:.2f}s", flush=True)
        except BaseException:
            for name, data in outputs.items():
                print(f"DIAGNOSTIC {name} PTY tail: {bytes(data[-5000:])!r}", flush=True)
                print(f"DIAGNOSTIC {name} states: {records(name)!r}", flush=True)
            path = root / "server.log"
            if path.exists():
                print("DIAGNOSTIC server: " + path.read_text(errors="replace")[-5000:], flush=True)
            raise
        finally:
            cleanup_failures = []
            for name, child in reversed(children):
                for sig in (signal.SIGTERM, signal.SIGKILL):
                    try:
                        os.killpg(child.pid, sig)
                    except ProcessLookupError:
                        pass
                    try:
                        child.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        pass
                    try:
                        os.killpg(child.pid, 0)
                    except ProcessLookupError:
                        break
                else:
                    cleanup_failures.append(f"{name} {child.pid}")
                    continue
                print(f"CLEANUP verified absent process group {name} {child.pid}", flush=True)
            for _, fd in masters:
                os.close(fd)
            assert not cleanup_failures, f"process groups survived cleanup: {cleanup_failures}"


if __name__ == "__main__":
    main()
