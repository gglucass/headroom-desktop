# SWE-bench A/B: Claude Code with and without Headroom

One question: does routing Claude Code through Headroom (the local proxy on
`127.0.0.1:6767`) change whether the agent solves real coding tasks, and how many
tokens does it save while doing so?

Each task is run twice with identical inputs, once per arm:

| arm | `ANTHROPIC_BASE_URL` | what it is |
|---|---|---|
| control | `https://api.anthropic.com` | Claude Code straight to Anthropic |
| treatment | `http://127.0.0.1:6767` | Claude Code through Headroom (whatever the running desktop app's proxy does) |

Outcome = SWE-bench "resolved" (all FAIL_TO_PASS and PASS_TO_PASS tests pass with the
agent's patch). Cost = the token usage Claude Code reports per run (input,
cache-creation, cache-read, output, list-price `total_cost_usd`), which is what
Anthropic billed, so the treatment arm's numbers are already post-compression.

## Files

- `bench.py` - the whole harness (select, run, eval, report). Run it with `.venv/bin/python`.
- `results/<name>/instances.json` - the chosen instance ids and exactly how they were chosen.
- `results/<name>/<instance>/<arm>/` - `prompt.txt`, `stream.jsonl` (full Claude Code
  transcript), `stderr.log`, `patch.diff`, `run.json` (usage, cost, turns, routing proof,
  Headroom per-request savings), `eval.json` and `test_output.txt` after eval.
- `results/<name>/report.md`, `preds_control.jsonl`, `preds_treatment.jsonl` - after `report`.
- Work trees, mirrors and per-run venvs live in `~/.cache/headroom-swebench/` (override with
  `SWEBENCH_WORK`). They are deliberately OUTSIDE this repo: Claude Code loads every
  `CLAUDE.md` from the cwd up to `/`, so a work dir under `bench/` would hand the agent
  headroom-desktop's own CLAUDE.md and CLAUDE.local.md.

## Setup

```bash
cd bench/swebench
uv venv --python 3.12 .venv
uv pip install -p .venv/bin/python swebench datasets
```

Dataset: `SWE-bench/SWE-bench_Verified` at HF revision `78f471bf...` (pinned in `bench.py`).
It has the same 500 instance ids, base commits, gold patches, test patches and problem
statements as `princeton-nlp/SWE-bench_Verified` (checked), plus the `eval_script`,
`image` and `log_parser` columns that swebench 5.x's harness requires.

## How a run works

Per task and arm:

1. Fresh checkout at `base_commit`: `git init` + `git fetch --depth 1 <local bare mirror> <base_commit>`.
   No remote, no history past the base commit, so the agent cannot `git log` its way to the
   real fix. Mirrors are `git clone --bare` from GitHub, one per repo, reused.
2. A fresh per-run venv (`uv venv`, repo installed editable; Python 3.8/3.9/3.11 for Django
   by version, 3.9 + mpmath for SymPy). Its `bin` is first on `PATH`, `VIRTUAL_ENV` points
   at it, `PIP_REQUIRE_VIRTUALENV=1` blocks installs into the system Python, and the
   prompt names it. Verified that the agent's Bash tool sees all three.
3. `claude -p <prompt>` in the checkout with:
   `--model claude-sonnet-5-5 --effort high --output-format stream-json --verbose
   --permission-mode acceptEdits --allowedTools Read,Edit,Write,Grep,Glob,Bash
   --max-turns 100 --no-session-persistence --settings '{"env":{"ANTHROPIC_BASE_URL":"<arm url>"}}'`,
   with `ANTHROPIC_BASE_URL=<arm url>` also set in the process env, and every inherited
   `CLAUDE*`/`HEADROOM_*` variable stripped. Wall-clock timeout 25 min (process group killed).
   `stream-json` instead of `json`: its final `result` event is the same object `json` prints,
   and the rest is the full trajectory for debugging.
4. Patch = `git add -A && git diff --cached <base_commit>` minus test files
   (`tests/`, `test/`, `test_*.py`, `*_test.py`, `*_tests.py`, `conftest.py`, `__pycache__`).
   `run.json` also lists every changed file, test files included.
5. The prompt is the SWE-bench/OpenHands style one: issue text, "make minimal changes to
   non-test files", "do not modify tests", plus the venv path. See `PROMPT` in `bench.py`.

Arms run one after the other per task, alternating which goes first (`--parallel 2` runs both
at once; this machine has frozen under memory pressure, so the pilot ran sequentially).
Before each task the runner waits for macOS memory pressure to be normal and stops if it
stays elevated for 15 minutes.

### Routing verification (per run, in `run.json` -> `routing`)

- `tcp_remotes_seen`: the runner polls `lsof` on the claude process every 3s.
  Treatment must show `127.0.0.1:6767`; control must not, and shows Anthropic's address.
- `stats_model_requests_delta`: `/stats` `requests.by_model[<model>]` before vs after. The
  pilot model (`claude-sonnet-5-5`) is otherwise unused on the pilot machine (its everyday
  sessions run opus; the auto-mode classifier shows up as `claude-sonnet-5`), so with sequential runs
  this counter is a clean per-run signal: 0 for control, about the API call count for
  treatment. With `--parallel 2` it is only clean for the pair, not per arm.
- `headroom_events`: the proxy's own per-request rows in `~/.headroom/savings_events.jsonl`
  for the run's time window and model (requests, tokens before/after, saved). Control must
  have none.

The control override (`--settings '{"env":{"ANTHROPIC_BASE_URL":"https://api.anthropic.com"}}'`)
works on Claude Code 2.1.289: flag settings outrank `~/.claude/settings.json`'s
`env.ANTHROPIC_BASE_URL`. Smoke-tested before the pilot: a control call left the
`claude-sonnet-5-5` counter unchanged, a treatment call moved it by 1.

### What is NOT isolated (same in both arms)

The operator's global Claude Code config loads in both arms: `~/.claude/CLAUDE.md` (and RTK.md),
hooks (Read/markitdown, Bash/rtk rewrite, the Headroom SessionStart route guard, which only
reports), plugins (headroom, ponytail, ruby-lsp), MCP servers (headroom plus the claude.ai
connectors: 132 tools at init, deferred behind tool search), `ENABLE_TOOL_SEARCH=true`.
Adding `--strict-mcp-config` to the claude command would drop the MCP servers in both arms.
That adds tokens and some behaviour to both arms equally, but it means absolute numbers are
for the operator's Claude Code, not a vanilla one. `--bare` would strip it but only works with an
API key, not the subscription.

The treatment arm is the Headroom proxy exactly as the desktop app runs it (0.39.0 at pilot
time, coding profile, output shaper on). Record the version with each run set.

## Evaluation

Two paths:

1. Official harness (Docker, x86_64 images, slow under emulation on Apple Silicon):
   ```bash
   .venv/bin/python -m swebench.harness.run_evaluation \
     --dataset_name SWE-bench/SWE-bench_Verified \
     --predictions_path results/<name>/preds_<arm>.jsonl \
     --run_id <name>_<arm> --max_workers 1 --report_dir results/<name>
   ```
   Or SWE-bench's cloud evaluator `sb-cli` (`pip install sb-cli`, then
   `sb-cli submit swe-bench_verified test --predictions_path ... --run_id ...`), which needs
   a free API key (`sb-cli gen-api-key <email>`).
2. Local, no Docker (`bench.py eval`): a fresh checkout + venv, apply the agent's patch,
   apply the gold test patch, run the instance's own test command taken from its
   `eval_script`, then grade with swebench's own log parser and `get_eval_report`. It is not
   the official environment (macOS arm64, newer dependency versions), so `select
   --gold-check` keeps only instances whose GOLD patch resolves under this local eval;
   an instance that cannot pass with the reference fix is never used.

## Reproduce

```bash
cd bench/swebench
# pick instances (pilot: 5, seed 0, light repos, easy/medium, gold must pass locally)
.venv/bin/python bench.py select --n 5 --seed 0 --repos django/django sympy/sympy \
  --difficulty "<15 min fix" "15 min - 1 hour" --gold-check --out results/pilot/instances.json
.venv/bin/python bench.py run --instances results/pilot/instances.json --name pilot
.venv/bin/python bench.py eval --name pilot
.venv/bin/python bench.py report --name pilot
```

`run` skips runs that already have a `run.json` (`--force` to redo), so it can be resumed.

## Pilot results

Run 2026-10-05, Claude Code 2.1.289, Headroom 0.39.0, `claude-sonnet-5-5`, effort high,
sequential, local eval. Instances (seed 0, django+sympy, "<15 min fix" or "15 min - 1 hour",
gold must resolve and the unpatched repo must not under local eval; pool 277):
`django__django-13670, sympy__sympy-23413, django__django-13964, sympy__sympy-19346, django__django-12273`
(all five happen to be "15 min - 1 hour"). Official Docker eval was NOT run (see below).

| instance | arm | resolved | input | cache_create | cache_read | output | total_in | cost $ | turns | api calls | wall s |
|---|---|---|---|---|---|---|---|---|---|---|---|
| django__django-12273 | control | False | 26 | 31,633 | 462,204 | 8,043 | 493,863 | 0.30 | 13 | 13 | 118 |
| django__django-12273 | treatment | False | 22 | 32,070 | 377,183 | 8,724 | 409,275 | 0.29 | 11 | 11 | 164 |
| django__django-13670 | control | True | 6 | 21,793 | 78,030 | 628 | 99,829 | 0.11 | 4 | 3 | 16 |
| django__django-13670 | treatment | True | 8 | 32,862 | 97,347 | 598 | 130,217 | 0.16 | 4 | 4 | 12 |
| django__django-13964 | control | True | 6 | 22,986 | 79,489 | 944 | 102,481 | 0.12 | 5 | 3 | 15 |
| django__django-13964 | treatment | True | 1,963 | 22,433 | 109,571 | 991 | 133,967 | 0.13 | 6 | 4 | 15 |
| sympy__sympy-19346 | control | True | 10 | 23,513 | 146,627 | 1,738 | 170,150 | 0.14 | 7 | 5 | 19 |
| sympy__sympy-19346 | treatment | True | 10 | 23,789 | 142,671 | 1,883 | 166,470 | 0.14 | 8 | 5 | 24 |
| sympy__sympy-23413 | control | True | 16 | 29,178 | 269,283 | 3,395 | 298,477 | 0.20 | 8 | 8 | 37 |
| sympy__sympy-23413 | treatment | True | 14,473 | 60,288 | 278,561 | 5,184 | 353,322 | 0.38 | 10 | 10 | 56 |

| arm | runs | resolved | input | cache_create | cache_read | output | total_in | cost $ | wall min | headroom saved/before |
|---|---|---|---|---|---|---|---|---|---|---|
| control | 5 | 4 | 64 | 129,103 | 1,035,633 | 14,748 | 1,164,800 | 0.87 | 3 | - |
| treatment | 5 | 4 | 16,476 | 171,442 | 1,005,333 | 17,380 | 1,193,251 | 1.09 | 4 | 14,872/764,282 |

treatment vs control total_input: +2.4%
treatment vs control output: +17.8%
treatment vs control cost_usd: +25.5%

`total_in` = input + cache_create + cache_read (what Anthropic counted). `cost $` is Claude
Code's list-price `total_cost_usd`. "headroom saved/before" is the proxy's own per-request
accounting for the treatment runs (its "before" excludes the ~11k-token tool block).

n=5 proves nothing statistically. What the pilot does show:

- Routing held in all 10 runs: every treatment run's `/stats` model counter moved by exactly
  its API call count (4, 10, 4, 5, 11) and its process held a connection to `127.0.0.1:6767`;
  every control run moved it by 0, never touched 6767, and has no Headroom rows. (Treatment
  processes also open connections to Anthropic's IP; that is Claude Code's non-inference
  traffic: OAuth/account, claude.ai MCP connectors, telemetry.)
- Same outcome per task in both arms (4/5 each; django-12273 failed identically in both).
- These tasks are short (3-13 API calls), so there is little tool output to compress: Headroom
  saved 14,872 tokens, 1.2% of the treatment arm's would-be input.
- Treatment cost +25% overall, almost all from one run. In sympy-23413 (treatment) the proxy
  log shows `event=cache_breakpoints` WARNINGs (in_messages=2 out_messages=1: one message
  cache breakpoint dropped) on calls 3-7, so new turns were sent uncached (cache_read frozen at
  32,501 while uncached input grew 1.3k -> 4.8k), then on call 8 the first CCR compression
  injected the `headroom_retrieve` tool and switched to a buffered non-streaming request, and
  Headroom logged `CACHE-BUST: expected_cached=32,501 actual_read=0 tokens_lost=32,501
  tokens_saved=1,231`. django-13964 (treatment) had one more uncached-tail call. Excluding
  sympy-23413 the arms cost $0.67 vs $0.72. Worth a look independently of this benchmark.
- Both cache problems were fixed in 0.9.34 as sitecustomize vendors: `ccr_tool_eager` (the
  retrieve tool enters on a session's first request, upstream #3810) and `held_read_breakpoint`
  (read_maturation keeps the client's tail breakpoint). Rerun on 0.9.34-rc.7, treatment only
  (`results/pilot-rc6/`): 5/5 resolved, uncached input 16,476 -> 52 tokens, cache writes
  171k -> 138k (control 129k), $1.09 -> $0.82 (control $0.87). Most of that cost drop is one
  task (django-12273 took 5 calls instead of 11-13); without it the arms are $0.57 control,
  $0.68 treatment. Headroom saved 2.4% of input on these short tasks.
- The treatment's first call reads a slightly shorter shared prefix from cache (10,776 vs
  11,903 tokens) because the output shaper rewrites part of the system prompt.

To point the treatment arm at another Headroom (for example a scratch proxy running a
candidate sitecustomize), set `HEADROOM_BENCH_PROXY=http://127.0.0.1:<port>`; routing checks
follow that port.

## Full run

Not started: it spends the operator's Claude subscription quota, so it needs an explicit go-ahead.

Recommended: N=100 tasks drawn with `select --seed 0 --gold-check` from the django+sympy pool
(306 of the 500 Verified tasks; drop `--difficulty` to include the 28 "1-4 hours" ones, which
are the long sessions where compression actually has material to work on), both arms,
`claude-sonnet-5-5`. Then submit both prediction files to the official evaluator (sb-cli, or
Docker on a machine with memory to spare) for the headline numbers, keeping the local eval as
a cross-check.

```bash
.venv/bin/python bench.py select --n 100 --seed 0 --repos django/django sympy/sympy --gold-check --out results/full/instances.json
.venv/bin/python bench.py run --instances results/full/instances.json --name full   # add --parallel 2 to halve wall time
.venv/bin/python bench.py eval --name full && .venv/bin/python bench.py report --name full
```

Why django+sympy only: the agent needs a working Python env to run tests, and those two
install in seconds with `uv` on arm64. astropy, matplotlib, scikit-learn, xarray and sphinx
need native builds or old pinned stacks; covering all 12 repos means running the agent inside
the SWE-bench x86_64 images, which this 16 GB machine cannot do comfortably. The A/B delta
does not need all repos; an absolute resolve rate comparable to vexp's would.

What N buys: the comparison is paired (same task, both arms). Agent runs are stochastic, so
expect 10-20% of tasks to flip between arms even with no real effect; with N=100 the 95% CI on
the resolve-rate difference is about +-8 percentage points, with N=277 (the whole
easy/medium pool) about +-5. Report tokens and cost as paired per-task differences
(median and bootstrap CI), not just totals: per-task cost ratios in the pilot ranged
0.97x-1.9x.

Estimate for N=100 (200 runs), extrapolated from the pilot (mean 236k tokens, $0.20 list,
~55 s per run including setup):

| | tokens processed | list-price equivalent | wall, sequential | wall, `--parallel 2` |
|---|---|---|---|---|
| tasks like the pilot | ~47M (about 90% cache reads) | ~$40 | ~3 h | ~1.5 h |
| realistic mix (3-5x longer sessions) | 150-250M | $120-200 | 8-15 h | 4-8 h |

The pilot tasks were all solved in under 3 minutes; failing and harder tasks run to the
100-turn / 25-minute caps, so plan on the second row. A second model (e.g. opus) doubles the
run count at a higher price per token. Before a long run: check `sysctl vm.swapusage`, close
heavy apps, and keep `--parallel` at 2 at most.
