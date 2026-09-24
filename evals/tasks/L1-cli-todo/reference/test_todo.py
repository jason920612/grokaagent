import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


class TodoTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.db = os.path.join(self.dir.name, "db.json")

    def tearDown(self):
        self.dir.cleanup()

    def run_cli(self, *args):
        return subprocess.run([sys.executable, os.path.join(HERE, "todo.py"), "--db", self.db, *args],
                              capture_output=True, text=True, encoding="utf-8")

    def test_add_and_list(self):
        self.assertEqual(self.run_cli("add", "milk").stdout.strip(), "Added #1: milk")
        self.assertEqual(self.run_cli("list").stdout.strip(), "#1 [ ] milk (medium)")

    def test_unknown_id(self):
        r = self.run_cli("done", "9")
        self.assertEqual(r.returncode, 1)
        self.assertIn("error", r.stderr)


if __name__ == "__main__":
    unittest.main()
