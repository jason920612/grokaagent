import re

TAX_PERCENT = 5
_COUPON = re.compile(r"(PCT|OFF)([1-9][0-9]*)")


def _half_up(numerator: int, denominator: int) -> int:
    """Round numerator/denominator to the nearest integer, halves away from zero."""
    return (2 * numerator + denominator) // (2 * denominator)


def subtotal(lines) -> int:
    return sum(line.amount for line in lines)


def tax(amount: int) -> int:
    return _half_up(amount * TAX_PERCENT, 100)


def tier_percent(qty: int) -> int:
    if qty >= 50:
        return 10
    if qty >= 10:
        return 5
    return 0


def parse_coupons(coupons):
    percents, fixed = [], []
    for code in coupons:
        m = _COUPON.fullmatch(code) if isinstance(code, str) else None
        if not m:
            raise ValueError(f"invalid coupon: {code!r}")
        kind, n = m.group(1), int(m.group(2))
        if kind == "PCT":
            if not 1 <= n <= 50:
                raise ValueError(f"invalid coupon: {code!r}")
            percents.append(n)
        else:
            fixed.append(n)
    return percents, fixed


def quote(lines, coupons=()) -> dict:
    """Price an order. All values are integer cents."""
    percents, fixed = parse_coupons(coupons)
    sub = subtotal(lines)
    tier = sum(_half_up(line.amount * tier_percent(line.qty), 100) for line in lines)
    base = sub - tier
    amount = base
    coupon = 0
    if percents:
        off = _half_up(amount * max(percents), 100)
        amount -= off
        coupon += off
    for value in fixed:
        if base < 2 * value:
            continue
        off = min(value, amount)
        amount -= off
        coupon += off
    taxed = tax(amount)
    return {
        "subtotal": sub,
        "tier_discount": tier,
        "coupon_discount": coupon,
        "tax": taxed,
        "total": sub - tier - coupon + taxed,
    }
