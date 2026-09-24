import unittest

from shop.inventory import Inventory, OutOfStock
from shop.models import Line, Product
from shop.orders import place_order, ship_order

PEN = Product("PEN", "Pen", 150)


class InventoryTest(unittest.TestCase):
    def setUp(self):
        self.inv = Inventory()
        self.inv.add_stock("PEN", 10)

    def test_reserve_reduces_available(self):
        self.inv.reserve("PEN", 4)
        self.assertEqual(self.inv.available("PEN"), 6)
        self.assertEqual(self.inv.on_hand("PEN"), 10)

    def test_reserve_too_many(self):
        with self.assertRaises(OutOfStock):
            self.inv.reserve("PEN", 11)

    def test_ship_removes_stock(self):
        order = place_order(self.inv, [Line(PEN, 3)])
        ship_order(self.inv, order)
        self.assertEqual(self.inv.on_hand("PEN"), 7)
        self.assertEqual(self.inv.reserved("PEN"), 0)
        self.assertEqual(order.status, "shipped")


if __name__ == "__main__":
    unittest.main()
