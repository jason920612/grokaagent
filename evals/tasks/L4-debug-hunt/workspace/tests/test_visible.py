import unittest

from calc.cache import LRUCache
from calc.expr import EvalError, evaluate


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class CacheTest(unittest.TestCase):
    def test_basic_put_get(self):
        c = LRUCache(2)
        c.put("a", 1)
        self.assertEqual(c.get("a"), 1)
        self.assertIsNone(c.get("zz"))

    def test_evicts_least_recently_used(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        c.get("a")          # a is now the most recently used
        c.put("c", 3)       # so b must go
        self.assertEqual(c.get("a"), 1)
        self.assertIsNone(c.get("b"))

    def test_ttl_expires(self):
        clock = FakeClock()
        c = LRUCache(3, ttl=10, clock=clock)
        c.put("a", 1)
        clock.now = 11
        self.assertIsNone(c.get("a"))
        self.assertEqual(len(c), 0)


class ExprTest(unittest.TestCase):
    def test_precedence(self):
        self.assertEqual(evaluate("2+3*4"), 14)
        self.assertEqual(evaluate("(2+3)*4"), 20)

    def test_left_associative(self):
        self.assertEqual(evaluate("10-3-2"), 5)

    def test_syntax_error(self):
        with self.assertRaises(EvalError):
            evaluate("2+")


if __name__ == "__main__":
    unittest.main()
