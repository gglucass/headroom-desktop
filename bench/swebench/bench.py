#!/usr/bin/env python3
"""SWE-bench Verified A/B: Claude Code direct to Anthropic (control) vs through Headroom (treatment).

Subcommands (run with bench/swebench/.venv/bin/python):
  select  pick instances with a fixed seed, optionally keep only ones whose gold patch passes locally
  run     run claude -p on each instance in one or both arms
  eval    local (non-Docker) evaluation of every run's patch with the SWE-bench log parsers/grader
  report  results table, per-arm totals, and SWE-bench predictions JSONL per arm
"""

import argparse
import json
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / "results"
# Outside the headroom-desktop tree on purpose: Claude Code loads every CLAUDE.md from the cwd
# up to /, so a work dir under bench/ would feed headroom-desktop/CLAUDE.md to the agent.
WORK = Path(os.environ.get("SWEBENCH_WORK", Path.home() / ".cache/headroom-swebench"))
DATASET = "SWE-bench/SWE-bench_Verified"  # same 500 ids as princeton-nlp/SWE-bench_Verified, plus eval_script/image/log_parser
REVISION = "78f471bf655a3137b2e8a75af1501690ec009ec3"  # pinned HF dataset commit
# HEADROOM_BENCH_PROXY points the treatment arm at another Headroom (e.g. a scratch
# proxy with a candidate sitecustomize); default is the desktop app's front port.
PROXY = os.environ.get("HEADROOM_BENCH_PROXY", "http://127.0.0.1:6767")
PROXY_PORT = ":" + PROXY.rsplit(":", 1)[1].split("/")[0]
DIRECT = "https://api.anthropic.com"
ARM_URL = {"control": DIRECT, "treatment": PROXY}
SAVINGS_EVENTS = Path.home() / ".headroom/savings_events.jsonl"
CLAUDE = str(Path.home() / ".local/bin/claude")
TOOLS = "Read,Edit,Write,Grep,Glob,Bash"
TEST_PATHSPECS = [":(exclude,glob)**/tests/**", ":(exclude,glob)**/test/**", ":(exclude,glob)**/test_*.py",
                  ":(exclude,glob)**/*_test.py", ":(exclude,glob)**/*_tests.py", ":(exclude,glob)**/conftest.py",
                  ":(exclude,glob)**/__pycache__/**", ":(exclude,glob)**/*.pyc"]
PROMPT = """<uploaded_files>
{repo}
</uploaded_files>
I've uploaded a Python code repository in the directory {repo}. Consider the following issue description:

<issue_description>
{problem}
</issue_description>

Can you help me implement the necessary changes to the repository so that the requirements specified in the <issue_description> are met?
I've already taken care of all changes to any of the test files described in the <issue_description>. This means you DON'T have to modify the testing logic or any of the tests in any way!
Your task is to make the minimal changes to non-test files in the repository to ensure the <issue_description> is satisfied.
A Python environment with the repository installed in editable mode is at {venv}; use {venv}/bin/python to run code and tests. Do not install packages outside it.
Delete any temporary scripts you create before you finish.
"""


def sh(cmd, cwd=None, check=True, **kw):
    return subprocess.run(cmd, cwd=cwd, check=check, text=True, capture_output=True, **kw)


def load_dataset_rows():
    from datasets import load_dataset
    return list(load_dataset(DATASET, split="test", revision=REVISION))


def python_for(inst):
    # ponytail: hand-picked, SWE-bench 5 dropped its per-version spec table; the gold check in `select` catches misses
    if inst["repo"] == "django/django":
        v = tuple(int(x) for x in inst["version"].split("."))
        return "3.8" if v < (4, 0) else "3.9" if v < (5, 0) else "3.11"
    return "3.9"


def mirror(repo):
    path = WORK / "mirrors" / (repo.replace("/", "__") + ".git")
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        sh(["git", "clone", "-q", "--bare", f"https://github.com/{repo}.git", str(path)])  # --bare: branches+tags, no GitHub PR refs
        sh(["git", "-C", str(path), "config", "uploadpack.allowAnySHA1InWant", "true"])
    return path


def checkout(inst, dest):
    """Fresh repo at base_commit with no history beyond it and no remote, plus its own venv."""
    if dest.exists():
        shutil.rmtree(dest)
    repo, venv = dest / "repo", dest / "venv"
    repo.mkdir(parents=True)
    sh(["git", "init", "-q"], cwd=repo)
    sh(["git", "fetch", "-q", "--depth", "1", f"file://{mirror(inst['repo'])}", inst["base_commit"]], cwd=repo)
    sh(["git", "checkout", "-q", "FETCH_HEAD"], cwd=repo)
    sh(["uv", "venv", "-q", "-p", python_for(inst), str(venv)])
    extra = ["mpmath==1.3.0"] if inst["repo"] == "sympy/sympy" else []
    sh(["uv", "pip", "install", "-q", "-p", str(venv / "bin/python"), "-e", str(repo), *extra])
    return repo, venv


def run_tests(inst, repo, venv, log_path, timeout=1800):
    """Apply the gold test patch and run the instance's own test command, write a harness-style log."""
    s = inst["eval_script"]
    cmd = s.split(": '>>>>> Start Test Output'")[1].split(": '>>>>> End Test Output'")[0].strip()
    test_files = re.findall(r"^diff --git a/(\S+)", inst["test_patch"], re.M)
    sh(["git", "checkout", inst["base_commit"], "--", *test_files], cwd=repo, check=False)
    applied = subprocess.run(["git", "apply", "-"], cwd=repo, input=inst["test_patch"], text=True, capture_output=True)
    env = {**os.environ, "PATH": f"{venv}/bin:{os.environ['PATH']}", "VIRTUAL_ENV": str(venv), "LANG": "en_US.UTF-8",
           "LC_ALL": "en_US.UTF-8", "LANGUAGE": "en_US:en", "PYTHONIOENCODING": "utf8"}
    try:
        p = subprocess.run(["bash", "-c", cmd], cwd=repo, env=env, text=True, capture_output=True, timeout=timeout)
        out, rc = p.stdout + p.stderr, p.returncode
    except subprocess.TimeoutExpired:
        out, rc = ">>>>> Tests Timed Out", -1
    # Django prints a docstring test as "test_x (mod.Class)\n<docstring> ... ok"; the dataset names such tests
    # sometimes by method, sometimes by docstring (env-dependent), so emit the status under both names.
    out = re.sub(r"^(test\w+ \([\w.]+\))\n([^\n]*?) \.\.\. ([^\n]*)$", r"\1 ... \3\n\2 ... \3", out, flags=re.M)
    head = ">>>>> Applied Patch\n" if applied.returncode == 0 else ">>>>> Patch Apply Failed\n" + applied.stderr
    log_path.write_text(f"{head}>>>>> Start Test Output\n{out}\n>>>>> End Test Output\n>>>>> Test Exit Code: {rc}\n")


def grade(inst, patch, log_path):
    from swebench.harness.grading import get_eval_report
    from swebench.harness.utils import make_test_spec
    pred = {"instance_id": inst["instance_id"], "model_name_or_path": "x", "model_patch": patch}
    return get_eval_report(make_test_spec(inst), pred, str(log_path), True)[inst["instance_id"]]


def apply_patch(repo, patch):
    for cmd in (["git", "apply", "-"], ["git", "apply", "--reject", "-"], ["patch", "--batch", "--fuzz=5", "-p1"]):
        if subprocess.run(cmd, cwd=repo, input=patch, text=True, capture_output=True).returncode == 0:
            return True
    return False


def local_eval(inst, patch, dest):
    repo, venv = checkout(inst, dest)
    log = dest / "test_output.txt"
    if patch.strip() and not apply_patch(repo, patch):
        log.write_text(">>>>> Patch Apply Failed\n")
        return {"resolved": False, "patch_successfully_applied": False}
    run_tests(inst, repo, venv, log)
    return grade(inst, patch, log)


def pressure_ok():
    # macOS: 1 normal, 2 warn, 4 critical. This machine has frozen under pressure before.
    for _ in range(15):
        if int(sh(["sysctl", "-n", "kern.memorystatus_vm_pressure_level"]).stdout) < 2:
            return True
        print("  memory pressure elevated, waiting 60s", flush=True)
        time.sleep(60)
    return False


# ---------------------------------------------------------------- select

def cmd_select(a):
    rows = load_dataset_rows()
    pool = sorted((r for r in rows if (not a.repos or r["repo"] in a.repos) and (not a.difficulty or r["difficulty"] in a.difficulty)),
                  key=lambda r: r["instance_id"])
    random.Random(a.seed).shuffle(pool)
    chosen, considered = [], []
    for inst in pool:
        if len(chosen) == a.n:
            break
        entry = {k: inst[k] for k in ("instance_id", "repo", "version", "difficulty")}
        if a.gold_check:
            print(f"gold check {inst['instance_id']}", flush=True)
            try:
                rep = local_eval(inst, inst["patch"], WORK / "goldcheck" / inst["instance_id"])
                entry["gold_resolved_locally"] = bool(rep["resolved"])
                if rep["resolved"]:  # and the unpatched repo must NOT pass, or the local eval is vacuous
                    entry["unpatched_resolved_locally"] = bool(local_eval(inst, "", WORK / "goldcheck" / (inst["instance_id"] + "_unpatched"))["resolved"])
                    entry["gold_resolved_locally"] = not entry["unpatched_resolved_locally"]
            except subprocess.CalledProcessError as e:
                entry["gold_resolved_locally"], entry["setup_error"] = False, (e.stderr or "")[-500:]
            # The checkout carries the reference fix, and agents search the disk for other
            # copies of the file they are fixing (django-10554 ran `find /` for compiler.py).
            shutil.rmtree(WORK / "goldcheck" / inst["instance_id"], ignore_errors=True)
            print(f"  -> {entry['gold_resolved_locally']}", flush=True)
        considered.append(entry)
        if entry.get("gold_resolved_locally", True):
            chosen.append(inst["instance_id"])
    out = {"dataset": DATASET, "revision": REVISION, "seed": a.seed, "n": a.n, "repos": a.repos, "difficulty": a.difficulty,
           "gold_check": a.gold_check, "pool_size": len(pool),
           "procedure": "filter -> sort by instance_id -> random.Random(seed).shuffle -> walk in order, "
                        "keep an instance if (no gold check) or its gold patch resolves under local eval and the unpatched repo does not, stop at n",
           "instance_ids": chosen, "considered": considered}
    Path(a.out).parent.mkdir(parents=True, exist_ok=True)
    Path(a.out).write_text(json.dumps(out, indent=1))
    print(json.dumps(chosen))


# ---------------------------------------------------------------- run

def stats():
    with urllib.request.urlopen(PROXY + "/stats", timeout=15) as r:
        d = json.load(r)
    return {"requests_total": d["requests"]["total"], "by_model": d["requests"]["by_model"],
            "tokens_saved": d["tokens"]["saved"], "tokens_before": d["tokens"]["proxy_total_before_compression"],
            "tokens_input": d["tokens"]["input"]}


def savings_rows(offset, t0, t1, model):
    rows = []
    with open(SAVINGS_EVENTS, "rb") as f:
        if f.seek(0, 2) >= offset:
            f.seek(offset)
        else:  # rotated
            f.seek(0)
        for line in f:
            try:
                r = json.loads(line)
            except ValueError:
                continue
            ts = datetime.fromisoformat(r["ts"]).timestamp()
            if r.get("model") == model and t0 <= ts <= t1:
                rows.append(r)
    return rows


def watch_conns(pid, seen, stop):
    while not stop.is_set():
        out = subprocess.run(["lsof", "-nP", "-a", "-p", str(pid), "-iTCP", "-sTCP:ESTABLISHED", "-Fn"],
                             text=True, capture_output=True).stdout
        seen.update(l.split("->", 1)[1] for l in out.splitlines() if l.startswith("n") and "->" in l)
        stop.wait(3)


def usage_from_stream(lines):
    msgs, result = {}, None
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "assistant":
            m = ev["message"]
            msgs[m["id"]] = m.get("usage") or {}
        elif ev.get("type") == "result":
            result = ev
    keys = {"input": "input_tokens", "cache_creation": "cache_creation_input_tokens",
            "cache_read": "cache_read_input_tokens", "output": "output_tokens"}
    if result and result.get("modelUsage"):
        mu = result["modelUsage"].values()
        u = {"input": sum(m["inputTokens"] for m in mu), "cache_creation": sum(m["cacheCreationInputTokens"] for m in mu),
             "cache_read": sum(m["cacheReadInputTokens"] for m in mu), "output": sum(m["outputTokens"] for m in mu),
             "source": "result.modelUsage"}
    else:
        u = {k: sum(m.get(v) or 0 for m in msgs.values()) for k, v in keys.items()}
        u["source"] = "stream (no result event; output undercounts the last streaming message)"
    return u, len(msgs), result


def run_one(inst, arm, a, out_dir):
    out_dir.mkdir(parents=True, exist_ok=True)
    repo, venv = checkout(inst, WORK / "runs" / a.name / inst["instance_id"] / arm)
    prompt = PROMPT.format(repo=repo, venv=venv, problem=inst["problem_statement"])
    (out_dir / "prompt.txt").write_text(prompt)
    url = ARM_URL[arm]
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("CLAUDE", "ANTHROPIC_BASE_URL", "HEADROOM_", "VIRTUAL_ENV"))}
    env.update(ANTHROPIC_BASE_URL=url, PATH=f"{venv}/bin:{env['PATH']}", VIRTUAL_ENV=str(venv), PIP_REQUIRE_VIRTUALENV="1")
    cmd = [CLAUDE, "-p", prompt, "--model", a.model, "--effort", a.effort, "--output-format", "stream-json", "--verbose",
           "--permission-mode", "acceptEdits", "--allowedTools", TOOLS, "--max-turns", str(a.max_turns),
           "--no-session-persistence", "--settings", json.dumps({"env": {"ANTHROPIC_BASE_URL": url}})]
    s0, off, t0 = stats(), SAVINGS_EVENTS.stat().st_size, time.time()
    with open(out_dir / "stream.jsonl", "w") as so, open(out_dir / "stderr.log", "w") as se:
        p = subprocess.Popen(cmd, cwd=repo, env=env, stdout=so, stderr=se, stdin=subprocess.DEVNULL, start_new_session=True)
        conns, stop = set(), threading.Event()
        threading.Thread(target=watch_conns, args=(p.pid, conns, stop), daemon=True).start()
        try:
            p.wait(timeout=a.timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(p.pid, signal.SIGTERM)
            try:
                p.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(p.pid, signal.SIGKILL)
                p.wait()
        stop.set()
    t1 = time.time()
    time.sleep(2)  # let the proxy flush its savings event for the last request
    s1 = stats()
    sh(["git", "add", "-A"], cwd=repo)
    patch = sh(["git", "diff", "--cached", inst["base_commit"], "--", ".", *TEST_PATHSPECS], cwd=repo).stdout
    all_changed = sh(["git", "diff", "--cached", "--name-only", inst["base_commit"]], cwd=repo).stdout.split()
    (out_dir / "patch.diff").write_text(patch)
    usage, api_calls, result = usage_from_stream((out_dir / "stream.jsonl").read_text().splitlines())
    hr = savings_rows(off, t0, t1 + 2, a.model)
    rec = {
        "instance_id": inst["instance_id"], "arm": arm, "model": a.model, "effort": a.effort, "base_url": url,
        "started": datetime.fromtimestamp(t0, timezone.utc).isoformat(), "wall_s": round(t1 - t0, 1),
        "exit_code": p.returncode, "timed_out": timed_out, "api_calls": api_calls, "usage": usage,
        "total_input": usage["input"] + usage["cache_creation"] + usage["cache_read"],
        "cost_usd": (result or {}).get("total_cost_usd"), "num_turns": (result or {}).get("num_turns"),
        "duration_ms": (result or {}).get("duration_ms"), "subtype": (result or {}).get("subtype"),
        "is_error": (result or {}).get("is_error"), "permission_denials": len((result or {}).get("permission_denials") or []),
        "patch_bytes": len(patch), "changed_files": all_changed,
        "routing": {
            "tcp_remotes_seen": sorted(conns),
            "touched_headroom_port": any(c.endswith(PROXY_PORT) for c in conns),
            "stats_model_requests_delta": s1["by_model"].get(a.model, 0) - s0["by_model"].get(a.model, 0),
            "stats_requests_total_delta": s1["requests_total"] - s0["requests_total"],
            "stats_tokens_saved_delta_all_traffic": s1["tokens_saved"] - s0["tokens_saved"],
            "stats_tokens_before_delta_all_traffic": s1["tokens_before"] - s0["tokens_before"],
        },
        "headroom_events": {"requests": len(hr), "before": sum(r["before"] for r in hr), "after": sum(r["after"] for r in hr),
                            "saved": sum(r["saved"] for r in hr)},
    }
    (out_dir / "run.json").write_text(json.dumps(rec, indent=1))
    r = rec["routing"]
    print(f"  {inst['instance_id']} {arm}: {rec['wall_s']}s exit={p.returncode} timeout={timed_out} calls={api_calls} "
          f"stats_delta={r['stats_model_requests_delta']} port6767={r['touched_headroom_port']} patch={len(patch)}B", flush=True)
    return rec


def cmd_run(a):
    sel = json.loads(Path(a.instances).read_text())
    ids = sel["instance_ids"][: a.limit] if a.limit else sel["instance_ids"]
    rows = {r["instance_id"]: r for r in load_dataset_rows()}
    base = RESULTS / a.name
    base.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(PROXY + "/health", timeout=15) as h:
        hv = json.load(h).get("version")
    (base / "config.json").write_text(json.dumps({**vars(a), "claude_version": sh([CLAUDE, "--version"]).stdout.strip(),
                                                   "headroom_version": hv,
                                                   "instance_ids": ids}, indent=1))
    for i, iid in enumerate(ids):
        arms = list(a.arms) if i % 2 == 0 else list(reversed(a.arms))  # alternate order to balance cache/time effects
        todo = [arm for arm in arms if a.force or not (base / iid / arm / "run.json").exists()]
        if not todo:
            continue
        if not pressure_ok():
            sys.exit("memory pressure stayed elevated, stopping")
        print(f"[{i + 1}/{len(ids)}] {iid} arms={todo}", flush=True)
        if a.parallel > 1 and len(todo) > 1:
            ts = [threading.Thread(target=run_one, args=(rows[iid], arm, a, base / iid / arm)) for arm in todo]
            [t.start() for t in ts]
            [t.join() for t in ts]
        else:
            for arm in todo:
                run_one(rows[iid], arm, a, base / iid / arm)


# ---------------------------------------------------------------- eval / report

def cmd_eval(a):
    base = RESULTS / a.name
    rows = {r["instance_id"]: r for r in load_dataset_rows()}
    for rj in sorted(base.glob("*/*/run.json")):
        d = rj.parent
        if (d / "eval.json").exists() and not a.force:
            continue
        inst = rows[rj.parent.parent.name]
        patch = (d / "patch.diff").read_text()
        print(f"eval {inst['instance_id']} {d.name}", flush=True)
        if patch.strip():
            dest = WORK / "eval" / a.name / inst["instance_id"] / d.name
            rep = local_eval(inst, patch, dest)
            shutil.copy(dest / "test_output.txt", d)
        else:
            rep = {"resolved": False, "empty_patch": True}
        (d / "eval.json").write_text(json.dumps(rep, indent=1))
        print(f"  resolved={rep['resolved']}", flush=True)


def cmd_report(a):
    base = RESULTS / a.name
    runs = [json.loads(p.read_text()) for p in sorted(base.glob("*/*/run.json"))]
    hdr = "| instance | arm | resolved | input | cache_create | cache_read | output | total_in | cost $ | turns | api calls | wall s |"
    lines = [hdr, "|" + "---|" * (hdr.count("|") - 1)]
    tot = {}
    for r in runs:
        ev = base / r["instance_id"] / r["arm"] / "eval.json"
        res = json.loads(ev.read_text())["resolved"] if ev.exists() else "not evaluated"
        u = r["usage"]
        lines.append(f"| {r['instance_id']} | {r['arm']} | {res} | {u['input']:,} | {u['cache_creation']:,} | {u['cache_read']:,} | "
                     f"{u['output']:,} | {r['total_input']:,} | {r['cost_usd'] or 0:.2f} | {r['num_turns']} | {r['api_calls']} | {r['wall_s']:.0f} |")
        t = tot.setdefault(r["arm"], {"runs": 0, "resolved": 0, "input": 0, "cache_creation": 0, "cache_read": 0, "output": 0,
                                      "total_input": 0, "cost_usd": 0.0, "wall_s": 0.0, "hr_before": 0, "hr_saved": 0})
        t["runs"] += 1
        t["resolved"] += res is True
        for k in ("input", "cache_creation", "cache_read", "output"):
            t[k] += u[k]
        t["total_input"] += r["total_input"]
        t["cost_usd"] += r["cost_usd"] or 0
        t["wall_s"] += r["wall_s"]
        t["hr_before"] += r["headroom_events"]["before"]
        t["hr_saved"] += r["headroom_events"]["saved"]
    lines += ["", "| arm | runs | resolved | input | cache_create | cache_read | output | total_in | cost $ | wall min | headroom saved/before |",
              "|---|---|---|---|---|---|---|---|---|---|---|"]
    for arm, t in sorted(tot.items()):
        hrp = f"{t['hr_saved']:,}/{t['hr_before']:,}" if t["hr_before"] else "-"
        lines.append(f"| {arm} | {t['runs']} | {t['resolved']} | {t['input']:,} | {t['cache_creation']:,} | {t['cache_read']:,} | "
                     f"{t['output']:,} | {t['total_input']:,} | {t['cost_usd']:.2f} | {t['wall_s'] / 60:.0f} | {hrp} |")
    if {"control", "treatment"} <= tot.keys():
        c, tr = tot["control"], tot["treatment"]
        lines.append("")
        for k in ("total_input", "output", "cost_usd"):
            lines.append(f"treatment vs control {k}: {100 * (tr[k] - c[k]) / c[k]:+.1f}%" if c[k] else f"{k}: n/a")
    for arm in tot:
        with open(base / f"preds_{arm}.jsonl", "w") as f:
            for r in runs:
                if r["arm"] == arm:
                    f.write(json.dumps({"instance_id": r["instance_id"], "model_name_or_path": f"claude-code_{r['model']}_{arm}",
                                        "model_patch": (base / r["instance_id"] / arm / "patch.diff").read_text()}) + "\n")
    (base / "report.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


def main():
    ap = argparse.ArgumentParser()
    sp = ap.add_subparsers(dest="cmd", required=True)
    s = sp.add_parser("select")
    s.add_argument("--n", type=int, default=5)
    s.add_argument("--seed", type=int, default=0)
    s.add_argument("--repos", nargs="*", default=[])
    s.add_argument("--difficulty", nargs="*", default=[])
    s.add_argument("--gold-check", action="store_true")
    s.add_argument("--out", required=True)
    r = sp.add_parser("run")
    r.add_argument("--instances", required=True)
    r.add_argument("--name", required=True)
    r.add_argument("--arms", nargs="+", default=["control", "treatment"], choices=["control", "treatment"])
    r.add_argument("--model", default="claude-sonnet-5-5")
    r.add_argument("--effort", default="high")
    r.add_argument("--max-turns", type=int, default=100)
    r.add_argument("--timeout", type=int, default=1500)
    r.add_argument("--parallel", type=int, default=1, choices=[1, 2])
    r.add_argument("--limit", type=int, default=0)
    r.add_argument("--force", action="store_true")
    for name in ("eval", "report"):
        e = sp.add_parser(name)
        e.add_argument("--name", required=True)
        e.add_argument("--force", action="store_true")
    a = ap.parse_args()
    {"select": cmd_select, "run": cmd_run, "eval": cmd_eval, "report": cmd_report}[a.cmd](a)


if __name__ == "__main__":
    main()
