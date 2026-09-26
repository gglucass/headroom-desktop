//! Local recomputation of the output-shaper savings estimate.
//!
//! `/stats` reports whatever the backend's `SavingsLedger.best_estimate()`
//! returns, which has two defects we cannot wait on upstream to fix:
//!
//! 1. **Global-mean credit.** A treatment stratum the baseline never observed
//!    falls back to the all-requests mean. The seeded baseline only ever covers
//!    the model family the user ran *before* installing, so every later family
//!    (fable, sonnet, gpt) is scored against one number. On a real ledger that
//!    fallback produced 74% of the reported savings, crediting ~1,010 saved
//!    tokens to `sonnet|new_user_ask|m|notools` requests whose replies average
//!    73 tokens.
//! 2. **Ungated A/B switchover.** `best_estimate` prefers the measured holdout
//!    number as soon as a *single* stratum holds one sample in both arms, which
//!    is how three stale control samples once produced a -1439.9% reduction.
//!
//! We recompute from the same ledger file the proxy writes, which carries every
//! accumulator both estimators need, and apply our own rules: no global-mean
//! credit, and the measured number only wins once it covers a real share of
//! traffic at a usable confidence band.
//!
//! Read-only. The proxy owns this file; we never write it here.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// The measured (A/B holdout) estimate replaces the synthetic-control one only
/// once strata with data in both arms account for this share of treatment
/// volume. Below it the holdout describes a corner of the traffic, not the
/// traffic.
const MEASURED_MIN_COVERAGE_PCT: f64 = 50.0;

/// ...and only once its 95% band is this tight. At a 3% holdout this takes a
/// heavy user a couple of months; a lighter one may never reach it, which is
/// the correct outcome -- they keep the synthetic-control number.
const MEASURED_MAX_CI_HALF_WIDTH_PCT: f64 = 10.0;

/// The synthetic-control estimate is published only once the strata it can
/// score account for this share of shaped traffic. Below it the number
/// describes a corner of the machine and is read as if it described all of it.
/// A fleet snapshot on 2026-09-07 had 23 of 312 reporting users above 90%
/// reduction, which no shaper mechanism produces: it lowers verbosity, it does
/// not delete 19 of every 20 output tokens. Those are thin slices of scoreable
/// traffic whose baseline strata were seeded on a different request class.
///
/// ponytail: provisional value. Coverage only starts travelling to the server
/// in this same change, so there is no fleet distribution to tune against yet;
/// revisit once `output_reduction_coverage_pct` has a fortnight of data.
const ESTIMATED_MIN_COVERAGE_PCT: f64 = 20.0;

/// A baseline stratum speaks for a treatment stratum only with this many
/// observations behind it. Below it, one long pre-install reply becomes the
/// "mean" an entire request class is scored against (a real ledger held
/// `opus|new_user_ask|m|notools` at n=1, mean 849 — and 3,022 quota pings
/// averaging 40 tokens each booked ~809 "saved" against it).
const MIN_BASELINE_N: u64 = 10;

/// Distinct conversations each arm of a stratum needs before its A/B
/// difference counts, mirroring the backend (#3460). A stratum's requests
/// come from one conversation at a time, so five requests from one
/// conversation are one observation, not five.
const MEASURED_MIN_CLUSTERS: usize = 5;

/// Running count / sum / sum-of-squares, mirroring the backend's `_Accum` so
/// mean and variance come out bit-comparable.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Accum {
    n: u64,
    sum: f64,
    sumsq: f64,
    /// Conversation-qualified observations only (upstream #3460, wheel
    /// 0.38.0): legacy rows carry no conversation, so they never enter the
    /// measured mean and cannot clear [`MEASURED_MIN_CLUSTERS`].
    qn: u64,
    qsum: f64,
    qsumsq: f64,
    clusters: Vec<String>,
}

impl Accum {
    fn qmean(&self) -> f64 {
        if self.qn == 0 {
            0.0
        } else {
            self.qsum / self.qn as f64
        }
    }

    fn qvar(&self) -> f64 {
        if self.qn < 2 {
            return 0.0;
        }
        let n = self.qn as f64;
        ((self.qsumsq - self.qsum * self.qsum / n) / (n - 1.0)).max(0.0)
    }

    fn mean(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f64
        }
    }

    /// Sample variance (unbiased); 0 below two observations, as upstream.
    fn var(&self) -> f64 {
        if self.n < 2 {
            return 0.0;
        }
        let n = self.n as f64;
        ((self.sumsq - self.sum * self.sum / n) / (n - 1.0)).max(0.0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Baseline {
    strata: HashMap<String, Accum>,
    /// The all-requests accumulator. Deliberately unused: crediting a stratum
    /// the baseline never saw against the global mean is the overcount this
    /// module exists to remove.
    #[allow(dead_code)]
    glob: Accum,
}

impl Baseline {
    /// `(mean, var, n)` for *key*: the exact stratum only, and only with
    /// [`MIN_BASELINE_N`] observations behind it.
    ///
    /// This used to back off to the longest matching key prefix, merged.
    /// That readmitted the global-mean overcount one level down: an
    /// `opus|new_user_ask|xs|notools` ping (a ~24-token quota check) has no
    /// exact stratum, so it borrowed the merged `opus|new_user_ask|` mean of
    /// 1,611 tokens and booked a 98.7% "reduction" per ping — 49% of one real
    /// ledger's claimed savings, and a permanent "Output −99%" day chip once
    /// pings were the only scored traffic. A verbosity baseline is only
    /// evidence for the stratum it observed; anything else returns `None` and
    /// the request stays out of the estimate.
    fn lookup(&self, key: &str) -> Option<(f64, f64, u64)> {
        let acc = self.strata.get(key)?;
        if acc.n < MIN_BASELINE_N {
            return None;
        }
        Some((acc.mean(), acc.var(), acc.n))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Ledger {
    baseline: Baseline,
    treatment: HashMap<String, Accum>,
    control: HashMap<String, Accum>,
}

impl Ledger {
    /// What to score *key* against: the seeded verbosity baseline where it has
    /// evidence, otherwise this stratum's own control arm.
    ///
    /// The verbosity baseline is learned once, at install, and never relearned
    /// -- every transcript written since is already shaped. So it speaks only
    /// for the strata that machine happened to run beforehand, and no amount
    /// of waiting widens that. On one real ledger it covered 11 opus strata
    /// and 42% of requests, with nothing at all for fable, sonnet or haiku:
    /// more than half the traffic permanently unscoreable, and a day spent
    /// entirely on those models produced no sample at all.
    ///
    /// A control-arm request is unshaped by construction, so it observes the
    /// same counterfactual the baseline was seeded to capture -- in the same
    /// period, from the same client, in exactly this stratum. It is the only
    /// evidence that can ever arrive for a stratum the seeding missed.
    ///
    /// Baseline first and control only where it is silent, so filling the
    /// holdout adds coverage without moving the number for strata that
    /// already had some.
    fn baseline_for(&self, key: &str) -> Option<(f64, f64, u64)> {
        self.baseline.lookup(key).or_else(|| {
            let c = self.control.get(key)?;
            (c.n >= MIN_BASELINE_N).then(|| (c.mean(), c.var(), c.n))
        })
    }

    /// Every shaped request in the ledger, scored or not: the denominator
    /// both coverage gates measure against.
    fn shaped(&self) -> u64 {
        self.treatment.values().map(|acc| acc.n).sum()
    }
}

/// One side of the counterfactual, ready for the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputEstimate {
    /// "estimated" (synthetic control) or "measured" (A/B holdout).
    pub method: &'static str,
    pub reduction_percent: f64,
    pub ci_low_percent: f64,
    pub ci_high_percent: f64,
    /// Treatment requests the estimate actually covers -- not every shaped
    /// request, since strata without baseline evidence are excluded.
    pub requests: u64,
    /// `requests` as a share of every shaped request, so a reader can tell a
    /// number about all of this machine's traffic from one about a corner of
    /// it. Travels to the server with the percentage for the same reason.
    pub coverage_percent: f64,
    pub tokens_saved: u64,
    pub baseline_tokens: u64,
}

/// Path to the proxy's savings ledger. Mirrors the backend's `workspace_dir()`
/// default of `~/.headroom` (neither the proxy nor the seeding run sets
/// `HEADROOM_WORKSPACE_DIR`, so both resolve here).
pub fn ledger_path() -> Option<PathBuf> {
    // The proxy writes this under Python's `Path.home()`, which is the
    // profile folder on Windows whatever `HOME` says.
    let home = crate::client_adapters::home_dir();
    Some(home.join(".headroom").join("output_savings.json"))
}

/// Outcome of one read of the on-disk ledger. `NoEvidence` and `Unscored` are
/// deliberately distinct: a missing, torn, or empty ledger says nothing about
/// this machine's traffic, while a healthy ledger that scores no strata is a
/// positive finding -- the only other number on offer (the backend's
/// global-mean-credited `/stats` figure) is exactly the overcount this module
/// exists to remove, so callers must show nothing there rather than fall back
/// to it. That fallback rendered an Opus-mean vs 4-token-GPT-replies
/// comparison as "Output -100%" on an all-codex Windows machine (0.9.7-rc.7).
#[derive(Debug, Clone, PartialEq)]
pub enum LedgerEstimate {
    /// No ledger, an unparseable (likely mid-write) one, or one with no shaped
    /// traffic yet. Falling back to the backend's figure is harmless here: it
    /// reads the same file, so it cannot know more than we do.
    NoEvidence,
    /// The ledger is healthy and holds shaped traffic, but not one stratum of
    /// it has baseline or control evidence (e.g. an all-gpt machine whose
    /// seeded baseline covers only pre-install claude traffic). Resolves
    /// itself once the holdout's control arm feeds a live stratum past
    /// [`MIN_BASELINE_N`].
    ///
    /// A merely THIN estimate is not this: it stays [`LedgerEstimate::Scored`]
    /// and reports [`OutputEstimate::covers_enough`] false, because this
    /// variant makes the tracker discard every sampled output bucket it holds.
    Unscored,
    Scored(OutputEstimate),
}

impl OutputEstimate {
    /// Whether this estimate speaks for the machine or for a corner of it.
    /// False withholds the PERCENTAGE from the tile and the server report; the
    /// coverage counters still travel, and the dollar series is untouched.
    pub fn covers_enough(&self) -> bool {
        self.coverage_percent >= ESTIMATED_MIN_COVERAGE_PCT
    }
}

impl LedgerEstimate {
    pub fn scored(self) -> Option<OutputEstimate> {
        match self {
            LedgerEstimate::Scored(estimate) => Some(estimate),
            _ => None,
        }
    }
}

/// Best available estimate from the on-disk ledger.
///
/// Cheap enough to call per dashboard build (a few KB of JSON), and tolerant of
/// a torn read: the backend writes this file with a plain `write_text`, so a
/// concurrent flush can hand us a truncated document. That reads as
/// [`LedgerEstimate::NoEvidence`] and the caller keeps its previous value
/// rather than showing a wrong one.
pub fn estimate() -> LedgerEstimate {
    let Some(path) = ledger_path() else {
        return LedgerEstimate::NoEvidence;
    };
    match std::fs::read(path) {
        Ok(bytes) => estimate_from_bytes(&bytes),
        Err(_) => LedgerEstimate::NoEvidence,
    }
}

fn estimate_from_bytes(bytes: &[u8]) -> LedgerEstimate {
    let Ok(ledger) = serde_json::from_slice::<Ledger>(bytes) else {
        return LedgerEstimate::NoEvidence;
    };
    let estimated = estimate_from_baseline(&ledger);
    let best = match measured_if_ready(&ledger, &estimated) {
        Some(measured) => Some(measured),
        None => estimated,
    };
    if let Some(best) = best {
        return LedgerEstimate::Scored(best);
    }
    if ledger.treatment.values().any(|acc| acc.n > 0) {
        LedgerEstimate::Unscored
    } else {
        LedgerEstimate::NoEvidence
    }
}

/// Synthetic control: treatment output against the best counterfactual each
/// stratum has (see [`Ledger::baseline_for`]), over the strata that have one.
///
/// `Σ n·(μ − ȳ)` with `Var ≈ Σ [ n·σ²_y + n²·σ²_μ/m ]`, matching the backend's
/// derivation so the only difference is which strata qualify.
fn estimate_from_baseline(ledger: &Ledger) -> Option<OutputEstimate> {
    let mut saved = 0.0;
    let mut baseline_tokens = 0.0;
    let mut var = 0.0;
    let mut requests = 0u64;

    for (key, acc) in &ledger.treatment {
        if acc.n == 0 {
            continue;
        }
        let Some((mu, mu_var, m)) = ledger.baseline_for(key) else {
            continue;
        };
        let n = acc.n as f64;
        requests += acc.n;
        saved += n * (mu - acc.mean());
        baseline_tokens += n * mu;
        var += n * acc.var() + (n * n) * (mu_var / m as f64);
    }

    finalize(
        "estimated",
        saved,
        baseline_tokens,
        var,
        requests,
        ledger.shaped(),
    )
}

/// A/B measurement: per-stratum control mean minus treatment mean, over strata
/// with data in both arms. `None` until the holdout has produced any.
fn estimate_from_holdout(ledger: &Ledger) -> Option<OutputEstimate> {
    let mut saved = 0.0;
    let mut baseline_tokens = 0.0;
    let mut var = 0.0;
    let mut requests = 0u64;

    for (key, t) in &ledger.treatment {
        let Some(c) = ledger.control.get(key) else {
            continue;
        };
        if t.qn == 0 || c.qn == 0 {
            continue;
        }
        if t.clusters.len() < MEASURED_MIN_CLUSTERS || c.clusters.len() < MEASURED_MIN_CLUSTERS {
            continue;
        }
        let n = t.qn as f64;
        requests += t.qn;
        saved += n * (c.qmean() - t.qmean());
        baseline_tokens += n * c.qmean();
        var += (n * n) * (c.qvar() / c.qn as f64 + t.qvar() / t.qn as f64);
    }

    finalize(
        "measured",
        saved,
        baseline_tokens,
        var,
        requests,
        ledger.shaped(),
    )
}

/// The measured estimate, but only once it is worth showing: it must cover
/// [`MEASURED_MIN_COVERAGE_PCT`] of the shaped traffic and carry a band no wider
/// than [`MEASURED_MAX_CI_HALF_WIDTH_PCT`].
///
/// Coverage is measured against the same denominator the synthetic control
/// uses, so a holdout only displaces an estimate it genuinely outgrew.
fn measured_if_ready(
    ledger: &Ledger,
    estimated: &Option<OutputEstimate>,
) -> Option<OutputEstimate> {
    let measured = estimate_from_holdout(ledger)?;
    if measured.coverage_percent < MEASURED_MIN_COVERAGE_PCT {
        return None;
    }
    let half_width = (measured.ci_high_percent - measured.ci_low_percent) / 2.0;
    if half_width > MEASURED_MAX_CI_HALF_WIDTH_PCT {
        return None;
    }
    // Never trade a usable estimate for a measurement of less traffic.
    if estimated
        .as_ref()
        .is_some_and(|e| e.requests > measured.requests)
    {
        return None;
    }
    Some(measured)
}

/// Percent + 95% normal-approximation band, or `None` when the result is not
/// one we can honestly display (no covered requests, a degenerate baseline, or
/// a >100% blowup from a near-zero one — the dashboard once rendered such a
/// blowup as "Output --6,130.7%").
///
/// A NEGATIVE result floors to 0% instead of returning `None`. "No reduction"
/// is an answer, not an error: discarding it meant a holdout measuring ~0
/// could never replace the estimate, and a `None` here demotes a scoreable
/// ledger to [`LedgerEstimate::Unscored`], hiding a tile whose honest value is
/// zero. The CI is left unclamped so the gate in [`measured_if_ready`] still
/// sees the real band.
fn finalize(
    method: &'static str,
    saved: f64,
    baseline_tokens: f64,
    var: f64,
    requests: u64,
    shaped: u64,
) -> Option<OutputEstimate> {
    if requests == 0 || shaped == 0 || baseline_tokens <= 0.0 || !baseline_tokens.is_finite() {
        return None;
    }
    let pct = saved / baseline_tokens * 100.0;
    if !pct.is_finite() || pct > 100.0 {
        return None;
    }
    let se = var.max(0.0).sqrt();
    Some(OutputEstimate {
        method,
        reduction_percent: pct.max(0.0),
        ci_low_percent: (saved - 1.96 * se) / baseline_tokens * 100.0,
        ci_high_percent: (saved + 1.96 * se) / baseline_tokens * 100.0,
        requests,
        coverage_percent: requests as f64 / shaped as f64 * 100.0,
        tokens_saved: saved.max(0.0).round() as u64,
        baseline_tokens: baseline_tokens.round() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline covers `opus` only; treatment also ran `fable`. Mirrors the
    /// real shape of a seeded ledger.
    const MIXED: &str = r#"{
      "baseline": {
        "strata": {
          "opus|ask|l|tools":  {"n": 10, "sum": 10000, "sumsq": 10200000},
          "opus|ask|xl|tools": {"n": 5,  "sum": 10000, "sumsq": 20500000}
        },
        "glob": {"n": 15, "sum": 20000, "sumsq": 30700000}
      },
      "treatment": {
        "opus|ask|l|tools":   {"n": 4, "sum": 3200, "sumsq": 2580000},
        "opus|ask|l|notools": {"n": 2, "sum": 1500, "sumsq": 1130000},
        "fable|ask|l|tools":  {"n": 3, "sum": 2100, "sumsq": 1490000}
      },
      "control": {}
    }"#;

    /// One stratum, a healthy holdout on it.
    const HOLDOUT: &str = r#"{
      "baseline": {
        "strata": {"opus|ask|l|tools": {"n": 100, "sum": 95000, "sumsq": 90490000}},
        "glob":   {"n": 100, "sum": 95000, "sumsq": 90490000}
      },
      "treatment": {"opus|ask|l|tools": {"n": 1000, "sum": 800000, "sumsq": 649990000,
                                         "qn": 1000, "qsum": 800000, "qsumsq": 649990000,
                                         "clusters": ["c1", "c2", "c3", "c4", "c5"]}},
      "control":   {"opus|ask|l|tools": {"n": 100,  "sum": 100000, "sumsq": 100990000,
                                         "qn": 100,  "qsum": 100000, "qsumsq": 100990000,
                                         "clusters": ["c1", "c2", "c3", "c4", "c5"]}}
    }"#;

    /// `MIXED` with a control arm swapped in.
    fn mixed_with_control(control: &str) -> String {
        MIXED.replace("\"control\": {}", &format!("\"control\": {control}"))
    }

    fn with_control(control: &str) -> String {
        let idx = HOLDOUT
            .find("\"control\"")
            .expect("fixture has a control arm");
        format!("{}{control}\n    }}", &HOLDOUT[..idx])
    }

    fn estimate(json: &str) -> OutputEstimate {
        estimate_from_bytes(json.as_bytes())
            .scored()
            .expect("estimate")
    }

    #[test]
    fn excludes_strata_the_baseline_never_saw() {
        let e = estimate(MIXED);
        assert_eq!(e.method, "estimated");
        // Only the 4 opus|ask|l|tools requests have an exact, well-fed
        // baseline stratum. The 2 opus|ask|l|notools (no exact stratum) and
        // the 3 fable requests (no evidence at all) stay out.
        assert_eq!(e.requests, 4);
        // 4 of the 9 shaped requests are scoreable.
        assert!((e.coverage_percent - 44.444_444).abs() < 1e-5);
        assert_eq!(e.tokens_saved, 800);
        assert_eq!(e.baseline_tokens, 4000);
        assert!((e.reduction_percent - 20.0).abs() < 1e-9);
        assert!((e.ci_low_percent - 7.777_253).abs() < 1e-5);
        assert!((e.ci_high_percent - 32.222_747).abs() < 1e-5);
    }

    #[test]
    fn a_stratum_the_baseline_missed_is_scored_against_its_own_control_arm() {
        // fable has no baseline stratum and never can -- the verbosity
        // baseline was seeded once, before Headroom shaped anything. Its own
        // control arm is unshaped by construction, so it is the counterfactual.
        let e = estimate(&mixed_with_control(
            r#"{"fable|ask|l|tools": {"n": 10, "sum": 10000, "sumsq": 10200000}}"#,
        ));
        assert_eq!(e.method, "estimated");
        // 4 opus (seeded baseline, mu 1000 vs 800) + 3 fable (control mean
        // 1000 vs 700). The 2 opus|ask|l|notools still have neither.
        assert_eq!(e.requests, 7);
        assert_eq!(e.tokens_saved, 1_700);
        assert_eq!(e.baseline_tokens, 7_000);

        // A thin control arm is not evidence: same floor as the baseline.
        let thin = estimate(&mixed_with_control(
            r#"{"fable|ask|l|tools": {"n": 9, "sum": 9000, "sumsq": 9180000}}"#,
        ));
        assert_eq!(thin.requests, 4);
        assert_eq!(thin.tokens_saved, 800);
    }

    #[test]
    fn lookup_is_exact_stratum_only_with_enough_evidence() {
        let ledger: Ledger = serde_json::from_str(MIXED).expect("parse");
        // Exact stratum with n >= MIN_BASELINE_N qualifies.
        let (mu, _var, n) = ledger
            .baseline
            .lookup("opus|ask|l|tools")
            .expect("exact hit");
        assert_eq!((mu, n), (1000.0, 10));
        // No prefix backoff: an xs ping must not borrow the merged opus|ask|
        // mean (the "Output -99%" day-chip artifact).
        assert!(ledger.baseline.lookup("opus|ask|l|notools").is_none());
        assert!(ledger.baseline.lookup("opus|ask|xs|notools").is_none());
        // Exact stratum below MIN_BASELINE_N is not evidence either.
        assert!(ledger.baseline.lookup("opus|ask|xl|tools").is_none());
        assert!(ledger.baseline.lookup("fable|ask|l|tools").is_none());
    }

    #[test]
    fn measured_replaces_estimated_once_it_is_solid() {
        let e = estimate(HOLDOUT);
        assert_eq!(e.method, "measured");
        assert!((e.reduction_percent - 20.0).abs() < 1e-9);
        assert_eq!(e.requests, 1000);
    }

    #[test]
    fn a_thin_control_arm_never_takes_over() {
        // Three-sample-control territory: the point estimate looks fine (20%)
        // but the band spans -257%..297%, so the synthetic control stands.
        let e = estimate(&with_control(
            r#""control": {"opus|ask|l|tools": {"n": 2, "sum": 2000, "sumsq": 6000000,
                                               "qn": 2, "qsum": 2000, "qsumsq": 6000000,
                                               "clusters": ["c1", "c2", "c3", "c4", "c5"]}}"#,
        ));
        assert_eq!(e.method, "estimated");
        assert!((e.reduction_percent - 15.789_474).abs() < 1e-5);
    }

    #[test]
    fn a_holdout_covering_a_corner_of_traffic_never_takes_over() {
        // Same tight holdout, but a second stratum carries 4/5 of the shaped
        // requests and has no control samples at all.
        let json = HOLDOUT.replace(
            r#""treatment": {"opus|ask|l|tools": {"n": 1000, "sum": 800000, "sumsq": 649990000,"#,
            r#""treatment": {"opus|ask|xl|tools": {"n": 4000, "sum": 4000000, "sumsq": 4009990000},
                             "opus|ask|l|tools": {"n": 1000, "sum": 800000, "sumsq": 649990000,"#,
        ).replace(
            r#""strata": {"opus|ask|l|tools": {"n": 100, "sum": 95000, "sumsq": 90490000}}"#,
            r#""strata": {"opus|ask|l|tools": {"n": 100, "sum": 95000, "sumsq": 90490000},
                          "opus|ask|xl|tools": {"n": 100, "sum": 120000, "sumsq": 144990000}}"#,
        );
        let e = estimate(&json);
        assert_eq!(e.method, "estimated");
        assert_eq!(e.requests, 5000);
    }

    #[test]
    fn an_estimate_covering_a_corner_of_traffic_is_not_publishable() {
        // The 90-100% fleet tail: a well-fed baseline stratum scores 4 of 104
        // shaped requests, and the percentage gets read as if it described the
        // machine. It stays SCORED -- demoting it to Unscored would make the
        // tracker discard the machine's sampled output buckets -- but it is
        // not publishable.
        let json = MIXED.replace(
            r#""fable|ask|l|tools":  {"n": 3, "sum": 2100, "sumsq": 1490000}"#,
            r#""fable|ask|l|tools":  {"n": 98, "sum": 68600, "sumsq": 48706000}"#,
        );
        let thin = estimate(&json);
        assert_eq!(thin.requests, 4);
        assert!((thin.coverage_percent - 3.846_153).abs() < 1e-5);
        assert!(!thin.covers_enough());

        // Its own control arm restores coverage, and with it the headline.
        let scored = estimate(&json.replace(
            r#""control": {}"#,
            r#""control": {"fable|ask|l|tools": {"n": 10, "sum": 10000, "sumsq": 10200000}}"#,
        ));
        // 102 of 104: the 2 opus|ask|l|notools still have no evidence.
        assert_eq!(scored.requests, 102);
        assert!((scored.coverage_percent - 98.076_923).abs() < 1e-5);
        assert!(scored.covers_enough());
    }

    #[test]
    fn shaped_traffic_with_no_baseline_evidence_is_unscored_not_no_evidence() {
        // The exact state of an all-codex machine: shaped gpt traffic against
        // a claude-seeded baseline. Unscored -- NOT NoEvidence -- so callers
        // hide the number instead of falling back to the backend's
        // global-mean-credited one (the Windows "Output -100%" chip).
        let json = r#"{"baseline": {"strata": {}, "glob": {"n": 15, "sum": 20000, "sumsq": 30700000}},
                       "treatment": {"fable|ask|l|tools": {"n": 3, "sum": 2100, "sumsq": 1490000}}}"#;
        assert_eq!(
            estimate_from_bytes(json.as_bytes()),
            LedgerEstimate::Unscored
        );
    }

    #[test]
    fn a_torn_absent_or_traffic_free_ledger_carries_no_evidence() {
        // Unreadable documents and ledgers without shaped traffic say nothing
        // about this machine, so the backend fallback stays available there.
        assert_eq!(estimate_from_bytes(b""), LedgerEstimate::NoEvidence);
        assert_eq!(
            estimate_from_bytes(br#"{"baseline": {"strata": {"opus|ask|l|to"#),
            LedgerEstimate::NoEvidence
        );
        assert_eq!(estimate_from_bytes(b"{}"), LedgerEstimate::NoEvidence);
        let seeded_but_idle = r#"{
          "baseline": {"strata": {"opus|ask|l|tools": {"n": 10, "sum": 10000, "sumsq": 10200000}}, "glob": {"n": 10, "sum": 10000, "sumsq": 10200000}},
          "treatment": {"opus|ask|l|tools": {"n": 0, "sum": 0, "sumsq": 0}}
        }"#;
        assert_eq!(
            estimate_from_bytes(seeded_but_idle.as_bytes()),
            LedgerEstimate::NoEvidence
        );
    }

    #[test]
    fn a_shaper_that_made_replies_longer_floors_at_zero() {
        // Treatment above baseline gives a negative percent, which the tile
        // used to render as "Output --6,130.7%". It floors to an honest 0%
        // rather than vanishing: reading Unscored here hides a tile whose
        // honest value is zero.
        let json = r#"{
          "baseline": {"strata": {"opus|ask|l|tools": {"n": 10, "sum": 1000, "sumsq": 110000}}, "glob": {"n": 0, "sum": 0, "sumsq": 0}},
          "treatment": {"opus|ask|l|tools": {"n": 4, "sum": 3200, "sumsq": 2580000}}
        }"#;
        let e = estimate_from_bytes(json.as_bytes())
            .scored()
            .expect("floored estimate");
        assert_eq!(e.method, "estimated");
        assert_eq!(e.reduction_percent, 0.0);
        assert_eq!(e.tokens_saved, 0);
    }

    /// Upstream #3460 (wheel 0.38.0): an arm written before conversations
    /// were tracked, or one whose requests all came from a few conversations,
    /// is not evidence -- five requests from one session are one observation.
    /// The estimate must stay "estimated" so the holdout boost keeps feeding
    /// control traffic until both arms really are conversation-diverse.
    #[test]
    fn a_holdout_short_on_conversations_never_takes_over() {
        let legacy = estimate(&with_control(
            r#""control": {"opus|ask|l|tools": {"n": 100, "sum": 100000, "sumsq": 100990000}}"#,
        ));
        assert_eq!(legacy.method, "estimated");
        let few = estimate(&with_control(
            r#""control": {"opus|ask|l|tools": {"n": 100, "sum": 100000, "sumsq": 100990000,
                                               "qn": 100, "qsum": 100000, "qsumsq": 100990000,
                                               "clusters": ["c1", "c2", "c3", "c4"]}}"#,
        ));
        assert_eq!(few.method, "estimated");
    }

    #[test]
    fn a_solid_holdout_measuring_no_reduction_still_takes_over() {
        // Control replies come out SHORTER than treatment: the true effect is
        // ~zero-or-negative. Once the holdout is solid (full coverage, tight
        // band) that honest zero must replace the synthetic estimate -- the
        // old [0,100] validity check discarded it, so a shaper that saved
        // nothing kept its estimated percentage forever.
        let e = estimate(&with_control(
            r#""control": {"opus|ask|l|tools": {"n": 100, "sum": 70000, "sumsq": 49010000,
                                               "qn": 100, "qsum": 70000, "qsumsq": 49010000,
                                               "clusters": ["c1", "c2", "c3", "c4", "c5"]}}"#,
        ));
        assert_eq!(e.method, "measured");
        assert_eq!(e.reduction_percent, 0.0);
        assert!(e.ci_high_percent < 0.0, "band stays unclamped for the gate");
    }
}
