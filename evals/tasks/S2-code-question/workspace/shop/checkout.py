"""Checkout: subtotal + shipping. Free shipping over NT$3000."""
from shipping.fees import shipping_fee

FREE_SHIPPING_OVER = 3000


def total(cart, zone: str, tier: str = "standard") -> int:
    subtotal = cart.subtotal()
    if subtotal >= FREE_SHIPPING_OVER:
        return subtotal
    return subtotal + shipping_fee(cart.weight(), zone, tier)
