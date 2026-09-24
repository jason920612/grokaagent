import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *  # noqa: E401,E702,F403
import os
import re
import subprocess
import tempfile

ws, ev = args()
g = Grade()
NAMES = ["RateLimitExceeded", "TokenBucket", "SlidingWindowCounter", "parse_rate", "retry_after", "limited"]
# Characters that only exist in Simplified Chinese (their Traditional forms differ).
SIMPLIFIED = set("这为个说们时进应该发实现对数据设请执错误类参务处动态单间认块过还让变简导开关计视频录读写线层级网络页码库")
TRADITIONAL_OK = set("數參說們時進應該發實現對據設置請執行返回錯誤類務處動態單間認塊過還讓變簡導開關計視頻錄讀寫線層級網絡頁碼庫")
SIMPLIFIED -= TRADITIONAL_OK  # guard against typos in the list above
# Facts that must appear in a section (any alternative, case-insensitive).
FACTS = {
    "TokenBucket": [("ValueError",), ("monotonic",), ("capacity",), ("refill_rate",)],
    "SlidingWindowCounter": [("ValueError",), ("60",)],
    "RateLimitExceeded": [("retry_after",)],
    "parse_rate": [("ValueError",), ("min",), ("hour", "小時"), ("day", "天")],
    "retry_after": [("inf", "無限", "無窮"), ("0.0", "0 ")],
    "limited": [("RateLimitExceeded",), ("raise_on_limit",), ("None",)],
}

doc = read(ws, "docs/API.md")
if not doc:
    g.check("docs/API.md exists", False, 10)
    g.emit()
    sys.exit(0)
g.check("docs/API.md exists", True, 0.5)

# Sections by ### heading.
sections = {}
parts = re.split(r"(?m)^###\s+(.*)$", doc)
for i in range(1, len(parts) - 1, 2):
    head, body = parts[i], parts[i + 1]
    for n in NAMES:
        if n not in sections and re.search(rf"(?<![A-Za-z0-9_]){re.escape(n)}(?![A-Za-z0-9_])", head):
            sections[n] = body
            break
g.ratio("one ### section per public name", len(sections) / len(NAMES), 2.0,
        f"missing {[n for n in NAMES if n not in sections]}")
sub_ok = sum(all(k in sections[n] for k in ("參數", "回傳", "例外")) for n in sections)
g.ratio("sections have 參數/回傳/例外", sub_ok / len(NAMES), 1.5)
ex_ok = sum("```python" in sections[n] for n in sections)
g.ratio("sections have a python example", ex_ok / len(NAMES), 1.0)
overview = parts[0]
g.check("overview (概覽) before the sections", "概覽" in doc and len(re.findall(r"[一-鿿]", overview)) >= 40, 0.5)

fact_hits = fact_total = 0
missed = []
for n, facts in FACTS.items():
    body = sections.get(n, "").lower()
    for alts in facts:
        fact_total += 1
        if any(a.lower() in body for a in alts):
            fact_hits += 1
        else:
            missed.append(f"{n}:{alts[0]}")
g.ratio("documents real behavior (exceptions, defaults, units)", fact_hits / fact_total, 2.5, f"missed {missed[:8]}")

cjk = re.findall(r"[一-鿿]", doc)
simp = [c for c in cjk if c in SIMPLIFIED]
g.check("substantial Chinese text (>= 400 CJK chars)", len(cjk) >= 400, 1.0, f"{len(cjk)} CJK chars")
g.check("Traditional Chinese: <= 2 simplified-only chars", len(cjk) >= 100 and len(simp) <= 2, 2.0,
        f"simplified found: {''.join(simp[:20])}")

blocks = re.findall(r"```python\s*\n(.*?)```", doc, flags=re.S)
ran = 0
fails = []
with tempfile.TemporaryDirectory() as tmp:
    for i, code in enumerate(blocks):
        f = Path(tmp) / f"ex{i}.py"
        f.write_text(code, encoding="utf-8")
        env = dict(os.environ, PYTHONPATH=str(ws), PYTHONIOENCODING="utf-8")
        try:
            r = subprocess.run([sys.executable, str(f)], cwd=str(ws), env=env, capture_output=True,
                               text=True, encoding="utf-8", errors="replace", timeout=10)
            if r.returncode == 0:
                ran += 1
            else:
                fails.append(f"#{i}: {r.stderr.strip().splitlines()[-1] if r.stderr.strip() else r.returncode}")
        except subprocess.TimeoutExpired:
            fails.append(f"#{i}: timeout")
g.ratio("python examples run without error", ran / len(blocks) if blocks else 0.0, 2.5,
        f"{ran}/{len(blocks)} ok; {fails[:4]}")
g.emit()
