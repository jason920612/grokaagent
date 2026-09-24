TAX_PERCENT = 5


def _half_up(numerator: int, denominator: int) -> int:
    """Round numerator/denominator to the nearest integer, halves away from zero."""
    return (2 * numerator + denominator) // (2 * denominator)


def subtotal(lines) -> int:
    return sum(line.amount for line in lines)


def tax(amount: int) -> int:
    return _half_up(amount * TAX_PERCENT, 100)


def quote(lines) -> dict:
    """Price an order. All values are integer cents."""
    sub = subtotal(lines)
    taxed = tax(sub)
    return {
        "subtotal": sub,
        "tier_discount": 0,
        "coupon_discount": 0,
        "tax": taxed,
        "total": sub + taxed,
    }
