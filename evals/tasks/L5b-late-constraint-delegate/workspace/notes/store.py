"""JSON-file storage for notes.

A note is a dict: {"id": int, "title": str, "body": str, "created": str}.
Ids grow from 1 and are never reused, even after a delete.
"""
import datetime
import json
import os


class NoteStore:
    def __init__(self, path):
        self.path = path
        self._data = {"next_id": 1, "notes": []}
        if os.path.exists(path):
            with open(path, encoding="utf-8") as fh:
                self._data = json.load(fh)

    def _save(self):
        tmp = self.path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(self._data, fh, ensure_ascii=False, indent=2)
        os.replace(tmp, self.path)

    def add(self, title, body):
        note = {
            "id": self._data["next_id"],
            "title": title,
            "body": body,
            "created": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        }
        self._data["next_id"] += 1
        self._data["notes"].append(note)
        self._save()
        return note["id"]

    def get(self, note_id):
        for note in self._data["notes"]:
            if note["id"] == note_id:
                return note
        return None

    def all(self):
        return list(self._data["notes"])

    def delete(self, note_id):
        before = len(self._data["notes"])
        self._data["notes"] = [n for n in self._data["notes"] if n["id"] != note_id]
        if len(self._data["notes"]) == before:
            return False
        self._save()
        return True
