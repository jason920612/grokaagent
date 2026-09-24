import itertools

from .inventory import OutOfStock
from .pricing import quote

_ids = itertools.count(1)


class Order:
    def __init__(self, lines, prices):
        self.id = next(_ids)
        self.lines = list(lines)
        self.prices = prices
        self.status = "open"


def place_order(inventory, lines, **pricing_options) -> Order:
    """Price the order, then reserve stock for every line, or nothing at all."""
    prices = quote(lines, **pricing_options)
    done = []
    try:
        for line in lines:
            inventory.reserve(line.product.sku, line.qty)
            done.append(line)
    except OutOfStock:
        for line in done:
            inventory.release(line.product.sku, line.qty)
        raise
    return Order(lines, prices)


def cancel_order(inventory, order: Order) -> None:
    if order.status != "open":
        raise ValueError(f"order {order.id} is {order.status}")
    for line in order.lines:
        inventory.release(line.product.sku, line.qty)
    order.status = "cancelled"


def ship_order(inventory, order: Order) -> None:
    if order.status != "open":
        raise ValueError(f"order {order.id} is {order.status}")
    for line in order.lines:
        inventory.fulfil(line.product.sku, line.qty)
    order.status = "shipped"
