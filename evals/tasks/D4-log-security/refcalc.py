"""Reference incident detection per the D4 rules."""
import collections
import datetime as dt
import gzip
import json
import re
from pathlib import Path
from urllib.parse import unquote

LINE = re.compile(r'^(\S+) \S+ \S+ \[([^\]]+)\] "(\S+) (\S+) [^"]*" (\d{3}) (\S+) "[^"]*" "([^"]*)"$')
SQLI = ("' or '", "union select", "sleep(", "information_schema")


def records(ws):
    logs = Path(ws) / "logs"
    for f in sorted(logs.iterdir()):
        if f.name.endswith(".gz"):
            text = gzip.open(f, "rt", encoding="utf-8").read()
        else:
            text = f.read_text(encoding="utf-8")
        for line in text.splitlines():
            m = LINE.match(line)
            if not m:
                continue
            try:
                t = dt.datetime.strptime(m.group(2), "%d/%b/%Y:%H:%M:%S %z").astimezone(dt.timezone.utc)
            except ValueError:
                continue
            yield {"ip": m.group(1), "t": t, "method": m.group(3), "target": m.group(4),
                   "status": int(m.group(5)), "ua": m.group(7)}


def report(ws):
    recs = list(records(ws))
    fails = collections.defaultdict(list)
    trav, sqli = set(), set()
    ua404 = collections.Counter()
    fx = collections.Counter()
    for r in recs:
        if r["method"] == "POST" and r["target"] == "/login" and r["status"] == 401:
            fails[r["ip"]].append(r["t"])
        dec = unquote(r["target"]).lower()
        if "../" in dec or "..\\" in dec:
            trav.add(r["ip"])
        if any(s in dec for s in SQLI):
            sqli.add(r["ip"])
        if r["status"] == 404:
            ua404[r["ua"]] += 1
        if 500 <= r["status"] <= 599:
            fx[r["t"].strftime("%Y-%m-%dT%H:%M")] += 1
    brute = []
    for ip, ts in fails.items():
        ts.sort()
        j = 0
        for i in range(len(ts)):
            while (ts[i] - ts[j]).total_seconds() > 300:
                j += 1
            if i - j + 1 >= 10:
                brute.append(ip)
                break
    top = sorted(ua404.items(), key=lambda kv: (-kv[1], kv[0]))[:3]
    peak = sorted(fx.items(), key=lambda kv: (-kv[1], kv[0]))[0]
    return {
        "total_requests": len(recs),
        "brute_force_ips": sorted(brute),
        "traversal_ips": sorted(trav),
        "sqli_ips": sorted(sqli),
        "top_404_user_agents": [{"user_agent": u, "count": c} for u, c in top],
        "peak_5xx_minute": peak[0],
        "peak_5xx_count": peak[1],
    }


def write(ws):
    (Path(ws) / "incident_report.json").write_text(json.dumps(report(ws), indent=2), encoding="utf-8")
