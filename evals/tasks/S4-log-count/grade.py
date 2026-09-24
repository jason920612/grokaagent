import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import re
import subprocess
import tempfile

ws, ev = args()
g = Grade()


def expected() -> tuple[int, int]:
    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run([sys.executable, str(Path(__file__).parent / "setup.py"), tmp], check=True)
        root = Path(tmp) / "logs"
        files = lines = 0
        for p in root.rglob("*"):
            if not p.is_file() or p.suffix != ".log":
                continue
            if "archive" in p.relative_to(root).parts[:1]:
                continue
            n = sum(1 for l in p.read_text(encoding="utf-8").splitlines() if "ERROR" in l)
            if n:
                files += 1
                lines += n
        return files, lines


want_files, want_lines = expected()
text = (read(ws, "result.txt") or "").strip()
m = re.fullmatch(r"files=(\d+)\s+lines=(\d+)", text)
g.check("result.txt has the exact format 'files=N lines=M'", bool(m), weight=1, detail=repr(text[:80]))
nums = re.findall(r"(files|lines)\s*=\s*(\d+)", text)
got = {k: int(v) for k, v in nums}
g.check(f"files = {want_files}", got.get("files") == want_files, weight=2, detail=f"got {got.get('files')}")
g.check(f"lines = {want_lines}", got.get("lines") == want_lines, weight=2, detail=f"got {got.get('lines')}")
g.emit()
