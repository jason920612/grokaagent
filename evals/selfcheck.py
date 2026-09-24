#!/usr/bin/env python3
"""Validate every task's grader without calling a model.

For each task: grade the untouched workspace (must score below --max-blank)
and the workspace with `reference/` copied over it (must score >= 0.99).
Tasks whose reference is a delegation (events-based checks) may provide
`reference_events.jsonl` to stand in for the run's events.

    python evals/selfcheck.py            # all tasks
    python evals/selfcheck.py S1 D2      # some
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TASKS = ROOT / "tasks"


def prepare(task_dir: Path, dest: Path, with_reference: bool) -> None:
    src = task_dir / "workspace"
    if src.is_dir():
        shutil.copytree(src, dest)
    else:
        dest.mkdir(parents=True)
    setup = task_dir / "setup.py"
    if setup.is_file():
        subprocess.run([sys.executable, str(setup), str(dest)], check=True)
    ref = task_dir / "reference"
    if with_reference and ref.is_dir():
        shutil.copytree(ref, dest, dirs_exist_ok=True)
    fix = task_dir / "reference_fix.py"
    if with_reference and fix.is_file():
        subprocess.run([sys.executable, str(fix), str(dest)], check=True)


def grade(task_dir: Path, ws: Path, events: Path) -> dict:
    r = subprocess.run([sys.executable, str(task_dir / "grade.py"), str(ws), str(events)],
                       capture_output=True, text=True, encoding="utf-8", timeout=300)
    try:
        return json.loads(r.stdout.strip().splitlines()[-1])
    except Exception:
        return {"score": -1, "error": (r.stdout + r.stderr)[-1500:]}


def main() -> int:
    want = sys.argv[1:]
    max_blank = 0.35
    bad = 0
    for d in sorted(TASKS.iterdir()):
        if not (d / "task.json").is_file():
            continue
        tid = json.loads((d / "task.json").read_text(encoding="utf-8"))["id"]
        if want and not any(tid.startswith(w) for w in want):
            continue
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            blank_ws, ref_ws = tmp / "blank", tmp / "ref"
            prepare(d, blank_ws, False)
            prepare(d, ref_ws, True)
            no_events = tmp / "none.jsonl"
            no_events.write_text("", encoding="utf-8")
            ref_events = d / "reference_events.jsonl"
            g0 = grade(d, blank_ws, no_events)
            g1 = grade(d, ref_ws, ref_events if ref_events.is_file() else no_events)
        ok = 0 <= g0.get("score", -1) <= max_blank and g1.get("score", 0) >= 0.99
        bad += not ok
        print(f"{'OK ' if ok else 'BAD'} {tid:28s} blank={g0.get('score')}  reference={g1.get('score')}")
        if not ok:
            for label, g in (("blank", g0), ("reference", g1)):
                if "error" in g:
                    print(f"    {label} error: {g['error']}")
                for c in g.get("checks", []):
                    if (label == "reference" and not c["ok"]) or (label == "blank" and c["ok"]):
                        print(f"    {label}: {'+' if c['ok'] else '-'} {c['name']}  {c['detail'][:200]}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
