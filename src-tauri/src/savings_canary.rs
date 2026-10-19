//! Zero-savings canary.
//!
//! The 2026-08-21 Codex CLI 0.149.0 regression (tools moved out of the
//! top-level `tools` array into `additional_tools` input items) ran 7+ peak
//! hours before a human spotted it on the admin page. Nothing errored:
//! requests forwarded and streamed normally, they just compressed to exactly
//! zero. Every alarm we had was error-based or fleet-aggregated, and the
//! server-side per-client rate canary that looks like the obvious answer
//! cannot see it - codex is almost never a user's only client, so its
//! dominant-client cohort was n=1 on the day of the incident.
//!
//! The signature is local and per-request: a large request that ran through
//! the compression pipeline and saved nothing. This module reads it off the
//! `/transformations/feed` batch `run_activity_observation` already polls, and
//! reports once per process to Sentry, where events aggregate across the
//! fleet on a fixed fingerprint. One machine reporting is a lead; a hundred
//! is a graph.
//!
//! `detect_basis_violation` is the second canary here, for the opposite
//! failure: savings that are too LARGE to be true. See its banner.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use crate::models::TransformationFeedEvent;

/// Requests below this are too small to draw a conclusion from - a short turn
/// with nothing compressible legitimately saves zero.
const MIN_INPUT_TOKENS: u64 = 10_000;
/// Qualifying requests needed before the ratio means anything. The observer
/// pulls 150 events per pass, so a busy machine clears this easily and a
/// barely-used one stays quiet instead of paging on three samples.
const MIN_SAMPLE: usize = 20;
/// Healthy traffic scatters; a wire-format change zeroes essentially all of
/// it. Anything short of near-total is noise, not a regression.
const ZERO_RATIO: f64 = 0.9;

/// One report per process. A wedged client produces the same finding on every
/// observer pass, and the point is to learn that it happened, not how often.
static REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq)]
pub struct Anomaly {
    /// Large, compression-eligible requests examined.
    pub sample: usize,
    /// How many of those saved nothing.
    pub zero: usize,
    /// Distinct output-shaper strata seen on the zero-saved requests. The
    /// `|notools` suffix is what named the Codex regression.
    pub strata: Vec<String>,
    /// Distinct `provider/model` pairs, to point at which client broke.
    pub models: Vec<String>,
    /// Distinct transform families that ran on the zero-saved requests, with
    /// per-request parameters trimmed off. This is the discriminator the first
    /// ten reports lacked: a wire-format regression leaves the router with
    /// nothing to match (`router:noop` alone next to the output shaper), while
    /// a compressor that ran and returned nothing shows its real stages. Every
    /// report so far spanned every provider at once, which no single client's
    /// wire format can explain -- the transforms say which of the two it was.
    pub transforms: Vec<String>,
}

/// Pick out the anomaly, or `None` when the batch looks healthy or is too
/// thin to judge. Pure: `observe` owns the reporting side effects.
pub fn detect(events: &[TransformationFeedEvent]) -> Option<Anomaly> {
    // An empty transform list means the pipeline never ran (bypass header,
    // optimize disabled, unlicensed). Those save zero by design and must not
    // page - only requests that were actually compressed count.
    let considered: Vec<&TransformationFeedEvent> = events
        .iter()
        .filter(|e| {
            !e.transforms_applied.is_empty()
                && e.input_tokens_original.unwrap_or(0) >= MIN_INPUT_TOKENS
        })
        .collect();

    if considered.len() < MIN_SAMPLE {
        return None;
    }

    let zeroed: Vec<&&TransformationFeedEvent> = considered
        .iter()
        .filter(|e| e.tokens_saved.unwrap_or(0) <= 0)
        .collect();

    if (zeroed.len() as f64) < considered.len() as f64 * ZERO_RATIO {
        return None;
    }

    // BTreeSet: deduped and ordered, so the Sentry extras are stable across
    // machines instead of reshuffling per batch.
    let mut strata = BTreeSet::new();
    let mut models = BTreeSet::new();
    let mut transforms = BTreeSet::new();
    for event in &zeroed {
        for transform in &event.transforms_applied {
            if let Some(stratum) = transform.strip_prefix("output_shaper:stratum:") {
                strata.insert(stratum.to_string());
                continue;
            }
            // First two segments only: `router:tool_search_deferral:9tools:
            // 14341tok` carries per-request counts that would make every
            // machine's set unique and the fleet view unreadable.
            transforms.insert(
                transform
                    .split(':')
                    .take(2)
                    .collect::<Vec<&str>>()
                    .join(":"),
            );
        }
        let provider = event.provider.as_deref().unwrap_or("?");
        let model = event.model.as_deref().unwrap_or("?");
        models.insert(format!("{provider}/{model}"));
    }

    Some(Anomaly {
        sample: considered.len(),
        zero: zeroed.len(),
        strata: strata.into_iter().take(5).collect(),
        models: models.into_iter().take(5).collect(),
        transforms: transforms.into_iter().take(6).collect(),
    })
}

/// Report a detected anomaly to Sentry, at most once per process.
pub fn observe(events: &[TransformationFeedEvent]) {
    let Some(anomaly) = detect(events) else {
        return;
    };
    if REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }

    let strata = anomaly.strata.join(", ");
    let models = anomaly.models.join(", ");
    let transforms = anomaly.transforms.join(", ");
    // Fixed fingerprint: every affected machine lands in one issue, so the
    // event count is the blast radius. Counts stay out of it deliberately.
    let fingerprint: [&str; 1] = ["zero_savings_canary"];
    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "zero_savings_canary");
            scope.set_extra("sample", (anomaly.sample as u64).into());
            scope.set_extra("zero_saved", (anomaly.zero as u64).into());
            // NOT "min_input_tokens": Sentry's data scrubber nulls any extra
            // whose KEY contains "token", so the threshold this canary is
            // measured against arrived empty on every event (RUST-A5).
            scope.set_extra("min_input_toks", MIN_INPUT_TOKENS.into());
            scope.set_extra("strata", strata.clone().into());
            scope.set_extra("models", models.clone().into());
            scope.set_extra("transforms", transforms.clone().into());
            scope.set_fingerprint(Some(fingerprint.as_slice()));
        },
        || {
            sentry::capture_message(
                &format!(
                    "zero_savings_canary: {}/{} large requests compressed to nothing \
                     (models: {models}; strata: {strata}; transforms: {transforms})",
                    anomaly.zero, anomaly.sample
                ),
                sentry::Level::Warning,
            );
        },
    );
    log::warn!(
        "zero-savings canary: {}/{} requests over {MIN_INPUT_TOKENS} tokens saved nothing \
         (models: {models}; strata: {strata}; transforms: {transforms})",
        anomaly.zero,
        anomaly.sample
    );
}

// ── Savings-basis canary ─────────────────────────────────────────────────
//
// Every input-savings percentage the app shows divides compression savings by
// the new input those savings came out of (the INVARIANT banner on
// `parse_headroom_stats_from_json` in state.rs). That only means anything
// while the numerator is a SUBSET of the denominator. Savings taken off
// content that never entered as new input have nothing to divide by, so they
// push the rate toward 100% instead of measuring anything.
//
// It stopped holding on the OpenAI `/v1/responses` path. Codex re-sends the
// whole transcript every turn and the router recompresses all of it, so the
// same removed history is booked again on every turn while the denominator
// counts only what newly reached the provider. Measured 2026-09-08 across one
// machine's retained proxy logs: gpt/codex models reported 8.10M compression
// savings against 3.90M new input (2.1x, an impossible share, displayed as
// 67.5%), where Claude reported 4.18M against 43.3M -- its frozen cached
// prefix keeps removals inside new content, which is why its rate is honest.
// The fleet reads the same: codex-dominant user-days at 26.3%, claude-code at
// 4.0%.
//
// Repairing the numerator is upstream work in the router, which is the only
// layer that knows whether a removal is first-time or a re-run of one it
// already counted. This is the tripwire for the class: savings that outrun
// the new input they are divided by cannot mean what the label says, and we
// want to know which machines and how many.
//
// ponytail: machine-level, not per-provider. `/stats` breaks new input out by
// provider but never breaks savings out, so a mixed machine whose codex slice
// is inflated can still average under the threshold and stay quiet -- it fires
// on the codex-dominant machines, which is where the displayed number is most
// wrong (71 users / 150 user-days in the 21 days to 2026-09-08). Make it
// per-provider if and when a wheel reports savings by provider.

/// New input below this is too little to judge -- a freshly started backend
/// has a cold prefix cache, where savings legitimately sit close to the whole
/// (uncached) payload.
const BASIS_MIN_NEW_INPUT: u64 = 100_000;
/// Savings are counted on Headroom's tokenizer, new input on the provider's,
/// so the two scales disagree by a few percent even when the basis is sound.
/// Only a numerator that OUTRUNS its denominator is structurally impossible.
const BASIS_MAX_RATIO: f64 = 1.2;

/// One report per process, same reasoning as the zero-savings canary: this
/// re-detects on every `/stats` poll and the point is that it happened.
static BASIS_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq)]
pub struct BasisViolation {
    /// Compression-only savings (`tokens.saved` less tool-schema deferral,
    /// which rides the cached prefix and is excluded from the rate too).
    pub compression_saved: u64,
    /// `prefix_cache` new input: cache writes plus uncached input.
    pub new_input: u64,
    /// `compression_saved / new_input`. Anything above 1.0 is impossible.
    pub ratio: f64,
    /// Each provider's share of new input, biggest first ("openai:82%").
    /// Savings are not broken out per provider anywhere in `/stats`, so this
    /// is what points triage at the responsible path.
    pub providers: Vec<String>,
}

fn u64_at(root: &Value, path: &[&str]) -> Option<u64> {
    let mut current = root;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_u64()
}

/// Read the violation off a `/stats` body, or `None` when the basis holds or
/// the payload cannot answer. Pure: `observe_basis` owns the reporting.
pub fn detect_basis_violation(stats_body: &str) -> Option<BasisViolation> {
    let root: Value = serde_json::from_str(stats_body).ok()?;

    // Absent `prefix_cache.totals` means the backend reports no cache
    // dimension at all; the displayed rate falls back to forwarded tokens
    // there, which is a different (and bounded) basis. Nothing to check.
    let totals = root.get("prefix_cache")?.get("totals")?;
    let new_input = totals.get("cache_write_tokens").and_then(Value::as_u64)?
        + totals
            .get("uncached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    if new_input < BASIS_MIN_NEW_INPUT {
        return None;
    }

    // Mirror the displayed numerator exactly, including the tool-schema
    // subtraction -- a canary on a different figure than the one on screen
    // would fire on the wrong machines.
    let saved = u64_at(&root, &["tokens", "saved"])?;
    let tool_schema = u64_at(
        &root,
        &["summary", "compression", "tool_schema_tokens_saved"],
    )
    .or_else(|| {
        u64_at(
            &root,
            &["savings", "by_layer", "tool_search", "tokens_saved"],
        )
    })
    .unwrap_or(0);
    let compression_saved = saved.saturating_sub(tool_schema);

    let ratio = compression_saved as f64 / new_input as f64;
    if ratio <= BASIS_MAX_RATIO {
        return None;
    }

    let mut providers: Vec<(u64, String)> = root
        .get("prefix_cache")
        .and_then(|cache| cache.get("by_provider"))
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(name, stats)| {
                    let share = stats
                        .get("cache_write_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        + stats
                            .get("uncached_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                    (share, name.clone())
                })
                .collect()
        })
        .unwrap_or_default();
    providers.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    Some(BasisViolation {
        compression_saved,
        new_input,
        ratio,
        providers: providers
            .into_iter()
            .take(3)
            .map(|(share, name)| format!("{name}:{}%", share * 100 / new_input.max(1)))
            .collect(),
    })
}

/// Report a basis violation to Sentry, at most once per process.
pub fn observe_basis(stats_body: &str) {
    let Some(violation) = detect_basis_violation(stats_body) else {
        return;
    };
    if BASIS_REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }

    let providers = violation.providers.join(", ");
    // Fixed fingerprint, like the zero-savings canary: the fleet-wide event
    // count is the blast radius, and the ratio must not fragment the group.
    let fingerprint: [&str; 1] = ["savings_basis_canary"];
    sentry::with_scope(
        |scope| {
            scope.set_tag("flow", "savings_basis_canary");
            scope.set_extra("compression_saved", violation.compression_saved.into());
            scope.set_extra("new_input", violation.new_input.into());
            scope.set_extra("ratio", format!("{:.2}", violation.ratio).into());
            scope.set_extra("new_input_by_provider", providers.clone().into());
            scope.set_fingerprint(Some(fingerprint.as_slice()));
        },
        || {
            sentry::capture_message(
                &format!(
                    "savings_basis_canary: compression savings are {:.2}x the new input they \
                     are divided by (new input: {providers})",
                    violation.ratio
                ),
                sentry::Level::Warning,
            );
        },
    );
    log::warn!(
        "savings-basis canary: {} compression tokens saved against {} new input ({:.2}x); \
         the displayed input-savings rate is on a broken basis (new input: {providers})",
        violation.compression_saved,
        violation.new_input,
        violation.ratio
    );
}

// --- Compression-quarantine canary -----------------------------------------
//
// The third canary here, for a failure the other two cannot see: compression
// that never ran at all, on requests that reported no error to the user.
//
// Python cannot preempt an executor thread, so when one compression outruns
// `COMPRESSION_TIMEOUT_SECONDS` (30s) the worker keeps running and the backend
// arms a timeout-debt quarantine: EVERY subsequent compression raises
// `CompressionQuarantinedError` until that worker exits or
// `HEADROOM_COMPRESSION_QUARANTINE_MAX_SECONDS` (60s) lapses. One slow call
// therefore costs a minute of compression across all traffic. Measured on one
// machine 2026-09-21: a single 34s Kompress ONNX inference (188 words, saving
// 41 tokens) starved 125 of 2,336 requests -- 5.35% forwarded uncompressed,
// silently, since a quarantined request still succeeds.
//
// This reads the backend's own counters off `/metrics` rather than inferring
// the condition, and reports once per process on the fixed fingerprint the
// other two use: one machine is a lead, a hundred is a graph. It exists to
// answer whether the 5.35% is one host or the fleet, and goes quiet on its own
// once the backend stops failing quarantined requests outright.

/// Starved requests needed before a report means anything. A quarantine that
/// caught a handful of requests during one slow inference is the mechanism
/// working as designed; a persistent one is the bug.
const QUARANTINE_MIN_SKIPS: u64 = 20;
/// ...and they must be a real share of traffic. Absolute counts alone would
/// page a long-running backend that accumulated 20 skips over a week, which is
/// noise. Both bars, for the reason the compression rules give: a ratio is not
/// a measurement, and neither is a bare count.
const QUARANTINE_MIN_SHARE: f64 = 0.02;

/// One report per process, same reasoning as the two canaries above.
static QUARANTINE_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq)]
pub struct QuarantineStarvation {
    /// Times a timed-out worker armed the quarantine.
    pub activations: u64,
    /// Requests that were refused compression because of it. These forwarded
    /// uncompressed and returned 200, so nothing else reports them.
    pub skipped: u64,
    /// `headroom_requests_total`, from the same scrape, so the share cannot
    /// break its basis the way a cross-endpoint denominator would.
    pub requests: u64,
    /// `skipped / requests`.
    pub share: f64,
}

/// Read a single-sample Prometheus counter (`name{labels} value`) out of a
/// scrape body. Returns `None` when the series is absent -- which is the
/// normal state, since the backend only emits the quarantine series once it
/// has something to report.
fn counter(body: &str, series: &str) -> Option<u64> {
    body.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let value = line.strip_prefix(series)?;
            // Guard against `foo_total_extra` matching the prefix `foo_total`:
            // a real sample continues with a label block or whitespace.
            if !value.starts_with(['{', ' ']) {
                return None;
            }
            value.rsplit(' ').next()?.trim().parse().ok()
        })
}

/// Read the starvation off a `/metrics` body, or `None` when compression is
/// running normally or the scrape cannot answer. Pure: `observe_quarantine`
/// owns the reporting.
pub fn detect_quarantine_starvation(metrics_body: &str) -> Option<QuarantineStarvation> {
    let skipped = counter(
        metrics_body,
        "headroom_compression_quarantine_total{event=\"skipped\"}",
    )?;
    if skipped < QUARANTINE_MIN_SKIPS {
        return None;
    }
    // A zero denominator means the scrape predates any traffic, which cannot
    // have produced skips; treat it as unanswerable rather than dividing.
    let requests = counter(metrics_body, "headroom_requests_total").filter(|total| *total > 0)?;
    let share = skipped as f64 / requests as f64;
    if share < QUARANTINE_MIN_SHARE {
        return None;
    }
    Some(QuarantineStarvation {
        activations: counter(
            metrics_body,
            "headroom_compression_quarantine_total{event=\"activated\"}",
        )
        .unwrap_or(0),
        skipped,
        requests,
        share,
    })
}

/// The slowest single compressor call this process has made, across the
/// `compressor:*` families in the scrape we already hold.
///
/// This is the number the executor counters cannot give: kompress inference is
/// hard-limited to ONE slot process-wide, and that wait happens INSIDE the
/// executor worker, so `run_seconds_max` counts queueing for the slot and the
/// inference itself as one lump while `queue_wait_seconds_max` stays at zero.
/// A `run_seconds_max` far above this value means workers sat behind the slot;
/// close to it means one inference really is that slow.
fn slowest_compressor_ms(metrics_body: &str) -> Option<f64> {
    metrics_body
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let sample =
                line.strip_prefix("headroom_transform_timing_ms_max{transform=\"compressor:")?;
            sample.rsplit(' ').next()?.trim().parse::<f64>().ok()
        })
        .max_by(f64::total_cmp)
}

/// The compression executor's own counters, read off `/health`
/// (`runtime.compression_executor`). `/metrics` exports the quarantine
/// counters and nothing else, which is why RUST-HD stood for two days with
/// two unfalsifiable causes. These are what separate them: a worker that
/// overran because it waited behind others shows up in
/// `queue_wait_seconds_max`, one that overran on its own inference in
/// `run_seconds_max`, and `max_workers` says how much of the pool the global
/// quarantine was idling while it refused every request.
fn executor_extras(health_body: &str) -> Vec<(&'static str, String)> {
    const KEYS: [&str; 8] = [
        "max_workers",
        "queued_max",
        "queue_timeouts_total",
        "queue_wait_seconds_max",
        "run_seconds_max",
        "run_seconds_total",
        "leaked_threads_total",
        "timed_out_workers_max",
    ];
    let Ok(health) = serde_json::from_str::<serde_json::Value>(health_body) else {
        return Vec::new();
    };
    let Some(executor) = health.pointer("/runtime/compression_executor") else {
        return Vec::new();
    };
    KEYS.iter()
        .filter_map(|key| Some((*key, executor.get(key)?.to_string())))
        .collect()
}

/// Report quarantine starvation to Sentry, at most once per process.
/// `health_body` is the `/health` payload when the caller could fetch one; the
/// report still goes out without it, just without the executor counters.
pub fn observe_quarantine(metrics_body: &str, health_body: Option<&str>) {
    let Some(starved) = detect_quarantine_starvation(metrics_body) else {
        return;
    };
    if QUARANTINE_REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }

    // Requests lost per activation is the number that decides whether this is
    // worth fixing: it is the blast radius of ONE slow inference.
    let per_activation = starved.skipped as f64 / starved.activations.max(1) as f64;
    let executor = health_body.map(executor_extras).unwrap_or_default();
    let fingerprint: [&str; 1] = ["compression_quarantine_canary"];
    sentry::with_scope(
        |scope| {
            for (key, value) in &executor {
                scope.set_extra(&format!("executor_{key}"), value.as_str().into());
            }
            if let Some(slowest) = slowest_compressor_ms(metrics_body) {
                scope.set_extra("slowest_compressor_ms", format!("{slowest:.0}").into());
            }
            scope.set_tag("flow", "compression_quarantine_canary");
            scope.set_extra("activations", starved.activations.into());
            scope.set_extra("skipped", starved.skipped.into());
            scope.set_extra("requests", starved.requests.into());
            scope.set_extra("share", format!("{:.4}", starved.share).into());
            scope.set_extra(
                "skipped_per_activation",
                format!("{per_activation:.1}").into(),
            );
            scope.set_fingerprint(Some(fingerprint.as_slice()));
        },
        || {
            sentry::capture_message(
                &format!(
                    "compression_quarantine_canary: {} of {} requests forwarded with NO \
                     compression ({:.1}%) after {} timeout-debt quarantine(s)",
                    starved.skipped,
                    starved.requests,
                    starved.share * 100.0,
                    starved.activations
                ),
                sentry::Level::Warning,
            );
        },
    );
    log::warn!(
        "compression-quarantine canary: {} of {} requests ({:.1}%) were refused compression \
         after {} quarantine activation(s), {per_activation:.1} requests lost per activation; \
         one slow compression worker starves every request behind it",
        starved.skipped,
        starved.requests,
        starved.share * 100.0,
        starved.activations
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(input_tokens: u64, saved: i64, transforms: &[&str]) -> TransformationFeedEvent {
        TransformationFeedEvent {
            request_id: None,
            timestamp: None,
            provider: Some("openai".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            input_tokens_original: Some(input_tokens),
            input_tokens_optimized: Some(input_tokens.saturating_sub(saved.max(0) as u64)),
            tokens_saved: Some(saved),
            savings_percent: None,
            transforms_applied: transforms.iter().map(|t| t.to_string()).collect(),
            workspace: None,
            turn_id: None,
            uncached_input_tokens: None,
            cache_write_tokens: None,
            cache_read_tokens: None,
        }
    }

    /// The Codex 0.149.0 shape: compression ran, classified `notools`, saved 0.
    fn zeroed_codex_batch(count: usize) -> Vec<TransformationFeedEvent> {
        (0..count)
            .map(|_| {
                event(
                    40_000,
                    0,
                    &[
                        "output_shaper:stratum:gpt|new_user_ask|m|notools",
                        "output_shaper:verbosity:L2",
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn detects_a_fleet_of_large_requests_saving_nothing() {
        let anomaly = detect(&zeroed_codex_batch(MIN_SAMPLE)).expect("anomaly");
        assert_eq!(anomaly.sample, MIN_SAMPLE);
        assert_eq!(anomaly.zero, MIN_SAMPLE);
        assert_eq!(anomaly.strata, vec!["gpt|new_user_ask|m|notools"]);
        assert_eq!(anomaly.models, vec!["openai/gpt-5.6-sol"]);
        assert_eq!(anomaly.transforms, vec!["output_shaper:verbosity"]);
    }

    /// The two shapes the first ten reports could not be told apart by: a
    /// router that matched nothing, versus stages that ran and returned zero.
    /// Per-request parameters are trimmed so the sets collapse across machines.
    #[test]
    fn names_the_transform_families_behind_the_zeroes() {
        let batch: Vec<_> = (0..MIN_SAMPLE)
            .map(|_| {
                event(
                    40_000,
                    0,
                    &[
                        "router:noop",
                        "router:tool_search_deferral:9tools:14341tok",
                        "output_shaper:stratum:gpt|new_user_ask|m|notools",
                    ],
                )
            })
            .collect();
        assert_eq!(
            detect(&batch).expect("anomaly").transforms,
            vec!["router:noop", "router:tool_search_deferral"]
        );
    }

    #[test]
    fn stays_quiet_below_the_sample_floor() {
        assert!(detect(&zeroed_codex_batch(MIN_SAMPLE - 1)).is_none());
    }

    #[test]
    fn stays_quiet_when_compression_is_working() {
        let healthy: Vec<_> = (0..MIN_SAMPLE * 2)
            .map(|_| event(40_000, 608, &["openai:responses:tool_schema_compaction"]))
            .collect();
        assert!(detect(&healthy).is_none());
    }

    /// Passthrough (bypass header, optimize off, unlicensed) saves zero by
    /// design and must never page: the pipeline never ran, so no transforms.
    #[test]
    fn ignores_passthrough_requests() {
        let passthrough: Vec<_> = (0..MIN_SAMPLE * 2).map(|_| event(40_000, 0, &[])).collect();
        assert!(detect(&passthrough).is_none());
    }

    /// Small turns legitimately have nothing to compress.
    #[test]
    fn ignores_small_requests() {
        let small: Vec<_> = (0..MIN_SAMPLE * 2)
            .map(|_| event(MIN_INPUT_TOKENS - 1, 0, &["output_shaper:verbosity:L2"]))
            .collect();
        assert!(detect(&small).is_none());
    }

    /// A minority of zero-saved requests is ordinary scatter, not a break.
    #[test]
    fn tolerates_a_minority_of_zero_saved_requests() {
        let mut mixed = zeroed_codex_batch(4);
        mixed.extend(
            (0..16).map(|_| event(40_000, 900, &["openai:responses:tool_schema_compaction"])),
        );
        assert!(detect(&mixed).is_none());
    }

    /// Missing `tokens_saved` (older proxy, or an outcome that never recorded
    /// one) reads as zero rather than being silently dropped from the count.
    #[test]
    fn treats_absent_tokens_saved_as_zero() {
        let mut batch = zeroed_codex_batch(MIN_SAMPLE);
        for entry in &mut batch {
            entry.tokens_saved = None;
        }
        assert_eq!(detect(&batch).expect("anomaly").zero, MIN_SAMPLE);
    }

    #[test]
    fn observe_reports_at_most_once_per_process() {
        // The static is process-wide, so this asserts the swap contract
        // directly rather than fighting the other tests over Sentry state.
        assert!(!REPORTED.swap(true, Ordering::AcqRel));
        assert!(REPORTED.swap(true, Ordering::AcqRel));
        REPORTED.store(false, Ordering::Release);
    }

    /// `/stats` cut down to the fields the basis canary reads.
    fn stats_body(saved: u64, tool_schema: u64, providers: &[(&str, u64, u64)]) -> String {
        let by_provider: Vec<String> = providers
            .iter()
            .map(|(name, write, uncached)| {
                format!(
                    r#""{name}": {{"cache_write_tokens": {write}, "uncached_input_tokens": {uncached}}}"#
                )
            })
            .collect();
        let write: u64 = providers.iter().map(|(_, w, _)| w).sum();
        let uncached: u64 = providers.iter().map(|(_, _, u)| u).sum();
        format!(
            r#"{{
                "tokens": {{"saved": {saved}}},
                "summary": {{"compression": {{"tool_schema_tokens_saved": {tool_schema}}}}},
                "prefix_cache": {{
                    "totals": {{
                        "cache_write_tokens": {write},
                        "uncached_input_tokens": {uncached}
                    }},
                    "by_provider": {{{}}}
                }}
            }}"#,
            by_provider.join(", ")
        )
    }

    /// The measured 2026-09-08 codex shape: compression savings run at 2.1x
    /// the new input they are divided by, which no share of new content can.
    #[test]
    fn detects_savings_that_outrun_their_own_denominator() {
        let violation = detect_basis_violation(&stats_body(
            8_100_000,
            0,
            &[("openai", 0, 3_900_000), ("anthropic", 0, 100_000)],
        ))
        .expect("violation");
        assert_eq!(violation.compression_saved, 8_100_000);
        assert_eq!(violation.new_input, 4_000_000);
        assert!(
            (violation.ratio - 2.025).abs() < 1e-9,
            "{}",
            violation.ratio
        );
        // Savings are not split per provider anywhere in `/stats`, so the new
        // input mix is what names the path to look at.
        assert_eq!(violation.providers, vec!["openai:97%", "anthropic:2%"]);
    }

    /// The Claude shape from the same machine and the same hour: removals stay
    /// inside new content because the cached prefix is frozen.
    #[test]
    fn stays_quiet_when_savings_come_out_of_new_input() {
        assert!(detect_basis_violation(&stats_body(
            4_180_000,
            0,
            &[("anthropic", 20_000_000, 23_300_000)]
        ))
        .is_none());
    }

    /// Tool-schema deferral rides the cached prefix and is excluded from the
    /// displayed rate; counting it here would fire on tool-heavy Claude
    /// sessions, where the basis is sound.
    #[test]
    fn excludes_tool_schema_deferral_from_the_numerator() {
        // 3.9M all-layers against 300k new input is 13x -- but 3.7M of it is
        // deferral, and the 200k that is compression fits inside new input.
        assert!(detect_basis_violation(&stats_body(
            3_900_000,
            3_700_000,
            &[("anthropic", 100_000, 200_000)]
        ))
        .is_none());
    }

    /// A cold prefix cache legitimately puts savings near the whole payload.
    #[test]
    fn stays_quiet_below_the_new_input_floor() {
        assert!(detect_basis_violation(&stats_body(
            BASIS_MIN_NEW_INPUT * 5,
            0,
            &[("openai", 0, BASIS_MIN_NEW_INPUT - 1)]
        ))
        .is_none());
    }

    /// Tokenizer scale disagreement (local numerator, provider denominator) is
    /// not a basis break.
    #[test]
    fn tolerates_tokenizer_scale_disagreement() {
        assert!(
            detect_basis_violation(&stats_body(1_100_000, 0, &[("openai", 0, 1_000_000)]))
                .is_none()
        );
    }

    /// No `prefix_cache.totals` means the rate is on the forwarded-token
    /// basis, which is bounded by construction. Nothing to say.
    #[test]
    fn ignores_backends_that_report_no_cache_dimension() {
        assert!(detect_basis_violation(r#"{"tokens": {"saved": 9000000}}"#).is_none());
    }

    #[test]
    fn observe_basis_reports_at_most_once_per_process() {
        assert!(!BASIS_REPORTED.swap(true, Ordering::AcqRel));
        assert!(BASIS_REPORTED.swap(true, Ordering::AcqRel));
        BASIS_REPORTED.store(false, Ordering::Release);
    }

    /// Shaped like a real scrape, including the `# HELP`/`# TYPE` comments and
    /// a neighbouring series that shares the `_total` prefix.
    fn metrics_body(activated: u64, skipped: u64, requests: u64) -> String {
        format!(
            "# HELP headroom_requests_total Requests\n\
             # TYPE headroom_requests_total counter\n\
             headroom_requests_total {requests}\n\
             headroom_requests_failed_total 9\n\
             # TYPE headroom_compression_quarantine_total counter\n\
             headroom_compression_quarantine_total{{event=\"activated\"}} {activated}\n\
             headroom_compression_quarantine_total{{event=\"skipped\"}} {skipped}\n"
        )
    }

    /// The 2026-09-21 measurement: 2 activations starved 72 of 327 requests.
    #[test]
    fn reads_starvation_off_a_real_scrape() {
        let starved = detect_quarantine_starvation(&metrics_body(2, 72, 327)).expect("starvation");
        assert_eq!(starved.activations, 2);
        assert_eq!(starved.skipped, 72);
        assert_eq!(starved.requests, 327);
        assert!((starved.share - 72.0 / 327.0).abs() < 1e-9);
    }

    /// A backend that never quarantined emits no such series at all.
    #[test]
    fn stays_quiet_when_compression_never_stalled() {
        assert!(detect_quarantine_starvation("headroom_requests_total 500\n").is_none());
    }

    /// The mechanism catching a few requests during one slow inference is it
    /// working, not failing.
    #[test]
    fn ignores_a_handful_of_skips() {
        assert!(detect_quarantine_starvation(&metrics_body(1, 19, 40)).is_none());
    }

    /// Enough skips, but spread thin enough to be the designed backpressure --
    /// the count alone must not page.
    #[test]
    fn ignores_skips_that_are_a_trivial_share_of_traffic() {
        assert!(detect_quarantine_starvation(&metrics_body(3, 100, 50_000)).is_none());
    }

    /// `headroom_requests_failed_total` must not be mistaken for
    /// `headroom_requests_total` by prefix.
    #[test]
    fn does_not_match_a_longer_series_sharing_the_prefix() {
        let body = "headroom_requests_failed_total 9\n\
                    headroom_compression_quarantine_total{event=\"skipped\"} 72\n";
        assert!(detect_quarantine_starvation(body).is_none());
    }

    /// A scrape with skips but no traffic counter cannot produce a share, and
    /// guessing one is how a basis breaks.
    #[test]
    fn refuses_to_divide_without_a_denominator() {
        let body = "headroom_compression_quarantine_total{event=\"skipped\"} 72\n\
                    headroom_requests_total 0\n";
        assert!(detect_quarantine_starvation(body).is_none());
    }

    #[test]
    fn observe_quarantine_reports_at_most_once_per_process() {
        assert!(!QUARANTINE_REPORTED.swap(true, Ordering::AcqRel));
        assert!(QUARANTINE_REPORTED.swap(true, Ordering::AcqRel));
        QUARANTINE_REPORTED.store(false, Ordering::Release);
    }

    /// The shape is the live one: captured from `/health` on 2026-09-22, where
    /// a 26.1s worker against a 5s Codex-WS awaiter had opened the quarantine
    /// with seven of eight workers idle.
    #[test]
    fn executor_extras_read_the_counters_that_separate_the_two_overruns() {
        let health = r#"{"runtime":{"compression_executor":{
            "max_workers":8,"queued":0,"queued_max":1,"queue_timeouts_total":0,
            "queue_wait_seconds_total":0.41,"queue_wait_seconds_max":0.229,
            "run_seconds_total":101.1,"run_seconds_max":26.1,
            "leaked_threads_total":1,"timed_out_workers_max":1,"source":"auto"}}}"#;

        let extras: Vec<(&str, String)> = executor_extras(health);

        assert_eq!(
            extras,
            vec![
                ("max_workers", "8".to_string()),
                ("queued_max", "1".to_string()),
                ("queue_timeouts_total", "0".to_string()),
                ("queue_wait_seconds_max", "0.229".to_string()),
                ("run_seconds_max", "26.1".to_string()),
                ("run_seconds_total", "101.1".to_string()),
                ("leaked_threads_total", "1".to_string()),
                ("timed_out_workers_max", "1".to_string()),
            ]
        );
    }

    /// Live shape from `/metrics` on 2026-09-22: the slowest call was 7.1s of
    /// `code_aware` while the executor reported a 26.1s worker, which is the
    /// signature of waiting for the one process-wide kompress slot rather than
    /// of a single slow inference.
    #[test]
    fn slowest_compressor_ms_takes_the_max_across_the_compressor_families() {
        let body = concat!(
            "# HELP headroom_transform_timing_ms_max whatever\n",
            "headroom_transform_timing_ms_max{transform=\"compressor:text\"} 1115.74\n",
            "headroom_transform_timing_ms_max{transform=\"compressor:code_aware\"} 7137.48\n",
            "headroom_transform_timing_ms_max{transform=\"compressor:diff\"} 24.77\n",
            // Not a compressor family, and far larger: must not be picked up.
            "headroom_transform_timing_ms_max{transform=\"tool_crusher\"} 99999\n",
        );

        assert_eq!(slowest_compressor_ms(body), Some(7137.48));
        assert_eq!(slowest_compressor_ms("headroom_requests_total 12\n"), None);
    }

    /// An unreachable or reshaped `/health` must cost the report nothing: the
    /// canary's own five extras are the part that must always ship.
    #[test]
    fn executor_extras_are_empty_when_health_cannot_answer() {
        assert!(executor_extras("not json").is_empty());
        assert!(executor_extras(r#"{"runtime":{}}"#).is_empty());
        assert_eq!(
            executor_extras(r#"{"runtime":{"compression_executor":{"max_workers":4}}}"#),
            vec![("max_workers", "4".to_string())]
        );
    }
}
