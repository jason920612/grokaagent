"""Command line: python -m notes.cli --db FILE <command> ..."""
import argparse
import sys

import legacy_api
from notes.store import NoteStore


def build_parser():
    p = argparse.ArgumentParser(prog="notes")
    p.add_argument("--db", default="notes.json")
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("add")
    a.add_argument("title")
    a.add_argument("body")
    sub.add_parser("list")
    s = sub.add_parser("show")
    s.add_argument("id", type=int)
    d = sub.add_parser("delete")
    d.add_argument("id", type=int)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    store = NoteStore(args.db)
    if args.cmd == "add":
        note_id = store.add(args.title, args.body)
        print(f"已新增筆記 #{note_id}")
        return 0
    if args.cmd == "list":
        notes = legacy_api.list_notes(store)
        if not notes:
            print("目前沒有筆記")
        for note in notes:
            print(legacy_api.format_note(note))
        return 0
    if args.cmd == "show":
        note = legacy_api.get_note(store, args.id)
        if note is None:
            print(f"找不到筆記 #{args.id}", file=sys.stderr)
            return 1
        print(f"#{note['id']} {note['title']}\n\n{note['body']}")
        return 0
    if args.cmd == "delete":
        if store.delete(args.id):
            print(f"已刪除筆記 #{args.id}")
            return 0
        print(f"找不到筆記 #{args.id}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    sys.exit(main())
