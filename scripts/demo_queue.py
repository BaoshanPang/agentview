#!/usr/bin/env python3
"""Record a short video of agent-bouncer's job queue inside agentview.

Run from the repo root after `cargo build --release`:

    uv run --with pillow scripts/demo_queue.py

Needs vhs, ffmpeg and a real `agent-bouncer` on PATH or in ~/.local/bin. The
dashboard runs on fixtures/demo-sessions.json, but the queue is real: four
jobs go through agent-bouncer with two slots, so two run and two wait, and the
dashboard shows them as they move. Writes target/demo/queue.mp4 and
target/demo/queue.gif.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import demo  # noqa: E402

REPO = demo.REPO
OUTPUT_DIR = REPO / "target/demo"

# (VHS command, badge keys, badge caption, seconds to wait afterwards).
STEPS = [
    ("Sleep", None, None, 24.0),
]

# Jobs started through agent-bouncer: (label, folder under the demo work dir, seconds, delay before queueing).
JOBS = [
    ("cargo test", "shop-api", 14, 0.0),
    ("playwright e2e", "shop-api", 26, 0.4),
    ("lint + typecheck", "shop-web", 8, 0.8),
    ("docs build", "shop-api/docs", 8, 1.2),
]
SLOTS = 2


def find_bouncer() -> str:
    found = shutil.which("agent-bouncer") or str(Path.home() / ".local/bin/agent-bouncer")
    if not Path(found).is_file():
        sys.exit("agent-bouncer is required (see https://github.com/moritzWa/agent-bouncer)")
    return found


def write_fixture(path: Path, work: Path) -> None:
    document = json.loads(demo.FIXTURE.read_text())
    now_ms = int(time.time() * 1000)
    for index, session in enumerate(document["sessions"]):
        session["updated_at"] = now_ms - (2 + index * 4) * 60_000
        session["cwd"] = str(work / session["cwd"].removeprefix("/work/"))
        if session["name"] == "api-docs":
            session["cwd"] = str(work / "shop-api/docs")
    document["demo"]["restorable"] = []
    path.write_text(json.dumps(document))


def seed_history(bouncer_dir: Path) -> None:
    """Past runs of each job, so the queue can say when it expects to clear."""
    bouncer_dir.mkdir(parents=True, exist_ok=True)
    lines = [
        json.dumps({"ts": 1, "ev": "finish", "job": f"past-{index}", "label": label, "dur_ms": seconds * 1000})
        for index, (label, _, seconds, _) in enumerate(JOBS)
    ]
    (bouncer_dir / "events.jsonl").write_text("\n".join(lines) + "\n")


def tape(env: dict[str, str], fixture: Path, work: Path, go: Path, mp4: Path) -> str:
    exports = " ".join(f"{key}={value}" for key, value in env.items())
    lines = [
        f'Output "{mp4}"',
        'Set Shell "bash"',
        f"Set FontSize {demo.FONT_SIZE}",
        f"Set Width {demo.WIDTH}",
        f"Set Height {demo.HEIGHT}",
        f"Set Padding {demo.PADDING}",
        f"Set Framerate {demo.FRAMERATE}",
        f"Set Theme {json.dumps(demo.THEME)}",
        'Set FontFamily "Menlo"',
        "Set LineHeight 1.3",
        f"Set TypingSpeed {demo.TYPING_MS}ms",
        "",
        "Hide",
        f"Type@2ms \"export {exports} PS1='$ '; cd {work}/shop-api; clear\" Enter",
        f'Type@2ms "touch {go}; {REPO}/target/release/agentview --theme light --harness opencode --fixture {fixture}" Enter',
        "Sleep 2s",
        "Show",
    ]
    for command, _, _, wait in STEPS:
        if command != "Sleep":
            lines.append(command)
        lines.append(f"Sleep {int(wait * 1000)}ms")
    lines += ["Hide", "Ctrl+C", ""]
    return "\n".join(lines)


def badge_events() -> list[tuple[float, float, list[str], str]]:
    events, clock = [], 0.0
    for command, keys, caption, wait in STEPS:
        start = clock
        clock += demo.step_duration(command) + wait
        if keys:
            events.append((start, keys, caption))
    return [(start, start + demo.BADGE_SECONDS, keys, caption) for start, keys, caption in events]


def start_jobs(bouncer: str, env: dict[str, str], work: Path, go: Path) -> threading.Thread:
    def launch() -> None:
        while not go.exists():
            time.sleep(0.05)
        started = time.time()
        children = []
        for label, folder, seconds, delay in JOBS:
            time.sleep(max(0.0, started + delay - time.time()))
            children.append(subprocess.Popen(
                [bouncer, "run", "--wait", "--label", label, "--", "sleep", str(seconds)],
                cwd=work / folder,
                env={**{k: v for k, v in os.environ.items() if k != "AGENT_BOUNCER_JOB"}, **env},
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ))
        for child in children:
            child.wait()

    thread = threading.Thread(target=launch, daemon=True)
    thread.start()
    return thread


def main() -> None:
    binary = REPO / "target/release/agentview"
    if not binary.exists():
        sys.exit("build first: cargo build --release")
    for tool in ("vhs", "ffmpeg"):
        if shutil.which(tool) is None:
            sys.exit(f"{tool} is required")
    bouncer = find_bouncer()

    with tempfile.TemporaryDirectory(prefix="agentview-queue-demo-") as temp:
        temp = Path(temp).resolve()
        # Sessions under a temp directory are hidden, so the home lives in target/.
        home = OUTPUT_DIR / "queue-demo-home"
        shutil.rmtree(home, ignore_errors=True)
        work = home / "work"
        for _, folder, _, _ in JOBS:
            (work / folder).mkdir(parents=True, exist_ok=True)
        demo.seed_state(temp / "env")
        env = {
            "HOME": str(home),
            "XDG_STATE_HOME": str(temp / "env/state"),
            "PATH": f"{Path(bouncer).parent}:{os.environ['PATH']}",
            "AGENT_BOUNCER_DIR": str(temp / "bouncer"),
            "AGENT_BOUNCER_SLOTS": str(SLOTS),
            "AGENT_BOUNCER_MIN_FREE_GB": "0",
            "AGENT_BOUNCER_MAX_CPU": "100",
        }
        fixture = temp / "fixture.json"
        write_fixture(fixture, work)
        seed_history(temp / "bouncer")
        go = temp / "go"
        raw = temp / "raw.mp4"
        tape_path = temp / "queue.tape"
        tape_path.write_text(tape(env, fixture, work, go, raw))
        jobs = start_jobs(bouncer, env, work, go)
        demo.run("vhs", tape_path)
        jobs.join(timeout=60)

        inputs: list[str | Path] = []
        filters = []
        label = "0:v"
        for index, (start, end, keys, caption) in enumerate(badge_events(), start=1):
            badge = temp / f"badge-{index}.png"
            demo.draw_badge(keys, caption, badge)
            inputs += ["-i", badge]
            filters.append(
                f"[{label}][{index}:v]overlay=x=W-w-{28 * demo.SCALE}:y=H-h-{6 * demo.SCALE}"
                f":enable='between(t,{start:.3f},{end:.3f})'[v{index}]"
            )
            label = f"v{index}"
        OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
        mp4 = OUTPUT_DIR / "queue.mp4"
        graph = ["-filter_complex", ";".join(filters), "-map", f"[{label}]"] if filters else []
        demo.run(
            "ffmpeg", "-y", "-loglevel", "error", "-i", raw, *inputs, *graph,
            "-c:v", "libx264", "-crf", "16", "-pix_fmt", "yuv420p", mp4,
        )

        palette = temp / "palette.png"
        scale = f"fps={demo.GIF_FPS},scale={demo.GIF_WIDTH}:-1:flags=lanczos"
        demo.run(
            "ffmpeg", "-y", "-loglevel", "error", "-i", mp4,
            "-vf", f"{scale},palettegen=max_colors=128:stats_mode=diff", palette,
        )
        gif = OUTPUT_DIR / "queue.gif"
        demo.run(
            "ffmpeg", "-y", "-loglevel", "error", "-i", mp4, "-i", palette,
            "-lavfi", f"{scale}[x];[x][1:v]paletteuse=dither=none:diff_mode=rectangle", gif,
        )
    shutil.rmtree(home, ignore_errors=True)
    print(f"wrote {mp4} and {gif}")


if __name__ == "__main__":
    main()
