import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import subprocess

HIDDEN = r'''
import unittest

from calc.cache import LRUCache
from calc.expr import EvalError, evaluate


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class BugA_RecencyOnGet(unittest.TestCase):
    def test_get_refreshes_recency(self):
        c = LRUCache(3)
        for k in "abc":
            c.put(k, k)
        c.get("a"); c.get("b")
        c.put("d", "d")
        self.assertIsNone(c.get("c"))
        self.assertEqual([c.get(k) for k in "abd"], ["a", "b", "d"])

    def test_contains_counts_as_use(self):
        c = LRUCache(2)
        c.put("a", 1); c.put("b", 2)
        self.assertIn("a", c)
        c.put("c", 3)
        self.assertEqual(c.get("a"), 1)
        self.assertIsNone(c.get("b"))


class BugB_OverwriteWhenFull(unittest.TestCase):
    def test_overwrite_does_not_evict(self):
        c = LRUCache(2)
        c.put("a", 1); c.put("b", 2)
        c.put("b", 20)         # full, but b already exists: nothing is evicted
        self.assertEqual(len(c), 2)
        self.assertEqual((c.get("a"), c.get("b")), (1, 20))

    def test_overwrite_refreshes_order_and_ttl(self):
        clock = Clock()
        c = LRUCache(3, ttl=10, clock=clock)
        c.put("a", 1); c.put("b", 2); c.put("c", 3)
        clock.now = 5
        c.put("b", 20)         # b becomes most recent and gets a fresh ttl
        self.assertEqual(c.get("a"), 1)
        c.put("d", 4)          # evicts c (a was just read, b rewritten)
        self.assertIsNone(c.get("c"))
        clock.now = 12
        self.assertEqual(c.get("b"), 20)
        self.assertIsNone(c.get("a"))


class BugC_LeftAssociative(unittest.TestCase):
    def test_subtraction_chain(self):
        self.assertEqual(evaluate("10-3-2"), 5)
        self.assertEqual(evaluate("1-2+3"), 2)
        self.assertEqual(evaluate("100 - 10 - 1 - (2 - 1)"), 88)

    def test_mixed_chain(self):
        self.assertEqual(evaluate("2*3-4-5+6"), 3)
        self.assertEqual(evaluate("8/4/2"), 1.0)


class BugD_DivisionByZero(unittest.TestCase):
    def test_divide_by_zero_is_eval_error(self):
        for text in ("1/0", "5/(2-2)", "1/0.0", "3/(1-1)*2"):
            with self.assertRaises(EvalError, msg=text):
                evaluate(text)
        self.assertEqual(evaluate("7/2"), 3.5)
        self.assertEqual(evaluate("0/5"), 0.0)

    def test_zero_division_never_leaks(self):
        try:
            evaluate("(1+2)/(3-3)")
        except EvalError:
            pass
        except Exception as e:  # e.g. ZeroDivisionError
            self.fail(f"leaked {type(e).__name__}")
        else:
            self.fail("no error raised")


class Regression(unittest.TestCase):
    def test_ttl_boundary_inclusive(self):
        clock = Clock()
        c = LRUCache(2, ttl=10, clock=clock)
        c.put("a", 1)
        clock.now = 9.999
        self.assertEqual(c.get("a"), 1)
        clock.now = 10
        self.assertIsNone(c.get("a"))

    def test_unary_and_decimals(self):
        self.assertEqual(evaluate("2*-3"), -6)
        self.assertEqual(evaluate("--1"), 1)
        self.assertEqual(evaluate(".5+2."), 2.5)
'''

ORIGINAL_TESTS = ["test_basic_put_get", "test_evicts_least_recently_used", "test_ttl_expires",
                  "test_precedence", "test_left_associative", "test_syntax_error"]
KEY_ASSERTS = ['evaluate("10-3-2"), 5', 'self.assertIsNone(c.get("b"))', 'evaluate("(2+3)*4"), 20']

ws, ev = args()
g = Grade()
g.tests("hidden tests for all four bugs", run_unittests(ws, HIDDEN, timeout=120), weight=8.0)

ok, detail = False, ""
try:
    r = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-t", "."], cwd=str(ws),
                       capture_output=True, text=True, encoding="utf-8", timeout=120)
    ok = r.returncode == 0
    detail = r.stderr[-300:]
except (subprocess.TimeoutExpired, OSError) as e:
    detail = str(e)
g.check("visible tests pass", ok, weight=1.0, detail=detail)

src = read(ws, "tests/test_visible.py") or ""
missing = [n for n in ORIGINAL_TESTS if f"def {n}(" not in src] + [a for a in KEY_ASSERTS if a not in src]
g.check("visible tests not deleted or weakened", not missing, weight=1.0, detail=", ".join(missing))
g.emit()
