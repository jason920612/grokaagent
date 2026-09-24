import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *  # noqa: E401,E702,F403

sys.path.insert(0, str(Path(__file__).resolve().parent))
import refcalc  # noqa: E402

ws, ev = args()
g = Grade()
try:
    want = refcalc.report(ws)
except Exception as e:
    g.check("logs readable", False, 10, repr(e))
    g.emit()
    sys.exit(0)

got = read_json(ws, "incident_report.json")
if not isinstance(got, dict):
    got = {}


def ip_set_score(key, weight):
    w = set(want[key])
    v = got.get(key)
    if not isinstance(v, list):
        g.ratio(key, 0.0, weight, f"want {sorted(w)}")
        return
    s = {str(x) for x in v}
    # Jaccard: misses and false positives both cost.
    union = w | s
    score = len(w & s) / len(union) if union else 1.0
    order_ok = [str(x) for x in v] == sorted(s)
    g.ratio(key, score * (1.0 if order_ok else 0.9), weight,
            f"missing {sorted(w - s)} extra {sorted(s - w)}{'' if order_ok else ' (not sorted)'}")


g.check("total_requests", got.get("total_requests") == want["total_requests"], 1.0,
        f"got {got.get('total_requests')} want {want['total_requests']}")
ip_set_score("brute_force_ips", 2.0)
ip_set_score("traversal_ips", 1.5)
ip_set_score("sqli_ips", 1.5)

ua = got.get("top_404_user_agents")
hits = 0
if isinstance(ua, list):
    for i, w in enumerate(want["top_404_user_agents"]):
        try:
            if ua[i]["user_agent"] == w["user_agent"] and int(ua[i]["count"]) == w["count"]:
                hits += 1
        except Exception:
            pass
g.ratio("top_404_user_agents", hits / 3, 1.5, f"want {want['top_404_user_agents']}")
g.check("peak_5xx_minute", got.get("peak_5xx_minute") == want["peak_5xx_minute"], 1.0,
        f"got {got.get('peak_5xx_minute')} want {want['peak_5xx_minute']}")
g.check("peak_5xx_count", got.get("peak_5xx_count") == want["peak_5xx_count"], 0.5,
        f"got {got.get('peak_5xx_count')} want {want['peak_5xx_count']}")
g.emit()
