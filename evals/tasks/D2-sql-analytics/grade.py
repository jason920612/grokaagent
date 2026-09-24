import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *  # noqa: E401,E702,F403

sys.path.insert(0, str(Path(__file__).resolve().parent))
import refcalc  # noqa: E402

ws, ev = args()
g = Grade()
MONEY, RATE, DAYS = 0.011, 0.00051, 0.011


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def close(a, b, tol):
    a = num(a)
    return a is not None and abs(a - b) <= tol


try:
    want = refcalc.answers(ws)
except Exception as e:  # database deleted or damaged by the agent
    g.check("shop.db readable", False, 10, repr(e))
    g.emit()
    sys.exit(0)

got = read_json(ws, "answers.json")
if not isinstance(got, dict):
    got = {}

# Q1: per-month credit.
g1 = got.get("q1_monthly_net_revenue_2025") or {}
w1 = want["q1_monthly_net_revenue_2025"]
hits = sum(1 for k, v in w1.items() if isinstance(g1, dict) and close(g1.get(k), v, MONEY))
bad = [k for k, v in w1.items() if not (isinstance(g1, dict) and close(g1.get(k), v, MONEY))]
g.ratio("q1 monthly net revenue", hits / 12, 1.0, f"wrong months {bad[:4]}; e.g. want {w1.get(bad[0]) if bad else ''} got {g1.get(bad[0]) if bad and isinstance(g1, dict) else ''}")

# Q2: position-wise credit.
g2 = got.get("q2_top5_customers") or []
w2 = want["q2_top5_customers"]
hit2 = 0
for i, w in enumerate(w2):
    try:
        r = g2[i]
        if int(r["customer_id"]) == w["customer_id"] and close(r["net_spend"], w["net_spend"], MONEY):
            hit2 += 1
    except Exception:
        pass
g.ratio("q2 top 5 customers", hit2 / 5, 1.0, f"want {w2} got {str(g2)[:200]}")

g.check("q3 repeat purchase rate", close(got.get("q3_repeat_purchase_rate"), want["q3_repeat_purchase_rate"], RATE),
        1.0, f"want {want['q3_repeat_purchase_rate']} got {got.get('q3_repeat_purchase_rate')}")

g4 = got.get("q4_highest_refund_rate_category") or {}
w4 = want["q4_highest_refund_rate_category"]
ok4 = isinstance(g4, dict) and g4.get("category") == w4["category"]
ok4r = isinstance(g4, dict) and close(g4.get("rate"), w4["rate"], RATE)
g.ratio("q4 highest refund-rate category", (0.5 if ok4 else 0) + (0.5 if ok4r else 0), 1.0, f"want {w4} got {g4}")

g.check("q5 avg days first->second order", close(got.get("q5_avg_days_first_to_second"), want["q5_avg_days_first_to_second"], DAYS),
        1.0, f"want {want['q5_avg_days_first_to_second']} got {got.get('q5_avg_days_first_to_second')}")

g6 = got.get("q6_cohort_2025_03") or {}
w6 = want["q6_cohort_2025_03"]
parts = 0
if isinstance(g6, dict):
    parts += num(g6.get("cohort_size")) == w6["cohort_size"]
    parts += sum(close(g6.get(m), w6[m], RATE) for m in ("2025-04", "2025-05", "2025-06"))
g.ratio("q6 cohort retention", parts / 4, 1.0, f"want {w6} got {g6}")

sql = read(ws, "queries.sql") or ""
g.check("queries.sql saved with SELECT", "select" in sql.lower(), 0.5)
g.emit()
