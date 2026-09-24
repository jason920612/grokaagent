import unittest

from shop.models import Line, Product
from shop.pricing import quote

PEN = Product("PEN", "Pen", 150)
BOOK = Product("BOOK", "Book", 1299)


class PricingTest(unittest.TestCase):
    def test_simple_quote(self):
        q = quote([Line(PEN, 2), Line(BOOK, 1)])
        self.assertEqual(q["subtotal"], 1599)
        self.assertEqual(q["tax"], 80)
        self.assertEqual(q["total"], 1679)

    def test_tax_rounds_half_up(self):
        # 5% of 1010 is 50.5 -> 51
        q = quote([Line(Product("X", "x", 1010), 1)])
        self.assertEqual(q["tax"], 51)


if __name__ == "__main__":
    unittest.main()
