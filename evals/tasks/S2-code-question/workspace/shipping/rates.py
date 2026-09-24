"""Weight-based base rates (NT$)."""
import math

FIRST_KG_FEE = 60        # covers the first kilogram
PER_HALF_KG = 18         # every started 0.5 kg after the first kilogram
MIN_FEE = 80


def billable_weight(kg: float) -> float:
    """Round up to the next 0.5 kg; anything under 1 kg bills as 1 kg."""
    if kg <= 0:
        raise ValueError("weight must be positive")
    return max(1.0, math.ceil(kg * 2) / 2)


def base_rate(kg: float) -> float:
    billable = billable_weight(kg)
    extra_steps = (billable - 1.0) / 0.5
    return FIRST_KG_FEE + extra_steps * PER_HALF_KG
