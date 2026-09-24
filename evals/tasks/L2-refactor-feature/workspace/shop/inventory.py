class OutOfStock(Exception):
    pass


class Inventory:
    """Stock on hand plus reservations held by open orders."""

    def __init__(self):
        self._stock = {}
        self._reserved = {}

    def add_stock(self, sku: str, qty: int) -> None:
        if qty <= 0:
            raise ValueError("qty must be positive")
        self._stock[sku] = self._stock.get(sku, 0) + qty

    def on_hand(self, sku: str) -> int:
        return self._stock.get(sku, 0)

    def reserved(self, sku: str) -> int:
        return self._reserved.get(sku, 0)

    def available(self, sku: str) -> int:
        return self.on_hand(sku) - self.reserved(sku)

    def reserve(self, sku: str, qty: int) -> None:
        if qty <= 0:
            raise ValueError("qty must be positive")
        if qty > self.on_hand(sku):
            raise OutOfStock(f"{sku}: want {qty}, available {self.available(sku)}")
        self._reserved[sku] = self.reserved(sku) + qty

    def release(self, sku: str, qty: int) -> None:
        if qty <= 0 or qty > self.reserved(sku):
            raise ValueError("cannot release more than reserved")
        self._reserved[sku] = self.reserved(sku) - qty

    def fulfil(self, sku: str, qty: int) -> None:
        """Ship reserved units: they leave both the reservation and the stock."""
        self.release(sku, qty)
        self._stock[sku] -= qty
