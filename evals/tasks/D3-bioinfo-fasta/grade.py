import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *  # noqa: E401,E702,F403

sys.path.insert(0, str(Path(__file__).resolve().parent))
import refcalc  # noqa: E402

ws, ev = args()
g = Grade()
try:
    rows, rc = refcalc.report(ws)
except Exception as e:
    g.check("samples.fasta readable", False, 10, repr(e))
    g.emit()
    sys.exit(0)

text = read(ws, "report.tsv") or ""
lines = [l for l in text.splitlines() if l.strip()]
header_ok = bool(lines) and lines[0].strip().split("\t") == ["id", "length", "gc_percent", "longest_orf_nt"]
g.check("report.tsv header exact", header_ok, 0.5)
got = {}
order = []
for l in lines[1:]:
    parts = l.strip().split("\t")
    if len(parts) == 4:
        got[parts[0]] = parts[1:]
        order.append(parts[0])
g.check("rows in FASTA order", order == [r[0] for r in rows], 0.5, f"got {order[:5]}...")

n = len(rows)
len_ok = gc_ok = orf_ok = gc_fmt = 0
bad = {"length": "", "gc": "", "orf": ""}
for rid, length, gc, orf in rows:
    r = got.get(rid)
    if not r:
        continue
    try:
        if int(r[0]) == length:
            len_ok += 1
        elif not bad["length"]:
            bad["length"] = f"{rid}: got {r[0]} want {length}"
    except ValueError:
        pass
    try:
        if abs(float(r[1]) - float(gc)) <= 0.0051:
            gc_ok += 1
            gc_fmt += len(r[1].split(".")[-1]) == 2 and "." in r[1]
        elif not bad["gc"]:
            bad["gc"] = f"{rid}: got {r[1]} want {gc}"
    except ValueError:
        pass
    try:
        if int(r[2]) == orf:
            orf_ok += 1
        elif not bad["orf"]:
            bad["orf"] = f"{rid}: got {r[2]} want {orf}"
    except ValueError:
        pass
g.ratio("length column", len_ok / n, 1.0, bad["length"])
g.ratio("gc_percent column (N excluded, 2 decimals)", gc_ok / n, 2.0, bad["gc"])
g.ratio("gc_percent printed with two decimals", gc_fmt / n, 0.5)
g.ratio("longest_orf_nt column (6 frames)", orf_ok / n, 3.0, bad["orf"])

fa = read(ws, "revcomp.fasta") or ""
recs = []
cur = None
wrap_ok = True
for l in fa.splitlines():
    l = l.strip()
    if not l:
        continue
    if l.startswith(">"):
        cur = [l[1:].split()[0], []]
        recs.append(cur)
    elif cur is not None:
        cur[1].append(l)
for rid, parts in recs:
    if any(len(p) != 60 for p in parts[:-1]) or (parts and len(parts[-1]) > 60):
        wrap_ok = False
hits = 0
for i, (rid, seq) in enumerate(rc):
    if i < len(recs) and recs[i][0] == rid and "".join(recs[i][1]) == seq:
        hits += 1
g.ratio("revcomp.fasta: 3 shortest, order, sequence", hits / 3, 2.0,
        f"want ids {[r[0] for r in rc]} got {[r[0] for r in recs]}")
g.check("revcomp.fasta wrapped at 60", bool(recs) and wrap_ok, 0.5)
g.emit()
