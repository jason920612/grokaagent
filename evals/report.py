#!/usr/bin/env python3
"""Summarize a results directory into report.md (and print it).

    python evals/report.py evals/results/<stamp>

Tiers per category (mean score): S >= 0.95, A >= 0.85, B >= 0.70, C >= 0.50, D < 0.50.
"""
from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

CATS = [("short", "短任務"), ("long", "長任務"), ("domain", "專業領域")]


INFRA_MARKERS = ("http 402", "http 429", "http 5", "in_flight_budget", "temporarily unavailable",
                 "degraded", "overloaded", "rate limit", "error decoding response body", "error sending request")


def infra_failure(r: dict) -> bool:
    """The run died on the provider (credits, rate limit, outage), not the model."""
    m = r["metrics"]
    errs = " ".join(m.get("errors", [])).lower()
    hit = any(k in errs for k in INFRA_MARKERS)
    return hit and (m.get("finish") in ("error", "") or r.get("timed_out"))


def tier(x: float) -> str:
    for cut, name in ((0.95, "S"), (0.85, "A"), (0.70, "B"), (0.50, "C")):
        if x >= cut:
            return name
    return "D"


def mean(xs):
    xs = list(xs)
    return statistics.fmean(xs) if xs else 0.0


def load(out: Path) -> list[dict]:
    rows = []
    rj = out / "results.jsonl"
    if rj.exists():
        rows = [json.loads(l) for l in rj.read_text(encoding="utf-8").splitlines() if l.strip()]
    else:  # partial run: collect per-run files
        rows = [json.loads(p.read_text(encoding="utf-8")) for p in sorted(out.glob("*/*/t*/result.json"))]
    return rows


def main() -> None:
    sys.stdout.reconfigure(encoding="utf-8")
    out = Path(sys.argv[1])
    all_rows = load(out)
    invalid = [r for r in all_rows if infra_failure(r)]
    rows = [r for r in all_rows if not infra_failure(r)]
    models = sorted({r["model"] for r in rows})
    tasks = sorted({r["task"] for r in rows})
    by = defaultdict(list)
    for r in rows:
        by[(r["model"], r["task"])].append(r)
    L = []
    L.append(f"# grokaagent 真實任務評測：{', '.join(models)}\n")
    L.append(f"結果目錄：`{out}`；{len(rows)} 次執行，{len(tasks)} 個任務。\n")

    L.append("## 總覽與分級\n")
    L.append("| 模型 | " + " | ".join(n for _, n in CATS) + " | 總分 | 等級 | 平均耗時 | 平均輪數 | 工具錯誤率 | 快取命中 | 逾時 |")
    L.append("|" + "---|" * (len(CATS) + 8))
    for m in models:
        mr = [r for r in rows if r["model"] == m]
        cells = []
        for c, _ in CATS:
            xs = [r["score"] for r in mr if r["category"] == c]
            cells.append(f"{mean(xs):.2f} ({tier(mean(xs))})" if xs else "–")
        overall = mean(r["score"] for r in mr)
        calls = sum(r["metrics"]["tool_calls"] for r in mr)
        errs = sum(r["metrics"]["tool_errors"] for r in mr)
        inp = sum(r["metrics"]["input_tokens"] for r in mr)
        cached = sum(r["metrics"]["cached_tokens"] for r in mr)
        L.append(f"| {m} | " + " | ".join(cells) + f" | {overall:.2f} | **{tier(overall)}** | "
                 f"{mean(r['wall_s'] for r in mr):.0f}s | {mean(r['metrics']['turns'] for r in mr):.1f} | "
                 f"{(errs / calls if calls else 0):.1%} | {(cached / inp if inp else 0):.0%} | "
                 f"{sum(r['timed_out'] for r in mr)} |")
    L.append("\n等級：S ≥ 0.95、A ≥ 0.85、B ≥ 0.70、C ≥ 0.50、D < 0.50（各類別平均分）。\n")

    L.append("## 各任務\n")
    L.append("| 任務 | 類別 | " + " | ".join(models) + " |")
    L.append("|---|---|" + "---|" * len(models))
    for t in tasks:
        cat = next(r["category"] for r in rows if r["task"] == t)
        cells = []
        for m in models:
            rs = by.get((m, t), [])
            if not rs:
                cells.append("–")
                continue
            s = mean(r["score"] for r in rs)
            w = mean(r["wall_s"] for r in rs)
            turns = mean(r["metrics"]["turns"] for r in rs)
            e = sum(r["metrics"]["tool_errors"] for r in rs)
            c = sum(r["metrics"]["tool_calls"] for r in rs)
            flag = " ⏱" if any(r["timed_out"] for r in rs) else ""
            cells.append(f"**{s:.2f}** · {w:.0f}s · {turns:.0f} 輪 · 錯 {e}/{c}{flag}")
        L.append(f"| {t} | {cat} | " + " | ".join(cells) + " |")

    L.append("\n## 框架適應度\n")
    for m in models:
        mr = [r for r in rows if r["model"] == m]
        tools = defaultdict(int)
        for r in mr:
            for k, v in r["metrics"]["tools"].items():
                tools[k] += v
        top = ", ".join(f"{k} {v}" for k, v in sorted(tools.items(), key=lambda kv: -kv[1])[:12])
        edit_err = sum(r["metrics"]["edit_file_errors"] for r in mr)
        edits = tools.get("edit_file", 0)
        kids = sum(r["metrics"]["children"] for r in mr)
        finishes = defaultdict(int)
        for r in mr:
            finishes[r["metrics"]["finish"] or ("timeout" if r["timed_out"] else "none")] += 1
        L.append(f"### {m}\n")
        L.append(f"- 工具使用：{top}")
        L.append(f"- edit_file 失敗：{edit_err}/{edits}；子代理開啟：{kids}；結束原因：{dict(finishes)}")
        samples = [s for r in mr for s in r["metrics"]["tool_error_samples"]][:10]
        if samples:
            L.append("- 工具錯誤樣本：")
            for s in samples:
                L.append(f"  - `{s[:200]}`")
        errs = [e for r in mr for e in r["metrics"]["errors"]][:5]
        if errs:
            L.append("- 執行錯誤：")
            for e in errs:
                L.append(f"  - `{e[:200]}`")
        L.append("")

    L.append("## 未通過的檢查\n")
    for r in sorted(rows, key=lambda r: (r["task"], r["model"])):
        bad = [c for c in r["grade"].get("checks", []) if not c["ok"]]
        if not bad and "error" not in r["grade"]:
            continue
        L.append(f"- **{r['model']} / {r['task']} t{r['trial']}**（{r['score']:.2f}）")
        if "error" in r["grade"]:
            L.append(f"  - grader: {r['grade']['error'][:200]}")
        for c in bad[:6]:
            L.append(f"  - {c['name']} = {c.get('value', 0):.2f} {('— ' + c['detail'][:160]) if c['detail'] else ''}")
    text = "\n".join(L) + "\n"
    (out / "report.md").write_text(text, encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
