import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *  # noqa: E401,E702,F403
import csv
import io

sys.path.insert(0, str(Path(__file__).resolve().parent))
import refcalc  # noqa: E402

ws, ev = args()
g = Grade()
rows, summary = refcalc.schedule()
TOL = 0.011

text = read(ws, "schedule.csv")
got = []
header_ok = False
if text:
    try:
        reader = list(csv.reader(io.StringIO(text.strip())))
        header_ok = [h.strip() for h in reader[0]] == ["month", "payment", "interest", "principal", "extra", "balance"]
        for r in reader[1:]:
            if not r or not "".join(r).strip():
                continue
            got.append([float(x) for x in r])
    except Exception:
        got = []
g.check("schedule.csv header exact", header_ok, 0.5)
g.check("row count equals payoff months", len(got) == len(rows), 1.0, f"got {len(got)} want {len(rows)}")

by_month = {int(r[0]): r for r in got if len(r) == 6}
match = 0
first_bad = ""
for m, p, i, pr, e, b in rows:
    r = by_month.get(m)
    want = [m, float(p), float(i), float(pr), float(e), float(b)]
    if r and all(abs(a - w) <= TOL for a, w in zip(r, want)):
        match += 1
    elif not first_bad:
        first_bad = f"month {m}: got {r} want {want}"
g.ratio("rows matching reference (±0.01)", match / len(rows), 6.0, first_bad)

# Key rows are checked separately so a near-miss still shows where it broke.
for m in (24, 60, 61, len(rows)):
    r = by_month.get(m)
    ref = rows[m - 1]
    want = [m] + [float(x) for x in ref[1:]]
    ok = bool(r) and all(abs(a - w) <= TOL for a, w in zip(r, want))
    g.check(f"key month {m}", ok, 0.5, f"got {r} want {want}")

s = read_json(ws, "summary.json") or {}
for k, w in (("total_interest", 1.0), ("payoff_month", 1.0), ("payment_before_rate_change", 0.5), ("payment_after_rate_change", 0.5)):
    v = s.get(k) if isinstance(s, dict) else None
    try:
        ok = v is not None and abs(float(v) - float(summary[k])) <= TOL
    except (TypeError, ValueError):
        ok = False
    g.check(f"summary.{k}", ok, w, f"got {v} want {summary[k]}")
g.emit()
