# Upstream PR review fixes (headroomlabs-ai/headroom)

Maintainer review follow-ups for our two open upstream PRs. They live here
because this session could not push to the fork that hosts the PR branches
(`gglucass/headroom-labs`) -- the git proxy only authorizes
`gglucass/headroom-desktop`. Nothing in this directory is built or imported by
the desktop app; it is a transport for work that belongs upstream, and should
be deleted once both PRs carry the commits.

## PR #3386 - fix(stats): gate the output-shaping claim on the shaper being active

Fork branch: `fix/stats-output-shaping-inactive-gate` (head `781c27f`)

The reviewer showed `/stats` still publishing an old shaping claim when
effective steering is off: with the shaper enabled and `HEADROOM_VERBOSITY_LEVEL=0`,
`shape_request(...).changed` is False yet the endpoint returned `active=true`,
`method=measured`, `reduction_percent=20`. `_shaper_active` was set from
`OutputShaperSettings.enabled` -- the config gate -- before the level resolved,
and `/stats` never applied `steering_allowed_for(proxy.config)`.

`/stats` now resolves the shaper exactly as the request handlers do and takes
the layer active only when the resolved level is above 0. That covers all four
zero paths: explicit env level, learned profile, autotune controller, and cache
mode. Adds endpoint-level regressions through the real route (the previous unit
tests pinned `_output_reduction_payload` given a boolean, so they could not
catch the endpoint passing the wrong one), plus a positive control so the gate
cannot pass by being vacuously off.

Also merges upstream `main`, which the PR needed anyway: both sides had added a
module-level `/stats` helper at the same spot in `server.py`
(`_output_reduction_payload` vs `_code_syntax_breaker_status`). Both kept, no
logic changed on either side.

## PR #3460 - fix(output-savings): count conversations, not requests, in the holdout gate

Fork branch: `fix/output-savings-holdout-clusters` (head `cb54c97`)

The reviewer showed the cluster gate admitting unlabelled legacy traffic:
loading a stratum with 2,500 unattributed requests per arm and then recording
five matched conversations per arm flipped the estimate from `None` to
`measured -99.80% over 2,505 treatment requests`, while the labelled
conversations underneath showed no difference at all. The gate was a
per-stratum admission test, so whatever cleared it also admitted everything
already sitting in that stratum's mean and variance.

Each arm accumulator is now split in two. The totals keep every observation and
keep feeding the estimated tier; a separate `qualified` sub-accumulator holds
only observations that arrived with a conversation label, carries the cluster
set, and is the only view `estimate_from_holdout` reads. Provenance becomes
structural rather than a check: unattributed traffic has nowhere to land in the
measured path, so it can neither ride in behind a stratum that later qualifies
nor be added to one that already has. Pre-upgrade ledgers deserialize with
`qualified=None` and stay out of the measured estimate permanently, since their
sums cannot be split after the fact.

## Applying

Preferred -- the bundle carries the exact history, merge commit included:

    git remote add fork https://github.com/gglucass/headroom-labs   # if needed
    git fetch upstream-pr-review/upstream-review-fixes.bundle 'refs/heads/*:refs/remotes/rev/*'
    git push fork rev/pr3460:fix/output-savings-holdout-clusters
    git push fork rev/pr3386:fix/stats-output-shaping-inactive-gate

Or apply the patches onto the PR heads directly (each verified to apply
cleanly). The #3386 patch is the review fix only -- redo the `main` merge as
described above, or take it from the bundle.

    git am upstream-pr-review/0001-*.patch

## Verification

Run against the wheel's compiled `headroom._core` (`uv pip install
headroom-ai==0.37.0`, copy `_core.abi3.so` into the checkout) with the dev
extra, notably `httpx[http2]` -- without `h2` the TestClient suites fail at app
startup for reasons unrelated to these changes.

- #3460: 69 passed in `tests/test_output_savings.py`; 151 passed across the
  output-savings and output-shaper suites.
- #3386: 11 passed in `tests/test_stats_output_reduction_gate.py`. The four new
  endpoint regressions were confirmed to FAIL on the unfixed commit and pass
  with it. 143 passed across the related suites; 454 passed over every
  `/stats`-touching test file.
- `ruff check` and `ruff format --check` clean on every file touched.
- Two failures in `tests/test_compression_observability.py` (smart-crusher
  observer) are pre-existing on the untouched PR head and unrelated.
