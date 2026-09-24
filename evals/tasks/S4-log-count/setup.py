"""Generate logs/ deterministically: python setup.py <workspace>."""
import random
import sys
from pathlib import Path

ws = Path(sys.argv[1])
rng = random.Random(20260924)
levels = ["INFO", "DEBUG", "WARN", "ERROR", "error", "ERRORS", "Error"]
files = [
    "logs/app/api.log", "logs/app/auth.log", "logs/app/notes.txt", "logs/app/api.log.1",
    "logs/worker/queue.log", "logs/worker/2026/09/nightly job.log", "logs/worker/2026/09/cleanup.log",
    "logs/archive/api-2025.log", "logs/archive/old/auth-2024.log", "logs/db/slow.LOG", "logs/db/replica.log",
]
for rel in files:
    p = ws / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    n = rng.randint(40, 160)
    lines = []
    for i in range(n):
        lvl = rng.choices(levels, weights=[50, 20, 10, 6, 5, 2, 3])[0]
        lines.append(f"2026-09-{rng.randint(1, 23):02d}T{rng.randint(0, 23):02d}:{rng.randint(0, 59):02d}:00 {lvl} {rel.split('/')[-1]} event {i}")
    if rel.endswith("replica.log"):
        lines = [l.replace(" ERROR ", " INFO ").replace(" ERRORS ", " INFO ") for l in lines]
    newline = "\r\n" if "auth" in rel else "\n"
    p.write_text(newline.join(lines) + newline, encoding="utf-8", newline="")
