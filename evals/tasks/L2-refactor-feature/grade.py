import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import hashlib
import subprocess

VISIBLE = {
    "tests/test_inventory.py": "d8e5c16e9728420dc3e8ad2c2aedf36c8ebe3d8fcecfa9be423bde3e251ab802",
    "tests/test_pricing.py": "4b6e47b0b3bbbc9b487bbd8cecbb4d0282f8c84aac0e073ce8aeb85bbc4d9c19",
}

HIDDEN = r'''
import unittest

from shop.inventory import Inventory, OutOfStock
from shop.models import Line, Product
from shop.orders import cancel_order, place_order
from shop.pricing import quote

PEN = Product("PEN", "Pen", 150)
BOOK = Product("BOOK", "Book", 1299)


def P(price):
    return Product("X%d" % price, "x", price)


class Bug1Oversell(unittest.TestCase):
    def test_second_reservation_uses_available(self):
        inv = Inventory(); inv.add_stock("PEN", 10)
        inv.reserve("PEN", 6)
        with self.assertRaises(OutOfStock):
            inv.reserve("PEN", 6)
        self.assertEqual(inv.reserved("PEN"), 6)
        inv.reserve("PEN", 4)
        self.assertEqual(inv.available("PEN"), 0)

    def test_orders_cannot_oversell(self):
        inv = Inventory(); inv.add_stock("PEN", 10); inv.add_stock("BOOK", 1)
        place_order(inv, [Line(PEN, 6)])
        with self.assertRaises(OutOfStock):
            place_order(inv, [Line(BOOK, 1), Line(PEN, 6)])
        self.assertEqual(inv.available("PEN"), 4)
        self.assertEqual(inv.reserved("BOOK"), 0)


class Bug2Cancel(unittest.TestCase):
    def test_cancel_releases_all_units(self):
        inv = Inventory(); inv.add_stock("BOOK", 8); inv.add_stock("PEN", 20)
        order = place_order(inv, [Line(BOOK, 5), Line(PEN, 12)])
        cancel_order(inv, order)
        self.assertEqual(inv.reserved("BOOK"), 0)
        self.assertEqual(inv.reserved("PEN"), 0)
        self.assertEqual(inv.available("PEN"), 20)
        self.assertEqual(order.status, "cancelled")


class Tiers(unittest.TestCase):
    def test_tier_5_percent(self):
        q = quote([Line(PEN, 10)])
        self.assertEqual((q["subtotal"], q["tier_discount"], q["tax"], q["total"]), (1500, 75, 71, 1496))

    def test_tier_10_percent_and_per_line(self):
        q = quote([Line(PEN, 50), Line(P(7350 // 49), 49)])
        # 7500 * 10% = 750 ; 7350 * 5% = 367.5 -> 368
        self.assertEqual(q["tier_discount"], 750 + 368)

    def test_tier_half_up(self):
        q = quote([Line(P(7), 10)])
        self.assertEqual(q["tier_discount"], 4)

    def test_tier_not_aggregated_across_lines(self):
        q = quote([Line(PEN, 6), Line(PEN, 6)])
        self.assertEqual(q["tier_discount"], 0)
        self.assertEqual(q["total"], 1800 + 90)


class Coupons(unittest.TestCase):
    def test_single_percent(self):
        q = quote([Line(BOOK, 1)], coupons=["PCT10"])
        self.assertEqual((q["coupon_discount"], q["tax"], q["total"]), (130, 58, 1227))

    def test_only_best_percent(self):
        q = quote([Line(BOOK, 1)], coupons=["PCT10", "PCT20"])
        self.assertEqual(q["coupon_discount"], 260)

    def test_percent_boundary_50(self):
        q = quote([Line(P(1000), 1)], coupons=["PCT50"])
        self.assertEqual((q["coupon_discount"], q["total"]), (500, 525))

    def test_fixed_min_spend(self):
        q = quote([Line(BOOK, 1)], coupons=["OFF500", "OFF700"])
        self.assertEqual((q["coupon_discount"], q["tax"], q["total"]), (500, 40, 839))

    def test_fixed_stack_floor_zero(self):
        q = quote([Line(P(1000), 1)], coupons=["OFF500", "OFF500", "OFF500"])
        self.assertEqual((q["coupon_discount"], q["tax"], q["total"]), (1000, 0, 0))

    def test_percent_before_fixed(self):
        q = quote([Line(P(2000), 1)], coupons=["OFF500", "PCT10"])
        self.assertEqual((q["coupon_discount"], q["tax"], q["total"]), (700, 65, 1365))

    def test_min_spend_after_tier(self):
        q = quote([Line(P(100), 10)], coupons=["OFF500"])
        self.assertEqual((q["tier_discount"], q["coupon_discount"], q["tax"], q["total"]), (50, 0, 48, 998))

    def test_tier_then_percent(self):
        q = quote([Line(PEN, 10)], coupons=["PCT10"])
        # base 1425, 10% = 142.5 -> 143, amount 1282, tax 64.1 -> 64
        self.assertEqual((q["tier_discount"], q["coupon_discount"], q["tax"], q["total"]), (75, 143, 64, 1346))

    def test_invalid_codes(self):
        for code in ["PCT0", "PCT51", "OFF0", "OFF-5", "pct10", "FOO", "PCT10.5", "OFF"]:
            with self.assertRaises(ValueError, msg=code):
                quote([Line(BOOK, 1)], coupons=[code])

    def test_no_coupons_default(self):
        q = quote([Line(BOOK, 1)])
        self.assertEqual((q["coupon_discount"], q["total"]), (0, 1364))


class OrdersWithCoupons(unittest.TestCase):
    def test_coupons_reach_quote(self):
        inv = Inventory(); inv.add_stock("BOOK", 3)
        order = place_order(inv, [Line(BOOK, 1)], coupons=["PCT10"])
        self.assertEqual(order.prices["coupon_discount"], 130)
        self.assertEqual(inv.reserved("BOOK"), 1)

    def test_bad_coupon_leaves_no_reservation(self):
        inv = Inventory(); inv.add_stock("PEN", 10)
        with self.assertRaises(ValueError):
            place_order(inv, [Line(PEN, 2)], coupons=["BAD"])
        self.assertEqual(inv.reserved("PEN"), 0)
'''

ws, ev = args()
g = Grade()
g.tests("hidden bug + feature tests", run_unittests(ws, HIDDEN, timeout=180), weight=8.0)

ok, detail = False, ""
try:
    r = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-t", "."], cwd=str(ws),
                       capture_output=True, text=True, encoding="utf-8", timeout=180)
    ok = r.returncode == 0
    detail = r.stderr[-300:]
except (subprocess.TimeoutExpired, OSError) as e:
    detail = str(e)
g.check("visible tests pass", ok, weight=1.0, detail=detail)

changed = []
for rel, digest in VISIBLE.items():
    p = ws / rel
    try:
        got = hashlib.sha256(p.read_bytes().replace(b"\r\n", b"\n")).hexdigest()
    except OSError:
        got = "missing"
    if got != digest:
        changed.append(rel)
g.check("visible test files unchanged", not changed, weight=1.0, detail=", ".join(changed))
g.emit()
