"""Command line: python -m notes.cli --db FILE <command> ..."""
import argparse
import sys

import legacy_api
from notes.store import NoteStore, search, to_markdown, with_tag


def build_parser():
    p = argparse.ArgumentParser(prog="notes")
    p.add_argument("--db", default="notes.json")
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("add")
    a.add_argument("title")
    a.add_argument("body")
    a.add_argument("--tag", action="append", default=[])
    lst = sub.add_parser("list")
    lst.add_argument("--tag")
    s = sub.add_parser("show")
    s.add_argument("id", type=int)
    d = sub.add_parser("delete")
    d.add_argument("id", type=int)
    q = sub.add_parser("search")
    q.add_argument("query")
    e = sub.add_parser("export")
    e.add_argument("--md", required=True)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    store = NoteStore(args.db)
    if args.cmd == "add":
        note_id = store.add(args.title, args.body, args.tag)
        print(f"Added note #{note_id}")
        return 0
    if args.cmd == "list":
        notes = legacy_api.list_notes(store)
        if args.tag:
            notes = with_tag(notes, args.tag)
        if not notes:
            print("No notes")
        for note in notes:
            print(legacy_api.format_note(note))
        return 0
    if args.cmd == "show":
        note = legacy_api.get_note(store, args.id)
        if note is None:
            print(f"Note #{args.id} not found", file=sys.stderr)
            return 1
        print(f"#{note['id']} {note['title']}\n\n{note['body']}")
        return 0
    if args.cmd == "delete":
        if store.delete(args.id):
            print(f"Deleted note #{args.id}")
            return 0
        print(f"Note #{args.id} not found", file=sys.stderr)
        return 1
    if args.cmd == "search":
        found = search(legacy_api.list_notes(store), args.query)
        if not found:
            print("No matching notes")
        for note in found:
            print(legacy_api.format_note(note))
        return 0
    if args.cmd == "export":
        notes = legacy_api.list_notes(store)
        with open(args.md, "w", encoding="utf-8") as fh:
            fh.write(to_markdown(notes))
        print(f"Exported {len(notes)} notes to {args.md}")
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
