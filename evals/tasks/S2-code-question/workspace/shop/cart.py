from dataclasses import dataclass, field


@dataclass
class Item:
    sku: str
    price: int
    weight_kg: float
    qty: int = 1


@dataclass
class Cart:
    items: list = field(default_factory=list)

    def weight(self) -> float:
        return sum(i.weight_kg * i.qty for i in self.items)

    def subtotal(self) -> int:
        return sum(i.price * i.qty for i in self.items)
