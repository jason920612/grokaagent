#!/usr/bin/env python3
"""How models used the shell in one or more result directories.

    python evals/shellstats.py evals/results/<a> evals/results/<b> ...

Per model and directory: commands per run, which shell they asked for,
bash-style syntax share, nonzero exits, blocked commands, Python scripts
used for work a pipeline could do, plus turns / time / score.
"""
from __future__ import annotations

import json
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from report import infra_failure  # noqa: E402

BASHY = re.compile(r"\|\s*(grep|head|tail|wc|sort|uniq|awk|sed|cut|xargs|tr)\b|^\s*(ls|cat|find|grep|head|tail|wc)\b|\$\(|2>/dev/null|&&")
PY_INLINE = re.compile(r"^\s*(python3?|py)\s+(-c\b|-\s*<<)|<<\s*['\"]?(EOF|PY)")


def stats(root: Path) -> dict:
    per = defaultdict(lambda: defaultdict(float))
    for rf in root.glob("*/*/t*/result.json"):
        r = json.loads(rf.read_text(encoding="utf-8"))
        if infra_failure(r):
            continue
        m = per[r["model"]]
        m["runs"] += 1
        m["score"] += r["score"]
        m["wall"] += r["wall_s"]
        m["turns"] += r["metrics"]["turns"]
        started = {}
        ev = rf.parent / "events.jsonl"
        for line in ev.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            if e.get("type") == "tool_started" and e.get("name") in ("run_command", "run_background"):
                started[e["call_id"]] = e.get("args", {})
            elif e.get("type") == "tool_finished" and e.get("call_id") in started:
                a = started.pop(e["call_id"])
                cmd, out = a.get("command", ""), e.get("output", "")
                m["cmds"] += 1
                m[f"shell:{a.get('shell') or 'default'}"] += 1
                m["bashy"] += bool(BASHY.search(cmd))
                m["py_inline"] += bool(PY_INLINE.search(cmd))
                if '"blocked"' in out:
                    m["blocked"] += 1
                elif out.startswith('{"error"'):
                    m["error"] += 1
                else:
                    try:
                        code = json.loads(out).get("exit_code")
                    except (json.JSONDecodeError, AttributeError):
                        code = None
                    m["nonzero"] += code not in (0, None)
    return per


def main() -> None:
    sys.stdout.reconfigure(encoding="utf-8")
    dirs = [Path(a) for a in sys.argv[1:]]
    print("| 設定 | 模型 | 分數 | 每次指令數 | bash 風格 | python -c/heredoc | 非 0 結束 | 被擋 | 工具錯誤 | shell 參數 | 平均輪數 | 平均耗時 |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for d in dirs:
        for model, m in sorted(stats(d).items()):
            n, c = m["runs"] or 1, m["cmds"] or 1
            shells = ", ".join(f"{k[6:]} {int(v)}" for k, v in sorted(m.items()) if k.startswith("shell:"))
            print(f"| {d.name} | {model} | {m['score'] / n:.3f} | {m['cmds'] / n:.1f} | {m['bashy'] / c:.0%} | "
                  f"{m['py_inline'] / c:.0%} | {m['nonzero'] / c:.0%} | {int(m['blocked'])} | {int(m['error'])} | {shells} | "
                  f"{m['turns'] / n:.1f} | {m['wall'] / n:.0f}s |")


if __name__ == "__main__":
    main()
