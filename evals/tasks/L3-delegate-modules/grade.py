import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *

HIDDEN = r'''
import importlib, os, subprocess, sys, unittest

WS = os.getcwd()


def mod(name):
    return importlib.import_module(name)


def cli(*args):
    env = dict(os.environ, PYTHONIOENCODING="utf-8")
    return subprocess.run([sys.executable, os.path.join(WS, "main.py"), *args], capture_output=True,
                          text=True, encoding="utf-8", timeout=30, cwd=WS, env=env)


class Roman(unittest.TestCase):
    def test_to_roman_examples(self):
        r = mod("roman")
        for n, s in [(1, "I"), (4, "IV"), (9, "IX"), (14, "XIV"), (40, "XL"), (90, "XC"), (400, "CD"),
                     (1994, "MCMXCIV"), (2024, "MMXXIV"), (3999, "MMMCMXCIX")]:
            self.assertEqual(r.to_roman(n), s)

    def test_to_roman_range(self):
        r = mod("roman")
        for bad in (0, -1, 4000, 2.0, "5", True):
            with self.assertRaises(ValueError, msg=repr(bad)):
                r.to_roman(bad)

    def test_round_trip_all(self):
        r = mod("roman")
        for n in range(1, 4000):
            self.assertEqual(r.from_roman(r.to_roman(n)), n)

    def test_from_roman_rejects_non_canonical(self):
        r = mod("roman")
        for bad in ("IIII", "VX", "IC", "MMMM", "IIV", "iv", "", "XIIII", "VV", "IM", "MCMC", "A"):
            with self.assertRaises(ValueError, msg=bad):
                r.from_roman(bad)


class Luhn(unittest.TestCase):
    def test_valid_numbers(self):
        l = mod("luhn")
        for s in ("79927398713", "4539 3195 0343 6467", "059", "18"):
            self.assertTrue(l.is_valid(s), s)

    def test_invalid_numbers(self):
        l = mod("luhn")
        for s in ("79927398710", "8273 1232 7352 0569", "0", " 0", "", "055a444285", "059-"):
            self.assertFalse(l.is_valid(s), s)

    def test_check_digit_and_complete(self):
        l = mod("luhn")
        self.assertEqual(l.check_digit("7992739871"), "3")
        self.assertEqual(l.complete("7992739871"), "79927398713")
        self.assertEqual(l.complete("4539 3195 0343 646"), "4539319503436467")
        self.assertEqual(l.check_digit("0"), "0")
        for p in ("1", "12", "999", "1234567", "00000000001"):
            self.assertTrue(l.is_valid(l.complete(p)), p)

    def test_check_digit_rejects(self):
        l = mod("luhn")
        for bad in ("", "  ", "12a", "1-2"):
            with self.assertRaises(ValueError, msg=bad):
                l.check_digit(bad)


class Rle(unittest.TestCase):
    def test_encode_examples(self):
        r = mod("rle")
        self.assertEqual(r.encode("aaab"), "3a1b")
        self.assertEqual(r.encode(""), "")
        self.assertEqual(r.encode("x" * 12), "12x")
        self.assertEqual(r.encode("33"), "2\\3")
        self.assertEqual(r.encode("a\\"), "1a1\\\\")

    def test_decode_examples(self):
        r = mod("rle")
        self.assertEqual(r.decode("3a1b"), "aaab")
        self.assertEqual(r.decode("12x"), "x" * 12)
        self.assertEqual(r.decode("2\\3"), "33")
        self.assertEqual(r.decode(""), "")

    def test_round_trip(self):
        r = mod("rle")
        for s in ("hello  world\n\n", "1112223334", "\\\\\\n", "中文中文中", "a1b2c3\\x", "0", "9" * 25, " \t"):
            self.assertEqual(r.decode(r.encode(s)), s, s)

    def test_decode_errors(self):
        r = mod("rle")
        for bad in ("a", "0a", "03a", "3a2", "2\\", "23", "\\3"):
            with self.assertRaises(ValueError, msg=bad):
                r.decode(bad)


class Cli(unittest.TestCase):
    def test_roman(self):
        self.assertEqual(cli("roman", "to", "1994").stdout.strip(), "MCMXCIV")
        self.assertEqual(cli("roman", "from", "MMXXIV").stdout.strip(), "2024")

    def test_luhn(self):
        self.assertEqual(cli("luhn", "check", "79927398713").stdout.strip(), "valid")
        self.assertEqual(cli("luhn", "check", "79927398710").stdout.strip(), "invalid")
        self.assertEqual(cli("luhn", "complete", "7992739871").stdout.strip(), "79927398713")

    def test_rle(self):
        self.assertEqual(cli("rle", "encode", "aaab").stdout.strip(), "3a1b")
        self.assertEqual(cli("rle", "decode", "3a1b").stdout.strip(), "aaab")

    def test_errors(self):
        for args in (["roman", "to", "4000"], ["roman", "from", "IIII"], ["rle", "decode", "a"],
                     ["luhn", "complete", "12a"], ["nope", "x", "y"], ["roman", "to", "abc"]):
            r = cli(*args)
            self.assertEqual(r.returncode, 1, args)
            self.assertTrue(r.stderr.startswith("error: "), (args, r.stderr))
            self.assertNotIn("Traceback", r.stderr)
'''

ws, ev = args()
g = Grade()
g.tests("hidden module + CLI tests", run_unittests(ws, HIDDEN, timeout=240), weight=7.0)

evs = events(ev)
spawned = [e for e in evs if e.get("type") == "child_spawned"]
waited = [e for e in evs if e.get("type") == "tool_finished" and e.get("name") == "wait_agents"
          and e.get("agent_name") == "root" and not e.get("agent_path")]
g.check("delegated: >= 2 child agents spawned", len(spawned) >= 2, weight=1.5,
        detail=", ".join(e.get("name", "?") for e in spawned))
g.check("root waited for children (wait_agents)", bool(waited), weight=1.5, detail=f"{len(waited)} call(s)")
g.emit()
