#!/usr/bin/env python3
"""Run the real-task suite against one or more models through `grokaagent run`.

Each (model, task, trial) gets a fresh copy of the task's workspace and its
own GROKA_HOME / sessions / memory / skills, so runs never see each other or
the user's real data. Only the xAI login is shared (Grok models need it).

    python evals/run.py --models grok-4.7,deepseek-v4.1-flash
    python evals/run.py --models deepseek-v4.1-flash --tasks S1,D2 --trials 2

Keys are read from the environment named in evals/models.json (never from
argv, never printed). Results land in evals/results/<stamp>/; see report.py.
"""
from __future__ import annotations

import argparse
import concurrent.futures as cf
import datetime as dt
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
TASKS = ROOT / "tasks"
PRINT_LOCK = threading.Lock()


def log(msg: str) -> None:
    with PRINT_LOCK:
        print(f"[{dt.datetime.now():%H:%M:%S}] {msg}", flush=True)


def load_models() -> dict:
    return json.loads((ROOT / "models.json").read_text(encoding="utf-8"))


def load_tasks(filters: list[str]) -> list[dict]:
    out = []
    for d in sorted(TASKS.iterdir()):
        spec = d / "task.json"
        if not spec.is_file():
            continue
        t = json.loads(spec.read_text(encoding="utf-8"))
        t["dir"] = d
        if filters and not any(t["id"] == f or t["id"].startswith(f + "-") or t["id"].startswith(f) for f in filters):
            continue
        out.append(t)
    return out


def default_bin() -> Path:
    exe = "grokaagent.exe" if os.name == "nt" else "grokaagent"
    return REPO / "target" / "release" / exe


def model_env(base: dict, m: dict, home: Path) -> dict:
    env = {k: v for k, v in base.items() if not k.startswith(("GROKA_", "OPENAI_")) or k == "GROKA_BASH"}
    env.update({
        "GROKA_HOME": str(home / "cfg"),
        "GROKA_SESSIONS_DIR": str(home / "sessions"),
        "GROKA_MEMORY_DIR": str(home / "memory"),
        "GROKA_SKILLS_DIR": str(home / "skills"),
        "GROKA_NO_UPDATE": "1",
        "GROKA_NO_WEB_OPEN": "1",
        "PYTHONIOENCODING": "utf-8",
    })
    auth = base.get("GROKA_EVAL_XAI_AUTH")
    if auth:
        env["GROKA_XAI_AUTH_FILE"] = auth
    if m["provider"] == "openai":
        key = base.get(m["key_env"], "")
        if not key:
            raise SystemExit(f"model {m['model']} needs ${m['key_env']} in the environment")
        env.update({
            "GROKA_PROVIDER": "openai",
            "GROKA_API_BASE": m["base_url"],
            "GROKA_API_KEY": key,
            "GROKA_MODEL": m["model"],
        })
    if m.get("context"):
        env["GROKA_CONTEXT_WINDOW"] = m["context"]
    return env


def kill_tree(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(proc.pid)], capture_output=True)
    else:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def run_grader(task: dict, ws: Path, events: Path) -> dict:
    grader = task["dir"] / "grade.py"
    try:
        r = subprocess.run(
            [sys.executable, str(grader), str(ws), str(events)],
            capture_output=True, text=True, encoding="utf-8", timeout=300,
        )
        g = json.loads(r.stdout.strip().splitlines()[-1])
        if r.returncode != 0 and "score" not in g:
            raise ValueError(r.stderr[-500:])
        return g
    except Exception as e:  # a crashing grader scores 0 and says why
        return {"score": 0.0, "checks": [], "error": f"grader failed: {e}"}


def event_metrics(events: Path) -> dict:
    m = {
        "turns": 0, "tool_calls": 0, "tool_errors": 0, "tools": {}, "tool_error_samples": [],
        "edit_file_errors": 0, "children": 0, "child_names": [], "input_tokens": 0,
        "cached_tokens": 0, "compactions": 0, "errors": [], "finish": "", "final_text": "",
        "root_turns": 0, "server_tools": 0,
    }
    if not events.exists():
        return m
    for line in events.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        typ = e.get("type")
        root = not e.get("agent_path") and e.get("agent_name") == "root"
        if typ == "turn_started":
            m["turns"] += 1
            m["root_turns"] += int(root)
        elif typ == "tool_finished":
            name = e.get("name", "?")
            m["tool_calls"] += 1
            m["tools"][name] = m["tools"].get(name, 0) + 1
            out = e.get("output", "")
            if out.startswith('{"error"'):
                m["tool_errors"] += 1
                if name == "edit_file":
                    m["edit_file_errors"] += 1
                if len(m["tool_error_samples"]) < 12:
                    m["tool_error_samples"].append(f"{name}: {out[:220]}")
        elif typ == "model_finished":
            m["input_tokens"] += e.get("input_tokens", 0)
            m["cached_tokens"] += e.get("cached_tokens", 0)
        elif typ == "server_tool_observed":
            m["server_tools"] += 1
        elif typ == "child_spawned":
            m["children"] += 1
            m["child_names"].append(e.get("name", ""))
        elif typ == "context_compacted":
            m["compactions"] += 1
        elif typ == "error":
            m["errors"].append(e.get("message", "")[:300])
        elif typ == "run_finished" and root:
            m["finish"] = e.get("reason", "")
            m["final_text"] = e.get("text", "")[:2000]
    m["tool_error_rate"] = round(m["tool_errors"] / m["tool_calls"], 3) if m["tool_calls"] else 0.0
    m["cache_rate"] = round(m["cached_tokens"] / m["input_tokens"], 3) if m["input_tokens"] else 0.0
    return m


def run_one(bin_path: Path, alias: str, m: dict, task: dict, trial: int, out_root: Path) -> dict:
    run_dir = out_root / alias / task["id"] / f"t{trial}"
    if run_dir.exists():
        shutil.rmtree(run_dir)
    ws = run_dir / "workspace"
    src_ws = task["dir"] / "workspace"
    if src_ws.is_dir():
        shutil.copytree(src_ws, ws)
    else:
        ws.mkdir(parents=True)
    setup = task["dir"] / "setup.py"
    if setup.is_file():
        subprocess.run([sys.executable, str(setup), str(ws)], check=True, capture_output=True)
    home = run_dir / "home"
    home.mkdir(parents=True)
    events = run_dir / "events.jsonl"
    env = model_env(dict(os.environ), m, home)
    # The workspace lives inside this repo: keep git from reporting the
    # repo's own changes as the run's file changes.
    env["GIT_CEILING_DIRECTORIES"] = str(run_dir)
    cmd = [
        str(bin_path), "run",
        "--model", m["model"],
        "--events", str(events),
        "--workspace", str(ws),
        "--max-turns", str(task.get("max_turns", 40)),
        "--reasoning", m.get("reasoning", "high"),
        task["prompt"],
    ]
    timeout = task.get("timeout_s", 600)
    log(f"start  {alias:22s} {task['id']} t{trial}")
    t0 = time.time()
    kwargs = {"start_new_session": True} if os.name != "nt" else {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}
    with open(run_dir / "stdout.txt", "wb") as so, open(run_dir / "stderr.txt", "wb") as se:
        proc = subprocess.Popen(cmd, cwd=ws, env=env, stdout=so, stderr=se, stdin=subprocess.DEVNULL, **kwargs)
        timed_out = False
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_tree(proc)
            proc.wait()
    wall = round(time.time() - t0, 1)
    grade = run_grader(task, ws, events)
    metrics = event_metrics(events)
    result = {
        "model": alias, "model_id": m["model"], "task": task["id"], "category": task["category"],
        "title": task.get("title", ""), "trial": trial, "score": float(grade.get("score", 0.0)),
        "wall_s": wall, "timed_out": timed_out, "exit_code": proc.returncode, "grade": grade,
        "metrics": metrics,
    }
    (run_dir / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    log(f"done   {alias:22s} {task['id']} t{trial}  score={result['score']:.2f}  {wall:.0f}s"
        f"{'  TIMEOUT' if timed_out else ''}  turns={metrics['turns']} tool_err={metrics['tool_errors']}/{metrics['tool_calls']}")
    return result


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--models", required=True, help="comma list of aliases from models.json")
    ap.add_argument("--tasks", default="", help="comma list of task ids or id prefixes (default: all)")
    ap.add_argument("--trials", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=2, help="concurrent runs per model")
    ap.add_argument("--bin", type=Path, default=default_bin())
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--rerun-invalid", action="store_true",
                    help="with --out: rerun only runs that died on provider errors, in place")
    args = ap.parse_args()

    models = load_models()
    aliases = [a.strip() for a in args.models.split(",") if a.strip()]
    for a in aliases:
        if a not in models:
            raise SystemExit(f"unknown model alias {a}; known: {', '.join(models)}")
    tasks = load_tasks([f.strip() for f in args.tasks.split(",") if f.strip()])
    if not tasks:
        raise SystemExit("no tasks matched")
    args.bin = args.bin.resolve()
    if not args.bin.exists():
        raise SystemExit(f"binary not found: {args.bin} (cargo build --release)")
    out_root = (args.out or ROOT / "results" / dt.datetime.now().strftime("%Y%m%d-%H%M%S")).resolve()
    out_root.mkdir(parents=True, exist_ok=True)
    (out_root / "run.json").write_text(json.dumps({
        "models": {a: models[a] for a in aliases}, "tasks": [t["id"] for t in tasks],
        "trials": args.trials, "started": dt.datetime.now().isoformat(),
        "bin": str(args.bin),
    }, ensure_ascii=False, indent=2), encoding="utf-8")
    log(f"{len(aliases)} model(s) x {len(tasks)} task(s) x {args.trials} trial(s) -> {out_root}")

    only: set | None = None
    if args.rerun_invalid:
        sys.path.insert(0, str(ROOT))
        from report import infra_failure
        only = set()
        for rf in out_root.glob("*/*/t*/result.json"):
            r = json.loads(rf.read_text(encoding="utf-8"))
            if infra_failure(r):
                only.add((r["model"], r["task"], r["trial"]))
        log(f"rerunning {len(only)} invalid run(s)")

    # One pool per model so a slow model never starves the other.
    results: list[dict] = []
    pools = {a: cf.ThreadPoolExecutor(max_workers=args.jobs) for a in aliases}
    futures = []
    for trial in range(1, args.trials + 1):
        for t in tasks:
            for a in aliases:
                if only is not None and (a, t["id"], trial) not in only:
                    continue
                futures.append(pools[a].submit(run_one, args.bin, a, models[a], t, trial, out_root))
    for f in cf.as_completed(futures):
        try:
            results.append(f.result())
        except Exception as e:
            log(f"run crashed: {e!r}")
    for p in pools.values():
        p.shutdown()
    # Rebuild from every run on disk, so partial reruns merge with the rest.
    results = [json.loads(p.read_text(encoding="utf-8")) for p in out_root.glob("*/*/t*/result.json")]
    with open(out_root / "results.jsonl", "w", encoding="utf-8") as fh:
        for r in sorted(results, key=lambda r: (r["model"], r["task"], r["trial"])):
            fh.write(json.dumps(r, ensure_ascii=False) + "\n")
    log(f"all done -> {out_root / 'results.jsonl'}; python evals/report.py {out_root}")


if __name__ == "__main__":
    main()
