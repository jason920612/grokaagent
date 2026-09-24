from dataclasses import dataclass


@dataclass(frozen=True)
class Product:
    sku: str
    name: str
    price: int  # cents


@dataclass(frozen=True)
class Line:
    product: Product
    qty: int

    def __post_init__(self):
        if self.qty <= 0:
            raise ValueError("qty must be positive")

    @property
    def amount(self) -> int:
        return self.product.price * self.qty
