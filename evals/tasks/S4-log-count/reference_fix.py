import sys
from pathlib import Path

ws = Path(sys.argv[1])
root = ws / "logs"
files = lines = 0
for p in root.rglob("*"):
    if p.is_file() and p.suffix == ".log" and p.relative_to(root).parts[0] != "archive":
        n = sum(1 for l in p.read_text(encoding="utf-8").splitlines() if "ERROR" in l)
        if n:
            files += 1
            lines += n
(ws / "result.txt").write_text(f"files={files} lines={lines}\n", encoding="utf-8")
