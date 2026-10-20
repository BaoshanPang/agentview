#!/usr/bin/env python3
"""Token-free, real-binary restart/reboot acceptance harness for a disposable VM.

Copy this script and the installed agentview/opencode binaries to ~/reboot-recovery.
Run prepare, restart, reboot the VM, then recover, replay and cleanup.
Reboot/recover/replay may be repeated. Never use a real profile.
"""
import base64
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import signal
import sqlite3
import struct
import subprocess
import sys
import threading
import termios
import time
import urllib.request
import urllib.error

ROOT = Path.home() / "reboot-recovery"
assert "VirtualMac" in subprocess.check_output(["sysctl", "-n", "hw.model"]).decode(), "Run only inside a disposable macOS VM"
PROFILE = ROOT / "profile"
RECORD = PROFILE / "state/agentview/opencode/server.json"
ENV = dict(os.environ, **{
    "XDG_CONFIG_HOME": str(PROFILE / "config"),
    "XDG_DATA_HOME": str(PROFILE / "data"),
    "XDG_CACHE_HOME": str(PROFILE / "cache"),
    "XDG_STATE_HOME": str(PROFILE / "state"),
    "OPENCODE_CONFIG_DIR": str(PROFILE / "config/opencode"),
    "OPENCODE_DB": str(PROFILE / "opencode.db"),
    "AGENTVIEW_OPENCODE_API": "v2",
    "AGENTVIEW_OPENCODE_SERVICE_FILE": str(PROFILE / "state/opencode/service.json"),
    "TERM": "xterm-256color",
})


def wait(check, seconds=30):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            value = check()
        except (ConnectionError, urllib.error.URLError):
            value = None
        if value:
            return value
        time.sleep(.1)
    raise AssertionError("Timed out: " + str(check))


def api(method, path, body=None):
    record = json.loads(RECORD.read_text())
    request = urllib.request.Request(
        "http://127.0.0.1:" + str(record["port"]) + path,
        data=None if body is None else json.dumps(body).encode(), method=method,
        headers={"Content-Type": "application/json", "Authorization": "Basic " +
                 base64.b64encode(("opencode:" + record["password"]).encode()).decode()})
    with urllib.request.urlopen(request, timeout=15) as response:
        text = response.read()
        return json.loads(text) if text else None


def av(*args):
    result = subprocess.run([str(ROOT / "bin/agentview"), "--opencode-bin",
                             str(ROOT / "bin/opencode"), "--json", *args],
                            env=ENV, cwd=ROOT, capture_output=True, text=True, timeout=60)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def provider():
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with (ROOT / "provider-requests.jsonl").open("a") as log:
                log.write(json.dumps(body) + "\n")
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            if "FINISH_IMMEDIATELY" in json.dumps(body):
                for delta, finish in [({"role": "assistant", "content": "Finished."}, None), ({}, "stop")]:
                    self.wfile.write(("data: " + json.dumps({"id": "fixture", "object": "chat.completion.chunk",
                        "model": "fixture", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}) + "\n\n").encode())
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            try:
                if "WAIT_FIRST_RESPONSE" not in json.dumps(body):
                    self.wfile.write(("data: " + json.dumps({"id": "fixture", "object": "chat.completion.chunk",
                        "model": "fixture", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Work in progress."}, "finish_reason": None}]}) + "\n\n").encode())
                    self.wfile.flush()
                for _ in range(600):
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
                    time.sleep(1)
            except (BrokenPipeError, ConnectionResetError):
                pass

    http.server.ThreadingHTTPServer(("127.0.0.1", 48321), Handler).serve_forever()


def start_provider():
    if (ROOT / "provider.pid").exists():
        pid = int((ROOT / "provider.pid").read_text())
        check = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True)
        if str(Path(__file__).resolve()) + " provider" in check.stdout:
            return
    output = (ROOT / "provider.log").open("a")
    process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "provider"],
                               stdout=output, stderr=output, start_new_session=True)
    (ROOT / "provider.pid").write_text(str(process.pid))
    time.sleep(.3)
    assert process.poll() is None


def messages(id):
    return api("GET", f"/api/session/{id}/message?limit=200")["data"]


def continues(id):
    return sum((m["type"] == "user" and m.get("text") == "continue") or
               (m["type"] == "synthetic" and m.get("metadata", {}).get("notice") == "restart") for m in messages(id))


def assert_no_duplicate_inputs():
    with sqlite3.connect("file:" + str(PROFILE / "opencode.db") + "?mode=ro", uri=True) as db:
        assert db.execute("select count(*) from session_inbox where type='user' and json_extract(payload,'$.text')='continue'").fetchone()[0] == 0, "Duplicate continue is queued behind native recovery"
    for id in json.loads((ROOT / "sessions.json").read_text()).values():
        assert not any(m["type"] == "user" and m.get("text") == "continue" for m in messages(id)), "Unexpected user continuation was promoted"


def running(id):
    return id in api("GET", "/api/session/active")["data"]


def prepare():
    assert not RECORD.exists(), "Use a fresh isolated profile"
    (PROFILE / "config/opencode").mkdir(parents=True)
    (ROOT / "work").mkdir(exist_ok=True)
    config = {"model": "recovery-test/fixture", "provider": {"recovery-test": {
        "npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://127.0.0.1:48321/v1", "apiKey": "local-fixture"},
        "models": {"fixture": {"name": "Local recovery fixture", "limit": {"context": 20000, "output": 1000}}}}}}
    (PROFILE / "config/opencode/opencode.json").write_text(json.dumps(config))
    start_provider()
    service = subprocess.run([str(ROOT / "bin/opencode"), "service", "start"],
                             env=ENV, capture_output=True, text=True, timeout=30)
    assert service.returncode == 0, service.stderr
    print("bootstrap", av("opencode", "restart"), flush=True)
    ids = {}
    for name in ["completed", "stopped", "active", "waiting"]:
        session = api("POST", "/api/session", {"location": {"directory": str(ROOT / "work")},
                      "title": "Recovery test " + name})["data"]
        ids[name] = session["id"]
        api("POST", f"/api/session/{session['id']}/model", {"model": {"providerID": "recovery-test", "id": "fixture"}})
        api("POST", f"/api/session/{session['id']}/prompt", {"text": "FINISH_IMMEDIATELY" if name == "completed" else "WAIT_FIRST_RESPONSE" if name == "waiting" else "Keep working on " + name})
        wait(lambda: len(messages(session["id"])) >= 2)
        if name == "completed":
            wait(lambda: not running(session["id"]))
        if name == "stopped":
            wait(lambda: running(session["id"]))
            api("POST", f"/api/session/{session['id']}/interrupt", {})
            wait(lambda: not running(session["id"]))
    (ROOT / "sessions.json").write_text(json.dumps(ids))
    wait(lambda: running(ids["active"]))
    wait(lambda: running(ids["waiting"]))
    print("prepared", ids, flush=True)


def restart():
    ids = json.loads((ROOT / "sessions.json").read_text())
    before = json.loads(RECORD.read_text())
    report = av("opencode", "restart")
    print("restart", report, flush=True)
    wait(lambda: continues(ids["active"]) == 1)
    wait(lambda: continues(ids["waiting"]) == 1)
    assert continues(ids["completed"]) == continues(ids["stopped"]) == 0
    assert_no_duplicate_inputs()
    after = json.loads(RECORD.read_text())
    assert before["pid"] != after["pid"] and before["port"] == after["port"]
    wait(lambda: running(ids["active"]))
    wait(lambda: running(ids["waiting"]))
    wait(lambda: any(m["type"] == "assistant" and "completed" not in m.get("time", {}) for m in messages(ids["active"])))
    checkpoint()
    print("PASS controlled restart: only active root resumed; endpoint preserved", flush=True)


def checkpoint():
    ids = json.loads((ROOT / "sessions.json").read_text())
    (ROOT / "before-reboot.json").write_text(json.dumps({"boot": subprocess.check_output(["sysctl", "-n", "kern.boottime"]).decode(),
        "serverPID": json.loads(RECORD.read_text())["pid"], "continueCount": continues(ids["active"])}))
    subprocess.run(["sync"], check=True)


def recover():
    start_provider()
    ids = json.loads((ROOT / "sessions.json").read_text())
    before = json.loads((ROOT / "before-reboot.json").read_text())
    assert subprocess.check_output(["sysctl", "-n", "kern.boottime"]).decode() != before["boot"], "Must reboot VM first"
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
    process = subprocess.Popen([str(ROOT / "bin/agentview"), "--opencode-bin", str(ROOT / "bin/opencode"),
                                "--include-external", "--no-host-claude"],
                               stdin=slave, stdout=slave, stderr=slave, env=ENV, cwd=ROOT / "work", start_new_session=True)
    os.close(slave)
    (ROOT / "dashboard.pid").write_text(str(process.pid))
    log = (ROOT / "dashboard.ansi").open("wb")

    def drain():
        try:
            while True:
                data = os.read(master, 65536)
                if not data:
                    return
                log.write(data)
                log.flush()
        except OSError:
            pass

    threading.Thread(target=drain, daemon=True).start()
    try:
        wait(lambda: continues(ids["active"]) == before["continueCount"] + 1)
        wait(lambda: continues(ids["waiting"]) == before["continueCount"] + 1)
        assert continues(ids["completed"]) == continues(ids["stopped"]) == 0
        print("PASS actual reboot: active resumed once; completed/stopped untouched", flush=True)
        time.sleep(3)
        assert continues(ids["active"]) == before["continueCount"] + 1
        assert continues(ids["waiting"]) == before["continueCount"] + 1
        assert_no_duplicate_inputs()
        # Recreate an old restart's lost acknowledgement: the private journal
        # still names an already natively recovered session.
        legacy_journal()
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=10)
        os.close(master)


def legacy_journal():
    ids = json.loads((ROOT / "sessions.json").read_text())
    session = api("GET", f"/api/session/{ids['active']}")["data"]
    journal = RECORD.parent / "restart-pending.json"
    journal.write_text(json.dumps([{"id": ids["active"], "cwd": str(ROOT / "work"),
        "agent": session.get("agent", "build"),
        "providerId": session["model"]["providerID"], "modelId": session["model"]["id"],
        "variant": session["model"].get("variant")}]))
    journal.chmod(0o600)


def replay():
    ids = json.loads((ROOT / "sessions.json").read_text())
    before = continues(ids["active"])
    assert (RECORD.parent / "restart-pending.json").exists(), "Create the lost-ACK journal before testing replay"
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
    process = subprocess.Popen([str(ROOT / "bin/agentview"), "--opencode-bin", str(ROOT / "bin/opencode"),
                                "--include-external", "--no-host-claude"],
                               stdin=slave, stdout=slave, stderr=slave, env=ENV, cwd=ROOT / "work", start_new_session=True)
    os.close(slave)
    def drain():
        try:
            while os.read(master, 65536):
                pass
        except OSError:
            pass
    threading.Thread(target=drain, daemon=True).start()
    try:
        wait(lambda: not (RECORD.parent / "restart-pending.json").exists())
        time.sleep(2)
        assert continues(ids["active"]) == before, "Lost ACK admitted a duplicate continuation"
        assert continues(ids["waiting"]) == before, "Unjournaled active session resumed twice"
        assert continues(ids["completed"]) == continues(ids["stopped"]) == 0
        assert_no_duplicate_inputs()
        print("PASS second dashboard/lost-ACK replay: no duplicate continuation", flush=True)
        checkpoint()
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=10)
        os.close(master)


def cleanup():
    if RECORD.exists():
        try:
            for id in json.loads((ROOT / "sessions.json").read_text()).values():
                api("POST", f"/api/session/{id}/interrupt", {})
        except OSError:
            pass
        print(av("opencode", "stop"), flush=True)
    subprocess.run([str(ROOT / "bin/opencode"), "service", "stop"], env=ENV, timeout=20, check=True)
    pid = int((ROOT / "provider.pid").read_text())
    try:
        os.killpg(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass


if __name__ == "__main__":
    {"provider": provider, "prepare": prepare, "restart": restart, "recover": recover, "journal": legacy_journal, "replay": replay, "cleanup": cleanup}[sys.argv[1]]()
