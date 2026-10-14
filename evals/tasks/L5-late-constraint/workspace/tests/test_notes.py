import os
import tempfile
import unittest

import legacy_api
from notes.store import NoteStore


class StoreTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.db = os.path.join(self.dir.name, "n.json")

    def tearDown(self):
        self.dir.cleanup()

    def test_add_get_delete(self):
        s = NoteStore(self.db)
        a = s.add("a", "body a")
        b = s.add("b", "body b")
        self.assertEqual((a, b), (1, 2))
        self.assertEqual(s.get(1)["title"], "a")
        self.assertTrue(s.delete(1))
        self.assertFalse(s.delete(1))
        self.assertEqual(NoteStore(self.db).add("c", "x"), 3, "ids are never reused")

    def test_legacy_api_contract(self):
        s = NoteStore(self.db)
        s.add("first", "1")
        s.add("second", "2")
        self.assertIsNone(legacy_api.get_note(s, 99))
        self.assertEqual([n["title"] for n in legacy_api.list_notes(s)], ["first", "second"])
        self.assertEqual(legacy_api.format_note(s.get(1)), "#1 first")


if __name__ == "__main__":
    unittest.main()
