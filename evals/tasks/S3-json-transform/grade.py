import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import csv

ws, ev = args()
g = Grade()


def expected() -> dict:
    src = Path(__file__).parent / "workspace" / "servers.csv"
    regions, retired, maint, tags = {}, [], [], {}
    with open(src, encoding="utf-8", newline="") as fh:
        for row in csv.DictReader(fh):
            row = {k.strip(): (v or "").strip() for k, v in row.items()}
            name, status = row["name"], row["status"].lower()
            if status == "retired":
                retired.append(name)
                continue
            if status == "maintenance":
                maint.append(name)
            r = regions.setdefault(row["region"].lower(), {"servers": [], "total_cpu": 0, "total_mem_gb": 0.0})
            r["servers"].append(name)
            r["total_cpu"] += int(row["cpu"])
            r["total_mem_gb"] += float(row["mem_gb"])
            for t in {t.strip().lower() for t in row["tags"].split(";") if t.strip()}:
                tags[t] = tags.get(t, 0) + 1
    for r in regions.values():
        r["servers"].sort()
        r["total_mem_gb"] = round(r["total_mem_gb"], 1)
    return {"regions": regions, "retired": sorted(retired), "maintenance": sorted(maint), "by_tag": tags}


exp = expected()
got = read_json(ws, "inventory.json")
if not isinstance(got, dict):
    got = {}
g.check("inventory.json is a JSON object", bool(got), weight=0.5)

regions = got.get("regions") if isinstance(got.get("regions"), dict) else {}
ok = 0
for name, want in exp["regions"].items():
    have = regions.get(name)
    good = isinstance(have, dict) and have.get("servers") == want["servers"] \
        and have.get("total_cpu") == want["total_cpu"] \
        and isinstance(have.get("total_mem_gb"), (int, float)) and abs(have["total_mem_gb"] - want["total_mem_gb"]) < 0.051
    ok += good
g.ratio("regions (per region: servers, total_cpu, total_mem_gb)", ok / len(exp["regions"]) if set(regions) == set(exp["regions"]) else ok / (len(exp["regions"]) + 1),
        weight=3, detail=f"want keys {sorted(exp['regions'])}, got {sorted(regions)}")
g.check("retired list", got.get("retired") == exp["retired"], weight=1, detail=f"got {got.get('retired')!r}")
g.check("maintenance list", got.get("maintenance") == exp["maintenance"], weight=1, detail=f"got {got.get('maintenance')!r}")
bt = got.get("by_tag") if isinstance(got.get("by_tag"), dict) else {}
right = sum(1 for k, v in exp["by_tag"].items() if bt.get(k) == v)
extra = len(set(bt) - set(exp["by_tag"]))
g.ratio("by_tag counts", max(0, right - extra) / len(exp["by_tag"]), weight=2,
        detail=f"want {exp['by_tag']}, got {bt}")
g.emit()
