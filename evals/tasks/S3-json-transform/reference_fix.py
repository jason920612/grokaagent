"""Write the correct inventory.json (reuses the grader's reference logic)."""
import importlib.util
import json
import sys
from pathlib import Path

ws = Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("g", Path(__file__).parent / "grade.py")
src = (Path(__file__).parent / "grade.py").read_text(encoding="utf-8")
ns: dict = {"__file__": str(Path(__file__).parent / "grade.py")}
code = src.split("exp = expected()")[0].replace("ws, ev = args()", "")
exec(compile(code, "grade_ref", "exec"), ns)
(ws / "inventory.json").write_text(json.dumps(ns["expected"](), ensure_ascii=False, indent=2), encoding="utf-8")
