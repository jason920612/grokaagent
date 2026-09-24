"""The fee the checkout actually charges."""
from decimal import Decimal, ROUND_HALF_UP

from .rates import MIN_FEE, base_rate
from .zones import multiplier

MEMBER_DISCOUNT = {"standard": 0.0, "silver": 0.05, "gold": 0.10}
DISCOUNT_CAP = 50


def shipping_fee(weight_kg: float, zone: str, tier: str = "standard") -> int:
    fee = base_rate(weight_kg) * multiplier(zone)
    discount = min(fee * MEMBER_DISCOUNT.get(tier, 0.0), DISCOUNT_CAP)
    fee = max(fee - discount, MIN_FEE)
    return int(Decimal(str(fee)).quantize(Decimal("1"), rounding=ROUND_HALF_UP))
