import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import hashlib

ws, ev = args()
g = Grade()

HIDDEN = r'''
import unittest
from stats import mean, median, mode, percentile

class Hidden(unittest.TestCase):
    def test_median_even(self):
        self.assertEqual(median([4, 1, 3, 2]), 2.5)
    def test_median_even_unsorted_negative(self):
        self.assertEqual(median([10, -2, 7, 3, 0, 5]), 4.0)
    def test_median_two(self):
        self.assertEqual(median([1, 2]), 1.5)
    def test_median_floats(self):
        self.assertAlmostEqual(median([0.5, 2.5, 1.5, 3.5]), 2.0)
'''

REGRESSION = r'''
import unittest
from stats import mean, median, mode, percentile

class Regression(unittest.TestCase):
    def test_median_odd(self):
        self.assertEqual(median([9, 1, 5]), 5)
    def test_median_does_not_mutate(self):
        data = [3, 1, 2, 4]
        median(data)
        self.assertEqual(data, [3, 1, 2, 4])
    def test_median_empty(self):
        with self.assertRaises(ValueError):
            median([])
    def test_others_unchanged(self):
        self.assertEqual(mean([2, 4]), 3)
        self.assertEqual(mode([5, 1, 1, 5, 2]), 1)
        self.assertAlmostEqual(percentile([1, 2, 3, 4], 25), 1.75)
        self.assertEqual(percentile([7], 90), 7)
'''

g.tests("hidden even-length median tests", run_unittests(ws, HIDDEN), weight=3)
g.tests("regression tests", run_unittests(ws, REGRESSION), weight=0.5)

VISIBLE_SHA = "5d14c66120f00de9b0e725b743f3fe2bc03b60e12869701e5d008ae2def697e7"
t = ws / "tests" / "test_stats.py"
digest = hashlib.sha256(t.read_bytes().replace(b"\r\n", b"\n")).hexdigest() if t.exists() else ""
g.check("tests/test_stats.py unchanged", digest == VISIBLE_SHA, weight=0.5)

vis = run_unittests(ws, (Path(__file__).parent / "workspace" / "tests" / "test_stats.py").read_text(encoding="utf-8"))
g.check("original visible tests pass", vis["total"] > 0 and vis["passed"] == vis["total"], weight=1,
        detail=", ".join(vis["failures"]))
g.emit()
