#!/usr/bin/env python3
"""Command-line todo list (reference solution for the eval task)."""
import argparse
import datetime as dt
import json
import os
import sys

PRIORITIES = ("low", "medium", "high")
RANK = {"high": 0, "medium": 1, "low": 2}


class CliError(Exception):
    def __init__(self, message, code):
        super().__init__(message)
        self.code = code


def parse_date(s):
    try:
        return dt.date.fromisoformat(s).isoformat()
    except (TypeError, ValueError):
        raise CliError(f"invalid date: {s}", 2)


def load(path):
    if not os.path.exists(path):
        return {"next_id": 1, "tasks": []}
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
        if not isinstance(data, dict) or not isinstance(data.get("tasks"), list):
            raise ValueError("bad shape")
        data.setdefault("next_id", max([t["id"] for t in data["tasks"]] + [0]) + 1)
        return data
    except (ValueError, OSError, KeyError, TypeError) as e:
        raise CliError(f"cannot read {path}: {e}", 1)


def save(path, data):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(data, fh, ensure_ascii=False, indent=2)
    os.replace(tmp, path)


def find(data, tid):
    for t in data["tasks"]:
        if t["id"] == tid:
            return t
    raise CliError(f"task #{tid} not found", 1)


def check_priority(p):
    if p not in PRIORITIES:
        raise CliError(f"invalid priority: {p}", 2)
    return p


def fmt(t):
    line = f"#{t['id']} [{'x' if t['done'] else ' '}] {t['title']} ({t['priority']})"
    if t.get("due"):
        line += f" due:{t['due']}"
    if t.get("tags"):
        line += " tags:" + ",".join(t["tags"])
    return line


def today(args):
    return parse_date(args.today) if getattr(args, "today", None) else dt.date.today().isoformat()


def add_tags(tags, new):
    for tag in new or []:
        if tag not in tags:
            tags.append(tag)


def cmd_add(args, data):
    title = args.title.strip()
    if not title:
        raise CliError("title must not be empty", 2)
    prio = check_priority(args.priority)
    due = parse_date(args.due) if args.due else None
    tags = []
    add_tags(tags, args.tag)
    task = {"id": data["next_id"], "title": title, "priority": prio, "due": due, "tags": tags, "done": False}
    data["next_id"] += 1
    data["tasks"].append(task)
    print(f"Added #{task['id']}: {title}")
    return True


def cmd_list(args, data):
    items = [t for t in data["tasks"] if args.all or not t["done"]]
    if args.priority:
        check_priority(args.priority)
        items = [t for t in items if t["priority"] == args.priority]
    if args.tag:
        items = [t for t in items if args.tag in t.get("tags", [])]
    if args.overdue:
        now = today(args)
        items = [t for t in items if not t["done"] and t.get("due") and t["due"] < now]
    if args.sort == "priority":
        items.sort(key=lambda t: (RANK[t["priority"]], t["id"]))
    elif args.sort == "due":
        items.sort(key=lambda t: (t.get("due") is None, t.get("due") or "", t["id"]))
    else:
        items.sort(key=lambda t: t["id"])
    if not items:
        print("No tasks.")
    for t in items:
        print(fmt(t))
    return False


def cmd_done(args, data):
    t = find(data, args.id)
    if t["done"]:
        print(f"Task #{t['id']} already completed")
        return False
    t["done"] = True
    print(f"Completed #{t['id']}")
    return True


def cmd_remove(args, data):
    t = find(data, args.id)
    data["tasks"].remove(t)
    print(f"Removed #{t['id']}")
    return True


def cmd_edit(args, data):
    if not any([args.title is not None, args.priority, args.due, args.add_tag, args.remove_tag]):
        raise CliError("nothing to update", 2)
    t = find(data, args.id)
    if args.title is not None:
        title = args.title.strip()
        if not title:
            raise CliError("title must not be empty", 2)
        t["title"] = title
    if args.priority:
        t["priority"] = check_priority(args.priority)
    if args.due:
        t["due"] = None if args.due == "none" else parse_date(args.due)
    add_tags(t.setdefault("tags", []), args.add_tag)
    for tag in args.remove_tag or []:
        if tag in t["tags"]:
            t["tags"].remove(tag)
    print(f"Updated #{t['id']}")
    return True


def cmd_stats(args, data):
    now = today(args)
    tasks = data["tasks"]
    open_ = [t for t in tasks if not t["done"]]
    overdue = [t for t in open_ if t.get("due") and t["due"] < now]
    counts = {p: sum(1 for t in open_ if t["priority"] == p) for p in PRIORITIES}
    print(f"total: {len(tasks)}")
    print(f"open: {len(open_)}")
    print(f"done: {len(tasks) - len(open_)}")
    print(f"overdue: {len(overdue)}")
    print(f"by priority: high={counts['high']} medium={counts['medium']} low={counts['low']}")
    return False


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise CliError(message, 2)


def build():
    p = Parser(prog="todo.py")
    p.add_argument("--db")
    sub = p.add_subparsers(dest="cmd", required=True, parser_class=Parser)
    a = sub.add_parser("add")
    a.add_argument("title")
    a.add_argument("--priority", default="medium")
    a.add_argument("--due")
    a.add_argument("--tag", action="append")
    ls = sub.add_parser("list")
    ls.add_argument("--all", action="store_true")
    ls.add_argument("--priority")
    ls.add_argument("--tag")
    ls.add_argument("--overdue", action="store_true")
    ls.add_argument("--today")
    ls.add_argument("--sort", choices=["id", "priority", "due"], default="id")
    for name in ("done", "remove"):
        s = sub.add_parser(name)
        s.add_argument("id", type=int)
    e = sub.add_parser("edit")
    e.add_argument("id", type=int)
    e.add_argument("--title")
    e.add_argument("--priority")
    e.add_argument("--due")
    e.add_argument("--add-tag", action="append")
    e.add_argument("--remove-tag", action="append")
    st = sub.add_parser("stats")
    st.add_argument("--today")
    return p


def main(argv=None):
    try:
        args = build().parse_args(argv)
        path = args.db or os.path.join(os.path.dirname(os.path.abspath(__file__)), "todo.json")
        data = load(path)
        handler = {"add": cmd_add, "list": cmd_list, "done": cmd_done, "remove": cmd_remove,
                   "edit": cmd_edit, "stats": cmd_stats}[args.cmd]
        if handler(args, data):
            save(path, data)
        return 0
    except CliError as e:
        print(f"error: {e}", file=sys.stderr)
        return e.code


if __name__ == "__main__":
    sys.exit(main())
