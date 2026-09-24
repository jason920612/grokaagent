import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import subprocess

HIDDEN = r'''
import json, os, subprocess, sys, tempfile, unittest

WS = os.getcwd()
TODO = os.path.join(WS, "todo.py")


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.db = os.path.join(self.tmp.name, "db.json")

    def tearDown(self):
        self.tmp.cleanup()

    def cli(self, *args):
        return subprocess.run([sys.executable, TODO, "--db", self.db, *args], capture_output=True,
                              text=True, encoding="utf-8", timeout=30, cwd=self.tmp.name)

    def ok(self, *args):
        r = self.cli(*args)
        self.assertEqual(r.returncode, 0, f"{args}: {r.stderr}")
        return r.stdout.strip()

    def lines(self, *args):
        out = self.ok(*args)
        return out.splitlines() if out else []


class HiddenTodo(Base):
    def test_add_messages_and_ids(self):
        self.assertEqual(self.ok("add", "買牛奶"), "Added #1: 買牛奶")
        self.assertEqual(self.ok("add", "  繳電費  ", "--priority", "high"), "Added #2: 繳電費")

    def test_list_format_full(self):
        self.ok("add", "繳電費", "--priority", "high", "--due", "2026-10-05", "--tag", "bill", "--tag", "home", "--tag", "bill")
        self.ok("add", "買牛奶")
        self.assertEqual(self.lines("list"), ["#1 [ ] 繳電費 (high) due:2026-10-05 tags:bill,home", "#2 [ ] 買牛奶 (medium)"])

    def test_empty_list(self):
        self.assertEqual(self.ok("list"), "No tasks.")

    def test_done_hides_and_all_shows(self):
        self.ok("add", "a"); self.ok("add", "b")
        self.assertEqual(self.ok("done", "1"), "Completed #1")
        self.assertEqual(self.lines("list"), ["#2 [ ] b (medium)"])
        self.assertEqual(self.lines("list", "--all"), ["#1 [x] a (medium)", "#2 [ ] b (medium)"])

    def test_done_twice(self):
        self.ok("add", "a"); self.ok("done", "1")
        self.assertEqual(self.ok("done", "1"), "Task #1 already completed")

    def test_unknown_id_exit_1(self):
        for cmd in ("done", "remove"):
            r = self.cli(cmd, "42")
            self.assertEqual(r.returncode, 1, cmd)
            self.assertIn("error: task #42 not found", r.stderr)
            self.assertNotIn("Traceback", r.stderr)

    def test_ids_not_reused(self):
        self.ok("add", "a"); self.ok("add", "b")
        self.assertEqual(self.ok("remove", "2"), "Removed #2")
        self.assertEqual(self.ok("add", "c"), "Added #3: c")

    def test_validation_exit_2_and_no_write(self):
        for args in (["add", "   "], ["add", "x", "--priority", "urgent"], ["add", "x", "--due", "2026-02-30"]):
            r = self.cli(*args)
            self.assertEqual(r.returncode, 2, args)
            self.assertTrue(r.stderr.strip().lower().startswith("error") or "error:" in r.stderr, r.stderr)
            self.assertNotIn("Traceback", r.stderr)
        self.assertEqual(self.ok("list"), "No tasks.")

    def test_sort_priority(self):
        self.ok("add", "l", "--priority", "low"); self.ok("add", "h", "--priority", "high")
        self.ok("add", "m"); self.ok("add", "h2", "--priority", "high")
        ids = [l.split()[0] for l in self.lines("list", "--sort", "priority")]
        self.assertEqual(ids, ["#2", "#4", "#3", "#1"])

    def test_sort_due_none_last(self):
        self.ok("add", "a"); self.ok("add", "b", "--due", "2026-12-01")
        self.ok("add", "c", "--due", "2026-01-15"); self.ok("add", "d", "--due", "2026-12-01")
        ids = [l.split()[0] for l in self.lines("list", "--sort", "due")]
        self.assertEqual(ids, ["#3", "#2", "#4", "#1"])

    def test_filters_combine(self):
        self.ok("add", "a", "--priority", "high", "--tag", "work")
        self.ok("add", "b", "--priority", "high", "--tag", "home")
        self.ok("add", "c", "--tag", "work")
        self.assertEqual([l.split()[0] for l in self.lines("list", "--priority", "high", "--tag", "work")], ["#1"])

    def test_overdue_with_today(self):
        self.ok("add", "old", "--due", "2026-01-01"); self.ok("add", "new", "--due", "2026-12-31")
        self.ok("add", "nodue"); self.ok("add", "olddone", "--due", "2025-01-01"); self.ok("done", "4")
        self.assertEqual([l.split()[0] for l in self.lines("list", "--overdue", "--today", "2026-06-01")], ["#1"])

    def test_edit_fields(self):
        self.ok("add", "a", "--due", "2026-03-01", "--tag", "x")
        self.assertEqual(self.ok("edit", "1", "--title", "b", "--priority", "low", "--add-tag", "y", "--remove-tag", "x"), "Updated #1")
        self.assertEqual(self.lines("list"), ["#1 [ ] b (low) due:2026-03-01 tags:y"])
        self.ok("edit", "1", "--due", "none")
        self.assertEqual(self.lines("list"), ["#1 [ ] b (low) tags:y"])

    def test_edit_errors(self):
        self.ok("add", "a")
        self.assertEqual(self.cli("edit", "1").returncode, 2)
        self.assertEqual(self.cli("edit", "1", "--priority", "huge").returncode, 2)
        self.assertEqual(self.cli("edit", "7", "--title", "z").returncode, 1)

    def test_stats(self):
        self.ok("add", "a", "--priority", "high", "--due", "2026-01-01")
        self.ok("add", "b", "--priority", "low")
        self.ok("add", "c", "--priority", "high", "--due", "2026-01-01"); self.ok("done", "3")
        self.ok("add", "d", "--due", "2027-01-01")
        self.assertEqual(self.lines("stats", "--today", "2026-06-01"),
                         ["total: 4", "open: 3", "done: 1", "overdue: 1", "by priority: high=1 medium=1 low=1"])

    def test_persistence_schema(self):
        self.ok("add", "a", "--tag", "t"); self.ok("add", "b"); self.ok("remove", "1")
        with open(self.db, encoding="utf-8") as fh:
            data = json.load(fh)
        self.assertEqual(data["next_id"], 3)
        self.assertEqual([t["id"] for t in data["tasks"]], [2])
        t = data["tasks"][0]
        self.assertEqual((t["title"], t["priority"], t["due"], t["tags"], t["done"]), ("b", "medium", None, [], False))

    def test_corrupt_db(self):
        with open(self.db, "w", encoding="utf-8") as fh:
            fh.write("{not json")
        r = self.cli("add", "x")
        self.assertEqual(r.returncode, 1)
        self.assertIn("error", r.stderr)
        self.assertNotIn("Traceback", r.stderr)
        with open(self.db, encoding="utf-8") as fh:
            self.assertEqual(fh.read(), "{not json")

    def test_default_db_next_to_script(self):
        import shutil
        d = tempfile.mkdtemp()
        try:
            shutil.copy(TODO, os.path.join(d, "todo.py"))
            r = subprocess.run([sys.executable, os.path.join(d, "todo.py"), "add", "x"], capture_output=True,
                               text=True, encoding="utf-8", timeout=30, cwd=self.tmp.name)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertTrue(os.path.exists(os.path.join(d, "todo.json")))
        finally:
            shutil.rmtree(d, ignore_errors=True)
'''

ws, ev = args()
g = Grade()
g.tests("hidden CLI tests", run_unittests(ws, HIDDEN, timeout=300), weight=9.0)

own = [p for p in ws.glob("test*.py")] + [p for p in ws.glob("*_test.py")] + [p for p in ws.glob("tests/test*.py")]
own = [p for p in own if "todo" in (p.read_text(encoding="utf-8", errors="replace"))]
g.check("agent wrote its own test file", bool(own), weight=0.5, detail=", ".join(p.name for p in own))
passed = False
detail = ""
if own:
    try:
        r = subprocess.run([sys.executable, "-m", "unittest", *[str(p.relative_to(ws)).replace("\\", "/")[:-3].replace("/", ".") for p in own]],
                           cwd=str(ws), capture_output=True, text=True, encoding="utf-8", timeout=240)
        passed = r.returncode == 0 and "Ran 0 tests" not in r.stderr
        detail = r.stderr[-300:]
    except subprocess.TimeoutExpired:
        detail = "timeout"
g.check("agent's own tests pass", passed, weight=0.5, detail=detail)
g.emit()
