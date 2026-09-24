#!/usr/bin/env python3
"""Compare framework metrics between two result directories.

    python evals/compare.py evals/results/<before> evals/results/<after>

Scores alone saturate on easy tasks; this looks at how the models used the
tools: edit_file failure rate and causes, total tool errors, turns, time,
and delete+rewrite workarounds.
"""
from __future__ import annotations

import json
import re
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path


INFRA_MARKERS = ("http 402", "http 429", "http 5", "in_flight_budget", "temporarily unavailable",
                 "degraded", "overloaded", "rate limit", "error decoding response body", "error sending request")


def infra_failure(r: dict) -> bool:
    """The run died on the provider (credits, rate limit, outage), not the model."""
    m = r["metrics"]
    errs = " ".join(m.get("errors", [])).lower()
    hit = any(k in errs for k in INFRA_MARKERS)
    return hit and (m.get("finish") in ("error", "") or r.get("timed_out"))


def edit_cause(out: str) -> str:
    if re.search(r"has \d+ (old|new) lines", out):
        return "header count"
    if "before edit_file" in out or "read " in out and "before editing" in out:
        return "not read"
    if "changed since" in out:
        return "stale"
    if "does not match" in out or "not found in the file" in out:
        return "no match"
    if "matches" in out and "places" in out or "occurs" in out:
        return "ambiguous"
    return "other"


def collect(root: Path) -> dict:
    per = defaultdict(lambda: {"runs": 0, "score": [], "wall": [], "turns": [], "tools": 0, "tool_err": 0,
                               "edit": 0, "edit_err": 0, "causes": Counter(), "write": 0, "delete": 0,
                               "read": 0, "notes": 0})
    for rf in root.glob("*/*/t*/result.json"):
        r = json.loads(rf.read_text(encoding="utf-8"))
        if infra_failure(r):
            per[r["model"]]["invalid"] = per[r["model"]].get("invalid", 0) + 1
            continue
        m = per[r["model"]]
        m["runs"] += 1
        m["score"].append(r["score"])
        m["wall"].append(r["wall_s"])
        m["turns"].append(r["metrics"]["turns"])
        ev = rf.parent / "events.jsonl"
        if not ev.exists():
            continue
        for line in ev.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            if e.get("type") != "tool_finished":
                continue
            name, out = e.get("name"), e.get("output", "")
            err = out.startswith('{"error"')
            m["tools"] += 1
            m["tool_err"] += err
            if name == "edit_file":
                m["edit"] += 1
                if err:
                    m["edit_err"] += 1
                    m["causes"][edit_cause(out)] += 1
                elif '"notes"' in out:
                    m["notes"] += 1
            elif name == "write_file":
                m["write"] += 1
            elif name == "delete_file":
                m["delete"] += 1
            elif name == "read_file":
                m["read"] += 1
    return per


def pct(a, b):
    return f"{a / b:.0%}" if b else "–"


def main() -> None:
    sys.stdout.reconfigure(encoding="utf-8")
    before, after = Path(sys.argv[1]), Path(sys.argv[2])
    A, B = collect(before), collect(after)
    print(f"# edit_file 前後比較\n\n前：`{before.name}`　後：`{after.name}`\n")
    print("| 模型 | 指標 | 前 | 後 |")
    print("|---|---|---|---|")
    for model in sorted(set(A) | set(B)):
        a, b = A.get(model), B.get(model)
        if not a or not b:
            continue
        rows = [
            ("有效執行（排除供應商錯誤）", f"{a['runs']} (+{a.get('invalid', 0)} 無效)", f"{b['runs']} (+{b.get('invalid', 0)} 無效)"),
            ("平均分", f"{statistics.fmean(a['score']):.3f}", f"{statistics.fmean(b['score']):.3f}"),
            ("edit_file 失敗率", f"{pct(a['edit_err'], a['edit'])} ({a['edit_err']}/{a['edit']})",
             f"{pct(b['edit_err'], b['edit'])} ({b['edit_err']}/{b['edit']})"),
            ("失敗原因", ", ".join(f"{k} {v}" for k, v in a["causes"].most_common()) or "–",
             ", ".join(f"{k} {v}" for k, v in b["causes"].most_common()) or "–"),
            ("成功但有提示（位移/寬鬆比對/過期）", "–", str(b["notes"])),
            ("全部工具錯誤率", pct(a["tool_err"], a["tools"]), pct(b["tool_err"], b["tools"])),
            ("每次執行 edit / write / delete / read",
             f"{a['edit']/a['runs']:.1f} / {a['write']/a['runs']:.1f} / {a['delete']/a['runs']:.1f} / {a['read']/a['runs']:.1f}",
             f"{b['edit']/b['runs']:.1f} / {b['write']/b['runs']:.1f} / {b['delete']/b['runs']:.1f} / {b['read']/b['runs']:.1f}"),
            ("平均輪數", f"{statistics.fmean(a['turns']):.1f}", f"{statistics.fmean(b['turns']):.1f}"),
            ("平均耗時", f"{statistics.fmean(a['wall']):.0f}s", f"{statistics.fmean(b['wall']):.0f}s"),
        ]
        for i, (k, x, y) in enumerate(rows):
            print(f"| {model if i == 0 else ''} | {k} | {x} | {y} |")


if __name__ == "__main__":
    main()
