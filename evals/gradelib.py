"""Helpers for task graders.

A grader is `python grade.py <workspace> <events.jsonl>` and prints one JSON
line: {"score": 0..1, "checks": [{"name", "ok", "weight", "detail"}]}.
Graders never trust the agent's own report; they inspect files and run
hidden tests against the workspace.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


class Grade:
    def __init__(self) -> None:
        self.checks: list[dict] = []

    def check(self, name: str, ok: bool, weight: float = 1.0, detail: str = "") -> bool:
        return self.ratio(name, 1.0 if ok else 0.0, weight, detail) == 1.0

    def ratio(self, name: str, value: float, weight: float = 1.0, detail: str = "") -> float:
        value = max(0.0, min(1.0, float(value)))
        self.checks.append({"name": name, "ok": value == 1.0, "value": round(value, 4),
                            "weight": weight, "detail": str(detail)[:400]})
        return value

    def tests(self, name: str, res: dict, weight: float = 1.0) -> float:
        """Credit proportional to passing hidden tests."""
        value = res["passed"] / res["total"] if res["total"] else 0.0
        detail = ", ".join(res["failures"][:8]) or ""
        if res["total"] == 0:
            detail = res.get("output", "")[-300:]
        return self.ratio(f"{name} ({res['passed']}/{res['total']})", value, weight, detail)

    def emit(self, **extra) -> None:
        total = sum(c["weight"] for c in self.checks) or 1.0
        got = sum(c["weight"] * c["value"] for c in self.checks)
        out = {"score": round(got / total, 4), "checks": self.checks, **extra}
        # Callers decode as UTF-8; the Windows console default (cp950) is not.
        sys.stdout.reconfigure(encoding="utf-8")
        print(json.dumps(out, ensure_ascii=False))


def args() -> tuple[Path, Path]:
    ws = Path(sys.argv[1]).resolve()
    events = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else Path("/nonexistent")
    return ws, events


def read(ws: Path, rel: str) -> str | None:
    p = ws / rel
    try:
        return p.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None


def read_json(ws: Path, rel: str):
    text = read(ws, rel)
    if text is None:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


def run_unittests(ws: Path, test_source: str, timeout: int = 120) -> dict:
    """Run hidden unittest code with the workspace on sys.path.

    Returns {"total", "passed", "failures": [names...], "output"}.
    """
    runner = r'''
import json, sys, unittest, io
sys.path.insert(0, sys.argv[1])
import os
os.chdir(sys.argv[1])
ns = {}
src = open(sys.argv[2], encoding="utf-8").read()
mod = type(sys)("hidden_tests")
exec(compile(src, "hidden_tests.py", "exec"), mod.__dict__)
suite = unittest.defaultTestLoader.loadTestsFromModule(mod)
stream = io.StringIO()
res = unittest.TextTestRunner(stream=stream, verbosity=0).run(suite)
bad = [str(t[0]).split(" ")[0] for t in res.failures + res.errors]
print("@@RESULT@@" + json.dumps({"total": res.testsRun, "failed": bad, "log": stream.getvalue()[-3000:]}))
'''
    with tempfile.TemporaryDirectory() as tmp:
        tp = Path(tmp) / "hidden.py"
        tp.write_text(test_source, encoding="utf-8")
        rp = Path(tmp) / "runner.py"
        rp.write_text(runner, encoding="utf-8")
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONIOENCODING="utf-8")
        try:
            r = subprocess.run([sys.executable, str(rp), str(ws), str(tp)], capture_output=True,
                               text=True, encoding="utf-8", timeout=timeout, env=env, cwd=str(ws))
        except subprocess.TimeoutExpired:
            return {"total": 0, "passed": 0, "failures": ["<timeout>"], "output": "timeout"}
    marker = [l for l in r.stdout.splitlines() if l.startswith("@@RESULT@@")]
    if not marker:
        return {"total": 0, "passed": 0, "failures": ["<crash>"], "output": (r.stdout + r.stderr)[-2000:]}
    d = json.loads(marker[-1][len("@@RESULT@@"):])
    return {"total": d["total"], "passed": d["total"] - len(d["failed"]), "failures": d["failed"], "output": d["log"]}


def events(events_path: Path) -> list[dict]:
    out = []
    try:
        for line in events_path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    except OSError:
        pass
    return out
