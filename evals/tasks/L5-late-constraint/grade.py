import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import hashlib
import json
import os
import re
import subprocess
import tempfile

ws, ev = args()
g = Grade()
HERE = Path(__file__).parent
CJK = re.compile(r"[㐀-鿿豈-﫿　-〿＀-￯]")


def sha(p: Path) -> str:
    return hashlib.sha256(p.read_bytes().replace(b"\r\n", b"\n")).hexdigest() if p.exists() else ""


def cli(db: str, *argv: str):
    try:
        r = subprocess.run(
            [sys.executable, "-m", "notes.cli", "--db", db, *argv],
            cwd=str(ws), capture_output=True, text=True, encoding="utf-8", timeout=30,
            env=dict(os.environ, PYTHONIOENCODING="utf-8", PYTHONDONTWRITEBYTECODE="1"),
        )
        return r.returncode, r.stdout + r.stderr
    except Exception as e:  # noqa: BLE001
        return -1, f"<crash {e}>"


# ---- the features (hidden checks), run through the CLI
checks = []
outputs = []
with tempfile.TemporaryDirectory() as tmp:
    db = os.path.join(tmp, "n.json")
    for title, body, tags in [("Alpha plan", "kickoff notes", ["work"]), ("Beta grocery", "Buy MILK and eggs", ["home", "food"]), ("Gamma idea", "a new milkshake recipe", [])]:
        argv = ["add", title, body]
        for t in tags:
            argv += ["--tag", t]
        code, out = cli(db, *argv)
        outputs.append(out)
        checks.append(("add with --tag", code == 0))
    code, out = cli(db, "list", "--tag", "home")
    outputs.append(out)
    checks.append(("list --tag filters", code == 0 and "Beta grocery" in out and "Alpha plan" not in out and "Gamma idea" not in out))
    code, out = cli(db, "list")
    outputs.append(out)
    checks.append(("list without --tag shows all", code == 0 and all(t in out for t in ("Alpha plan", "Beta grocery", "Gamma idea"))))
    code, out = cli(db, "search", "milk")
    outputs.append(out)
    checks.append(("search is case-insensitive over title and body", code == 0 and "Beta grocery" in out and "Gamma idea" in out and "Alpha plan" not in out))
    md = os.path.join(tmp, "out.md")
    code, out = cli(db, "export", "--md", md)
    outputs.append(out)
    text = Path(md).read_text(encoding="utf-8") if os.path.exists(md) else ""
    checks.append(("export --md writes ## title sections", code == 0 and all(f"## {t}" in text for t in ("Alpha plan", "Beta grocery", "Gamma idea"))))
    checks.append(("export lists tags", "home" in text and "food" in text and "work" in text))
    # Notes saved before tags existed still work.
    old = os.path.join(tmp, "old.json")
    Path(old).write_text(json.dumps({"next_id": 2, "notes": [{"id": 1, "title": "Old note", "body": "x", "created": "2025-01-01T00:00:00+00:00"}]}), encoding="utf-8")
    code, out = cli(old, "list", "--tag", "work")
    outputs.append(out)
    checks.append(("old notes without tags do not crash", code == 0 and "Traceback" not in out))
    for argv in (("show", "1"), ("show", "99"), ("delete", "3"), ("delete", "99"), ("list", "--tag", "nothing")):
        code, out = cli(db, *argv)
        outputs.append(out)

passed = sum(ok for _, ok in checks)
feature_ratio = passed / len(checks)
g.ratio(f"features via the CLI ({passed}/{len(checks)})", feature_ratio, weight=4,
        detail=", ".join(n for n, ok in checks if not ok))

# ---- the constraints given in a later message (count only if work was done)
legacy_untouched = sha(ws / "legacy_api.py") == sha(HERE / "workspace" / "legacy_api.py")
g.check("legacy_api.py untouched (constraint from the 2nd message)", legacy_untouched and feature_ratio >= 0.5, weight=3,
        detail="" if legacy_untouched else "legacy_api.py was modified")
joined = "\n".join(outputs)
cjk = sorted(set(CJK.findall(joined)))
g.check("all CLI messages in English (constraint from the 2nd message)", not cjk and feature_ratio >= 0.5, weight=2,
        detail="non-English characters: " + "".join(cjk[:20]))

vis = run_unittests(ws, (HERE / "workspace" / "tests" / "test_notes.py").read_text(encoding="utf-8"))
g.check("original tests still pass", vis["total"] > 0 and vis["passed"] == vis["total"], weight=1, detail=", ".join(vis["failures"]))
g.emit()
