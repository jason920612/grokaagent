"""Reference amortization per the task's stated rules (shared by grader and reference_fix)."""
from decimal import ROUND_HALF_UP, Decimal

CENT = Decimal("0.01")


def q(x: Decimal) -> Decimal:
    return x.quantize(CENT, rounding=ROUND_HALF_UP)


def payment(p: Decimal, r: Decimal, n: int) -> Decimal:
    return q(p * r / (1 - (1 + r) ** (-n)))


def schedule():
    principal = Decimal("350000.00")
    r1 = Decimal("0.06") / 12
    r2 = Decimal("0.0725") / 12
    pay1 = payment(principal, r1, 360)
    pay2 = None
    bal = principal
    rows = []
    month = 0
    pay = pay1
    while bal > 0:
        month += 1
        r = r1 if month <= 60 else r2
        if month == 61:
            pay2 = payment(bal, r2, 300)
            pay = pay2
        interest = q(bal * r)
        extra = Decimal("20000.00") if month == 24 else Decimal("0.00")
        if bal + interest <= pay or month == 360:
            p = bal + interest
            prin = bal
            extra = Decimal("0.00")  # nothing left to prepay
        else:
            p = pay
            prin = p - interest
        new_bal = bal - prin - extra
        rows.append((month, p, interest, prin, extra, new_bal))
        bal = new_bal
    total_interest = sum(r[2] for r in rows)
    summary = {
        "total_interest": float(total_interest),
        "payoff_month": rows[-1][0],
        "payment_before_rate_change": float(pay1),
        "payment_after_rate_change": float(pay2),
    }
    return rows, summary


def write(ws):
    import json
    from pathlib import Path

    rows, summary = schedule()
    ws = Path(ws)
    lines = ["month,payment,interest,principal,extra,balance"]
    for m, p, i, pr, e, b in rows:
        lines.append(f"{m},{p:.2f},{i:.2f},{pr:.2f},{e:.2f},{b:.2f}")
    (ws / "schedule.csv").write_text("\n".join(lines) + "\n", encoding="utf-8")
    (ws / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
