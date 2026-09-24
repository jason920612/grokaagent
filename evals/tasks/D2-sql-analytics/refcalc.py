"""Reference answers for D2, computed with SQL straight from shop.db."""
import json
import sqlite3
from pathlib import Path

PAID_LINES = """
    SELECT o.id AS order_id, o.customer_id, o.order_date, oi.quantity * oi.unit_price AS line, p.category
    FROM orders o JOIN order_items oi ON oi.order_id = o.id JOIN products p ON p.id = oi.product_id
    WHERE o.status = 'paid'
"""


def answers(ws) -> dict:
    con = sqlite3.connect(Path(ws) / "shop.db")
    q = lambda sql, *a: con.execute(sql, a).fetchall()  # noqa: E731

    gross = dict(q(f"SELECT substr(order_date,1,7), SUM(line) FROM ({PAID_LINES}) GROUP BY 1"))
    ref = dict(q("SELECT substr(refund_date,1,7), SUM(amount) FROM refunds GROUP BY 1"))
    q1 = {}
    for m in range(1, 13):
        k = f"2025-{m:02d}"
        q1[k] = round(gross.get(k, 0.0) - ref.get(k, 0.0), 2)

    rows = q(f"""
        WITH spend AS (SELECT customer_id, SUM(line) s FROM ({PAID_LINES}) GROUP BY customer_id),
             rf AS (SELECT o.customer_id, SUM(r.amount) s FROM refunds r JOIN orders o ON o.id = r.order_id
                    WHERE o.status = 'paid' GROUP BY o.customer_id)
        SELECT spend.customer_id, spend.s - COALESCE(rf.s, 0) AS net
        FROM spend LEFT JOIN rf ON rf.customer_id = spend.customer_id
        ORDER BY net DESC, spend.customer_id ASC LIMIT 5
    """)
    q2 = [{"customer_id": c, "net_spend": round(n, 2)} for c, n in rows]

    (buyers, repeaters), = q("""
        SELECT COUNT(*), SUM(n >= 2) FROM (SELECT customer_id, COUNT(*) n FROM orders WHERE status='paid' GROUP BY customer_id)
    """)
    q3 = round(repeaters / buyers, 4)

    cats = q(f"""
        WITH lines AS ({PAID_LINES}),
             tot AS (SELECT order_id, SUM(line) t FROM lines GROUP BY order_id),
             rf AS (SELECT order_id, SUM(amount) a FROM refunds GROUP BY order_id)
        SELECT l.category, SUM(l.line) gross, SUM(COALESCE(rf.a, 0) * l.line / tot.t) refunded
        FROM lines l JOIN tot ON tot.order_id = l.order_id LEFT JOIN rf ON rf.order_id = l.order_id
        GROUP BY l.category
    """)
    best = max(cats, key=lambda r: (r[2] / r[1], r[0]))
    q4 = {"category": best[0], "rate": round(best[2] / best[1], 4)}

    (avg,), = q("""
        WITH ranked AS (SELECT customer_id, order_date,
                          ROW_NUMBER() OVER (PARTITION BY customer_id ORDER BY order_date, id) rn
                        FROM orders WHERE status='paid')
        SELECT AVG(julianday(b.order_date) - julianday(a.order_date))
        FROM ranked a JOIN ranked b ON a.customer_id = b.customer_id AND a.rn = 1 AND b.rn = 2
    """)
    q5 = round(avg, 2)

    cohort = [c for (c,) in q("""
        SELECT customer_id FROM orders WHERE status='paid' GROUP BY customer_id
        HAVING substr(MIN(order_date),1,7) = '2025-03'
    """)]
    q6 = {"cohort_size": len(cohort)}
    for m in ("2025-04", "2025-05", "2025-06"):
        active = {c for (c,) in q("SELECT DISTINCT customer_id FROM orders WHERE status='paid' AND substr(order_date,1,7)=?", m)}
        q6[m] = round(sum(c in active for c in cohort) / len(cohort), 4) if cohort else 0.0
    con.close()
    return {
        "q1_monthly_net_revenue_2025": q1,
        "q2_top5_customers": q2,
        "q3_repeat_purchase_rate": q3,
        "q4_highest_refund_rate_category": q4,
        "q5_avg_days_first_to_second": q5,
        "q6_cohort_2025_03": q6,
    }


def write(ws):
    ws = Path(ws)
    (ws / "answers.json").write_text(json.dumps(answers(ws), ensure_ascii=False, indent=2), encoding="utf-8")
    (ws / "queries.sql").write_text("-- reference: see refcalc.py\nSELECT 1;\n", encoding="utf-8")
