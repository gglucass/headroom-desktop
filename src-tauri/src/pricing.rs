use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::device;
use crate::keychain;
use crate::models::{
    headroom_tier_for_claude_plan, headroom_tier_for_codex_plan, BillingPeriod,
    ClaudeAccountProfile, ClaudeAuthMethod, ClaudePlanTier, ClaudeUsage, ClaudeUsageWindow,
    CodexAccountProfile, CodexPlanTier, CodexRateLimitSnapshot, CodexUsage, HeadroomAccountProfile,
    HeadroomAuthCodeRequest, HeadroomPricingStatus, HeadroomSubscriptionTier, IntroOffer,
    PlanPrices, PricingCohort, PricingGateReason, TierMismatch, TierRecommendationSource,
};
use crate::state::AppState;
use crate::storage::{app_data_dir, config_file};

const HEADROOM_ACCOUNT_KEYCHAIN_SERVICE: &str = "com.extraheadroom.headroom.account";
const HEADROOM_ACCOUNT_SESSION_ACCOUNT: &str = "session-token";
#[cfg(debug_assertions)]
const DEFAULT_ACCOUNT_API_BASE_URL: &str = "http://127.0.0.1:3000/api/v1";
#[cfg(not(debug_assertions))]
const DEFAULT_ACCOUNT_API_BASE_URL: &str = "https://extraheadroom.com/api/v1";
const LOCAL_GRACE_PERIOD_HOURS: i64 = 72;
const TIER_MISMATCH_GRACE_DAYS: i64 = 14;
// Set to true in dev builds to skip sign-in requirement (indefinite trial)
#[cfg(debug_assertions)]
const INDEFINITE_TRIAL: bool = true;
const AUTH_CODE_EXPIRY_SECONDS: u64 = 900;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalPricingState {
    // Defaulted like every sibling: without this, one field added or removed in
    // a future build fails the whole parse, and the fallback below restarts the
    // grace clock from now for every existing user.
    #[serde(default = "Utc::now")]
    first_seen_at: DateTime<Utc>,
    #[serde(default)]
    reconcile_with_server: bool,
    #[serde(default)]
    mismatch_since: Option<DateTime<Utc>>,
    /// Server-bucketed paywall-first experiment flag. `None` = never fetched.
    /// Refreshed only by the launch-time config fetch so a mid-onboarding
    /// server flip can't strand a user halfway through the gated flow.
    #[serde(default)]
    paywall_first: Option<bool>,
    /// Last time any extraheadroom.com call succeeded (grace/start or account
    /// sync). Baseline for the server-silent Sentry alarm.
    #[serde(default)]
    last_server_contact_at: Option<DateTime<Utc>>,
    /// Last time an authenticated account sync succeeded. Baseline for the
    /// auth-silent alarm (backend reachable, Bearer channel dead).
    #[serde(default)]
    last_account_sync_ok_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct IdentityPayload {
    device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chopratejas_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_account_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_plan_tier: Option<ClaudePlanTier>,
    /// Raw OAuth fields, forwarded verbatim so the server can audit which
    /// taxonomy strings we haven't enumerated yet (especially when the
    /// classified `claude_plan_tier` ends up `unknown`).
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_organization_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_rate_limit_tier: Option<String>,
    /// Per-user rate-limit tier and seat tier, distinct from the org-level
    /// `rate_limit_tier`. On Team/Enterprise orgs these carry the seat-level
    /// entitlement (standard vs premium seat) the org-level string can't show.
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_user_rate_limit_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_seat_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_billing_type: Option<String>,
    // Codex identity, mirroring the Claude fields one-for-one. Sourced from
    // `~/.codex/auth.json` + the live access-token capture
    // (`detect_codex_profile`). Same audit rationale as the raw Claude fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_account_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_plan_tier: Option<CodexPlanTier>,
    /// Sanitized raw plan claim, present only when `codex_plan_tier` is
    /// Unknown; replaces "unknown" as the `X-Headroom-Codex-Plan` value.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_plan_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_organization_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_rate_limit_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_billing_type: Option<String>,
    /// Shape of the live Codex usage windows plus credits,
    /// `primary=99@43200;secondary=12@10080;credits=812` (used percent @ window
    /// minutes; credits balance or `unlimited`). The weekly gate meters against
    /// the windows and `secondary` is optional per plan, so without this we
    /// cannot tell from the fleet whether a clamped subscriber is being metered
    /// or silently allowed; credits identify seats spending beyond their plan
    /// allowance (the upsell cohort the plan claim can't reveal).
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_usage_windows: Option<String>,
    /// Same shape for Claude ("five_hour=NN@300;seven_day=NN@10080"), so the
    /// server can test whether rate-limit pressure predicts conversion.
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_usage_windows: Option<String>,
    /// When the local tier-mismatch clock started, if a mismatch is currently
    /// open. The clamp fires `TIER_MISMATCH_GRACE_DAYS` after this, so the
    /// server can derive both the mismatch cohort and who is actually clamped.
    #[serde(skip_serializing_if = "Option::is_none")]
    tier_mismatch_since: Option<String>,
    /// Highest Terms-of-Service version the user has accepted locally. Rides
    /// along on every grace/start so the server's device-keyed trial record
    /// captures acceptance. `None` (omitted) when nothing accepted yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted_terms_version: Option<u32>,
    /// Windows only: what the WSL probe found (`wsl_probe`). Omitted until the
    /// probe finishes and on every other platform.
    #[serde(skip_serializing_if = "Option::is_none")]
    wsl_agents: Option<String>,
    /// Claude Desktop bucket for this machine (`claude_desktop_verdict`).
    /// Always sent, so the server can tell "reported absent" from "an app old
    /// enough that it never reported" (which stays null).
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_desktop: Option<&'static str>,
}

/// Reqwest errors caused by the user's environment (offline, captive portal,
/// flaky DNS, slow network) rather than anything actionable on our side.
/// Filtering these out of Sentry keeps the activation alert signal-to-noise
/// high.
fn is_transient_transport_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout() || err.is_request()
}

/// Describe a round trip that never produced a response.
///
/// reqwest's `Display` is only "error sending request for url (https://...)":
/// the cause lives in the source chain and is dropped, so the user gets an
/// unactionable URL dump. That is what issue #58 reported, hit during a
/// headroom-web deploy switchover. Classify into three stable phrases rather
/// than interpolating the cause - the user gets something to act on, and
/// Sentry gets a message that groups instead of splintering on `os error 61`.
fn transport_failure(action: &str, err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "the request timed out"
    } else if err.is_connect() {
        "the server could not be reached"
    } else {
        "the request failed"
    };
    format!("Could not {action}: {kind}. Check your connection and try again in a moment.")
}

/// Report transport failures, but rank them. Before issue #58 the auth path
/// dropped transient ones entirely, so we could not answer "how many users hit
/// this" from the client at all - the whole investigation had to be done from
/// headroom-web's deploy log. Sign-in runs ~15x/day fleet-wide, so capturing
/// every failure at Warning costs little; only a non-transport failure (which
/// implies something wrong on our side) is worth an Error.
fn transport_level(err: &reqwest::Error) -> sentry::Level {
    if is_transient_transport_error(err) {
        sentry::Level::Warning
    } else {
        sentry::Level::Error
    }
}

/// Low-cardinality slug for a transport failure's kind. Part of the Sentry
/// fingerprint, so keep the values stable.
fn transport_kind_slug(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else {
        "other"
    }
}

/// Flatten a transport error's source chain into one line.
///
/// `transport_failure` deliberately reduces the cause to three stable phrases
/// so the message groups, which means the actual reason - DNS, refused, or a
/// corporate MITM proxy presenting an untrusted root - never leaves the
/// machine. `is_connect()` covers all three, so RUST-BE could not be told
/// apart from a captive portal. Carried as an extra, never a tag or
/// fingerprint component, so grouping is unaffected. Bounded because a chain
/// is attacker-agnostic but not length-bounded.
fn transport_cause_chain(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        if parts.len() >= 6 {
            break;
        }
        parts.push(cause.to_string());
        source = cause.source();
    }
    // A cause can be a filesystem one (a client cert, a CA bundle path), and
    // this is the one place a raw OS string leaves the machine unbridged --
    // the log bridge's scrub does not apply to an explicit extra.
    // Bounded by CHARS, not bytes: `String::truncate` panics when the byte
    // index is not a char boundary, and a localized Windows OS error message
    // is exactly the multibyte text that would land one mid-character.
    crate::logging::scrub_home(&parts.join(" <- "))
        .chars()
        .take(400)
        .collect()
}

/// (action, kind) pairs that have already reported a TRANSIENT transport
/// failure this session, so the repeats can be dropped.
static TRANSIENT_TRANSPORT_REPORTED: std::sync::Mutex<
    Option<std::collections::HashSet<(&'static str, &'static str)>>,
> = std::sync::Mutex::new(None);

/// Say whether a transient transport failure for `(action_slug, kind)` may
/// still report this session.
///
/// Issue #58 wants the user count, and capturing every transient failure did
/// deliver it -- then kept charging for it. The desktop-activation effect
/// re-fires whenever the runtime-health signal flaps (a 3s poll drives it), so
/// four persistently-offline machines produced RUST-7H: 229 events in 4 days,
/// all of them the same four users re-reporting a captive portal. The first
/// failure per (action, kind) still speaks, so `users_impacted` -- the number
/// issue #58 actually asked for -- is unchanged; only the per-host repetition
/// goes. Keyed by kind as well as action so a connect failure reported early
/// in a session cannot swallow a later, differently-caused timeout: those are
/// separate Sentry issues by design (see `capture_transport_failure`).
fn claim_transient_report_slot(action_slug: &'static str, kind: &'static str) -> bool {
    let mut seen = TRANSIENT_TRANSPORT_REPORTED.lock().unwrap_or_else(|e| {
        // A poisoned lock must not silence reporting outright.
        TRANSIENT_TRANSPORT_REPORTED.clear_poison();
        e.into_inner()
    });
    seen.get_or_insert_with(std::collections::HashSet::new)
        .insert((action_slug, kind))
}

/// Capture a transport failure fingerprinted by (call site, kind), with the
/// transient class capped at one event per (call site, kind) per session.
///
/// `capture_message` alone groups on the shared capture site, so every failure
/// mode of one call lands in a single issue: RUST-7H ended up holding both a
/// genuine server-side 502 and a user's flaky Wi-Fi, which is exactly the
/// camouflage that made an outage question unanswerable from the issue list.
/// Splitting on the kind keeps "could not be reached" (client network) apart
/// from "timed out" (usually a deploy switchover). `action_slug` names the call
/// site and is also part of the fingerprint, so keep it stable too.
///
/// A NON-transient failure implies something wrong on our side, so it is never
/// gated and reports every time.
fn capture_transport_failure(action_slug: &'static str, msg: &str, err: &reqwest::Error) {
    let kind = transport_kind_slug(err);
    if is_transient_transport_error(err) && !claim_transient_report_slot(action_slug, kind) {
        return;
    }
    sentry::with_scope(
        |scope| {
            scope.set_fingerprint(Some(&["transport-failure", action_slug, kind]));
            scope.set_tag("transport.kind", kind);
            scope.set_extra("cause_chain", transport_cause_chain(err).into());
        },
        || sentry::capture_message(msg, transport_level(err)),
    );
}

/// Capture a non-success HTTP status fingerprinted by (call site, status), so a
/// 502 thrown by the edge during a deploy switchover stays a separate issue
/// from a 500 raised inside the handler - and neither is grouped with the
/// transport failures above.
fn capture_http_status(action_slug: &str, msg: &str, status: u16) {
    let status_slug = status.to_string();
    sentry::with_scope(
        |scope| {
            scope.set_fingerprint(Some(&["http-status", action_slug, status_slug.as_str()]));
            scope.set_tag("http.status", status_slug.as_str());
        },
        || sentry::capture_message(msg, sentry::Level::Error),
    );
}

fn plan_tier_header_value(tier: &ClaudePlanTier) -> &'static str {
    match tier {
        ClaudePlanTier::Free => "free",
        ClaudePlanTier::Pro => "pro",
        ClaudePlanTier::Max5x => "max5x",
        ClaudePlanTier::Max20x => "max20x",
        ClaudePlanTier::Api => "api",
        ClaudePlanTier::Unknown => "unknown",
    }
}

impl IdentityPayload {
    fn for_state(state: &AppState) -> Self {
        let claude = state.cached_claude_profile();
        let codex = state.cached_codex_profile();
        let mut payload = Self::build(Some(&claude), codex.as_ref());
        let accepted = state.accepted_terms_version();
        payload.accepted_terms_version = (accepted > 0).then_some(accepted);
        payload.codex_usage_windows = state
            .codex_rate_limits
            .lock()
            .as_ref()
            .map(codex_usage_windows_summary);
        payload.claude_usage_windows = state.claude_usage_windows.lock().clone();
        payload.tier_mismatch_since = load_or_initialize_local_state()
            .ok()
            .and_then(|local| local.mismatch_since)
            .map(|since| since.to_rfc3339());
        payload
    }

    fn device_only() -> Self {
        Self::build(None, None)
    }

    fn build(claude: Option<&ClaudeAccountProfile>, codex: Option<&CodexAccountProfile>) -> Self {
        let device = device::current();
        Self {
            device_id: device.machine_id_digest,
            chopratejas_instance_id: device.chopratejas_instance_id,
            claude_account_uuid: claude.and_then(|p| p.account_uuid.clone()),
            claude_email: claude.and_then(|p| p.email.clone()),
            claude_plan_tier: claude.map(|p| p.plan_tier.clone()),
            claude_organization_type: claude.and_then(|p| p.organization_type.clone()),
            claude_rate_limit_tier: claude.and_then(|p| p.rate_limit_tier.clone()),
            claude_user_rate_limit_tier: claude.and_then(|p| p.user_rate_limit_tier.clone()),
            claude_seat_tier: claude.and_then(|p| p.seat_tier.clone()),
            claude_billing_type: claude.and_then(|p| p.billing_type.clone()),
            codex_account_uuid: codex.and_then(|p| p.account_uuid.clone()),
            codex_email: codex.and_then(|p| p.email.clone()),
            codex_plan_tier: codex.and_then(|p| p.plan_tier),
            codex_plan_raw: codex.and_then(|p| p.plan_raw.clone()),
            codex_organization_type: codex.and_then(|p| p.organization_type.clone()),
            codex_rate_limit_tier: codex.and_then(|p| p.rate_limit_tier.clone()),
            codex_billing_type: codex.and_then(|p| p.billing_type.clone()),
            codex_usage_windows: None,
            claude_usage_windows: None,
            tier_mismatch_since: None,
            accepted_terms_version: None,
            wsl_agents: crate::wsl_probe::result(),
            claude_desktop: Some(crate::client_adapters::claude_desktop_verdict()),
        }
    }

    fn apply_headers(
        &self,
        mut builder: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        builder = builder.header("X-Headroom-App-Version", env!("CARGO_PKG_VERSION"));
        builder = builder.header("X-Headroom-Os", std::env::consts::OS);
        builder = builder.header("X-Headroom-Device-Id", &self.device_id);
        if let Some(value) = self.chopratejas_instance_id.as_deref() {
            builder = builder.header("X-Headroom-Chopratejas-Id", value);
        }
        if let Some(value) = self.claude_account_uuid.as_deref() {
            builder = builder.header("X-Headroom-Claude-Uuid", value);
        }
        if let Some(value) = self.claude_email.as_deref() {
            builder = builder.header("X-Headroom-Claude-Email", value);
        }
        if let Some(tier) = self.claude_plan_tier.as_ref() {
            builder = builder.header("X-Headroom-Claude-Plan", plan_tier_header_value(tier));
        }
        if let Some(value) = self.claude_organization_type.as_deref() {
            builder = builder.header("X-Headroom-Claude-Organization-Type", value);
        }
        if let Some(value) = self.claude_rate_limit_tier.as_deref() {
            builder = builder.header("X-Headroom-Claude-Rate-Limit-Tier", value);
        }
        if let Some(value) = self.claude_user_rate_limit_tier.as_deref() {
            builder = builder.header("X-Headroom-Claude-User-Rate-Limit-Tier", value);
        }
        if let Some(value) = self.claude_seat_tier.as_deref() {
            builder = builder.header("X-Headroom-Claude-Seat-Tier", value);
        }
        if let Some(value) = self.claude_billing_type.as_deref() {
            builder = builder.header("X-Headroom-Claude-Billing-Type", value);
        }
        if let Some(value) = self.codex_account_uuid.as_deref() {
            builder = builder.header("X-Headroom-Codex-Uuid", value);
        }
        if let Some(value) = self.codex_email.as_deref() {
            builder = builder.header("X-Headroom-Codex-Email", value);
        }
        if let Some(tier) = self.codex_plan_tier.as_ref() {
            // An unclassifiable claim ships its sanitized raw value in place of
            // "unknown" so novel OpenAI plans are visible in the fleet by name.
            let value = match (tier, self.codex_plan_raw.as_deref()) {
                (CodexPlanTier::Unknown, Some(raw)) => raw,
                _ => tier.as_header_str(),
            };
            builder = builder.header("X-Headroom-Codex-Plan", value);
        }
        if let Some(value) = self.codex_organization_type.as_deref() {
            builder = builder.header("X-Headroom-Codex-Organization-Type", value);
        }
        if let Some(value) = self.codex_rate_limit_tier.as_deref() {
            builder = builder.header("X-Headroom-Codex-Rate-Limit-Tier", value);
        }
        if let Some(value) = self.codex_billing_type.as_deref() {
            builder = builder.header("X-Headroom-Codex-Billing-Type", value);
        }
        if let Some(value) = self.codex_usage_windows.as_deref() {
            builder = builder.header("X-Headroom-Codex-Usage-Windows", value);
        }
        if let Some(value) = self.claude_usage_windows.as_deref() {
            builder = builder.header("X-Headroom-Claude-Usage-Windows", value);
        }
        if let Some(value) = self.tier_mismatch_since.as_deref() {
            builder = builder.header("X-Headroom-Tier-Mismatch-Since", value);
        }
        if let Some(version) = self.accepted_terms_version {
            builder = builder.header("X-Headroom-Terms-Version", version.to_string());
        }
        builder
    }
}

/// Best-effort: record the user's accepted terms version on the server's
/// device-keyed trial identity. Called right after local acceptance so the
/// server learns immediately; the value also rides along on every subsequent
/// `grace/start` via `IdentityPayload::for_state`. Silent on failure — offline
/// is fine, the next identity push will carry it.
pub fn push_terms_acceptance(state: &AppState, version: u32) {
    let mut identity = IdentityPayload::for_state(state);
    identity.accepted_terms_version = Some(version);
    let _ = fetch_grace_start(&identity);
}

/// Best-effort: tell the server the user reached install-wizard `step`.
/// Piggybacks the existing `desktop/grace/start` POST (device identity already
/// travels with it) via the `X-Headroom-Funnel-Step` header. Fire-and-forget on
/// a detached thread so it never blocks the UI or gates the wizard; the server
/// is first-write-wins, so repeats are harmless.
pub fn report_funnel_step(state: &AppState, step: &str) {
    spawn_funnel_step(IdentityPayload::for_state(state), step);
}

/// `report_funnel_step` for contexts without an `AppState` (e.g. the proxy
/// intercept thread). Device identity alone keys the server's `TrialIdentity`.
pub fn report_funnel_step_device_only(step: &str) {
    spawn_funnel_step(IdentityPayload::device_only(), step);
}

/// Retry backoffs for the funnel beacon. A relaunch that races network
/// bring-up (Windows boot Wi-Fi, VPN) used to lose the step forever: one
/// detached thread, one POST, result discarded -- which is how a
/// `bootstrap_abandoned` report from a briefly-offline relaunch vanished.
/// The server is first-write-wins per (device, step), so retries are
/// idempotent; the sequence is bounded so an all-session-offline device
/// gives up instead of polling forever.
const FUNNEL_STEP_RETRY_BACKOFFS: &[std::time::Duration] = &[
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(60),
    std::time::Duration::from_secs(120),
    std::time::Duration::from_secs(300),
];

fn spawn_funnel_step(identity: IdentityPayload, step: &str) {
    let step = step.to_string();
    std::thread::spawn(move || {
        if let Err(err) = post_funnel_step_with_retries(
            &identity,
            &step,
            &api_base_url(),
            FUNNEL_STEP_RETRY_BACKOFFS,
        ) {
            // info, not warn: the log->Sentry bridge captures warns, and a
            // device offline for the whole session would emit one event per
            // funnel step for a condition that is not our bug.
            log::info!("funnel step {step} not delivered after retries: {err}");
        }
    });
}

fn post_funnel_step_with_retries(
    identity: &IdentityPayload,
    step: &str,
    base_url: &str,
    backoffs: &[std::time::Duration],
) -> Result<(), String> {
    let mut last_err = match post_grace_start_with_step_to(identity, step, base_url) {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };
    for backoff in backoffs {
        std::thread::sleep(*backoff);
        match post_grace_start_with_step_to(identity, step, base_url) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// Test-only seam: a single funnel-step POST against a parameterized
/// base URL so a canned-response server can stand in for headroom-web.
fn post_grace_start_with_step_to(
    identity: &IdentityPayload,
    step: &str,
    base_url: &str,
) -> Result<(), String> {
    let builder = http_client()?
        .post(join_url(base_url, "desktop/grace/start"))
        .header("X-Headroom-Funnel-Step", step);
    let response = identity
        .apply_headers(builder)
        .json(identity)
        .send()
        .map_err(|err| format!("grace/start funnel request failed: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "grace/start funnel returned {}",
            response.status().as_u16()
        ));
    }
    Ok(())
}

/// Stable comparison key for an `IdentityPayload`'s Claude fields. Used to
/// skip redundant `desktop/grace/start` posts when the bearer-triggered
/// worker fires for an account whose fingerprint has not changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityFingerprint {
    claude_account_uuid: Option<String>,
    claude_email: Option<String>,
    claude_plan_tier: Option<ClaudePlanTier>,
    claude_organization_type: Option<String>,
    claude_rate_limit_tier: Option<String>,
    claude_user_rate_limit_tier: Option<String>,
    claude_seat_tier: Option<String>,
    claude_billing_type: Option<String>,
    // Codex plan/account, so an account switch or plan change on the Codex side
    // also forces a fresh `desktop/grace/start` even when Claude is unchanged.
    codex_account_uuid: Option<String>,
    codex_plan_tier: Option<CodexPlanTier>,
}

impl IdentityFingerprint {
    fn from_payload(p: &IdentityPayload) -> Self {
        Self {
            claude_account_uuid: p.claude_account_uuid.clone(),
            claude_email: p.claude_email.clone(),
            claude_plan_tier: p.claude_plan_tier.clone(),
            claude_organization_type: p.claude_organization_type.clone(),
            claude_rate_limit_tier: p.claude_rate_limit_tier.clone(),
            claude_user_rate_limit_tier: p.claude_user_rate_limit_tier.clone(),
            claude_seat_tier: p.claude_seat_tier.clone(),
            claude_billing_type: p.claude_billing_type.clone(),
            codex_account_uuid: p.codex_account_uuid.clone(),
            codex_plan_tier: p.codex_plan_tier,
        }
    }

    /// True when there is nothing meaningful to report — no UUID and no real
    /// plan tier on either side. This is the bearer-not-yet-captured shape.
    /// Codex-only users (ChatGPT seat, no Claude) still report via the codex
    /// signal, so we don't gate solely on Claude.
    fn is_empty(&self) -> bool {
        self.claude_account_uuid.is_none()
            && matches!(self.claude_plan_tier, None | Some(ClaudePlanTier::Unknown))
            && self.codex_account_uuid.is_none()
            && matches!(self.codex_plan_tier, None | Some(CodexPlanTier::Unknown))
    }
}

/// True when a Claude profile carries every identity field we want headroom-web
/// to record: account UUID, email, and a classified plan tier (i.e. Anthropic's
/// OAuth profile fetch returned a populated payload, not a sparse one).
pub fn is_identity_complete(profile: &ClaudeAccountProfile) -> bool {
    profile.account_uuid.is_some()
        && profile.email.is_some()
        && !matches!(profile.plan_tier, ClaudePlanTier::Unknown)
}

/// Warm the cached Claude profile and, if it carries new identity fields,
/// push the populated `IdentityPayload` to `desktop/grace/start`.
///
/// Invoked by the bearer-pusher worker thread whenever the intercept proxy
/// captures a fresh bearer. The OAuth-profile fetch is throttled to once
/// per 24 h once we already know who the user is, so the per-hour bearer
/// rotations don't translate into per-hour calls to Anthropic's
/// `/api/oauth/profile`.
///
/// Throttle does NOT short-circuit the function: we still consult
/// `cached_claude_profile()` (which may have been refreshed by the pricing
/// UI) and push whatever fingerprint it yields if it differs from what we
/// last sent to headroom-web. That way an account switch picked up by
/// another path still propagates without waiting for the 24 h window to
/// expire.
///
/// Idempotent: if the resulting fingerprint matches the last successful
/// push in this session, this is a no-op. On HTTP failure the fingerprint
/// is not recorded, so the next bearer change retries.
pub fn warm_and_push_identity(state: &AppState) {
    const COMPLETE_FETCH_THROTTLE: std::time::Duration =
        std::time::Duration::from_secs(24 * 60 * 60);

    // When the throttle is active we skip the explicit cache warm — but we
    // still read whatever's currently cached and let the fingerprint memo
    // decide whether anything is worth pushing. `IdentityPayload::for_state`
    // calls `cached_claude_profile()`, which itself respects the 5-min TTL
    // and will only round-trip to Anthropic on a true cache miss.
    let throttled = state.complete_identity_fetched_within(COMPLETE_FETCH_THROTTLE);
    if !throttled {
        // Force-warm. Cheap when the bearer slot is empty (short-circuits
        // inside `detect_claude_profile_uncached`).
        let _ = state.cached_claude_profile();
    }

    let identity = IdentityPayload::for_state(state);
    let fp = IdentityFingerprint::from_payload(&identity);

    if fp.is_empty() {
        return;
    }

    if state.identity_fingerprint_already_pushed(&fp) {
        return;
    }

    // Fingerprint differs from last push but throttle is active: another
    // path (pricing UI poll, sign-in) must have refreshed the cache with
    // new identity fields. Push them now even though the worker would
    // otherwise have skipped the OAuth fetch — this is the account-switch
    // path.
    match fetch_grace_start(&identity) {
        Ok(_) => state.record_pushed_identity_fingerprint(fp),
        Err(_) => {
            // Silent — matches `reconcile_local_state_with_server`'s
            // pattern. `fetch_grace_start` failures are typically transient
            // (offline, captive portal, headroom-web blip) and the next
            // bearer change will retry. Sentry-capturing per failure would
            // pin every offline session.
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraceResponse {
    first_seen_at: DateTime<Utc>,
    #[allow(dead_code)]
    grace_ends_at: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    trial_started_at: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    trial_ends_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct ClaudeOauthProfile {
    account: ClaudeOauthProfileAccount,
    organization: Option<ClaudeOauthProfileOrganization>,
}

#[derive(Debug, Clone)]
struct ClaudeOauthProfileAccount {
    uuid: Option<String>,
    email: Option<String>,
    display_name: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct ClaudeOauthProfileOrganization {
    uuid: Option<String>,
    billing_type: Option<String>,
    subscription_created_at: Option<DateTime<Utc>>,
    has_extra_usage_enabled: bool,
    /// e.g. "claude_pro", "claude_max", "claude_enterprise"
    organization_type: Option<String>,
    /// e.g. "default_claude_ai", "claude_max_5x", "claude_max_20x",
    /// "default_claude_max_x5", "default_claude_max_x20" (Anthropic ships both
    /// the `_5x`/`_20x` and `_x5`/`_x20` orderings in the wild)
    rate_limit_tier: Option<String>,
    /// Per-user rate-limit tier. On Team/Enterprise orgs the org-level
    /// `rate_limit_tier` describes the org (e.g. "raven"), while this field
    /// carries the individual seat's limits. Value taxonomy not yet known —
    /// forwarded verbatim for server-side auditing.
    user_rate_limit_tier: Option<String>,
    /// Per-seat entitlement tier on Team/Enterprise orgs (standard vs premium
    /// seat). Same audit rationale as `user_rate_limit_tier`.
    seat_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteAccountEnvelope {
    account: RemoteAccountResponse,
    #[serde(default)]
    active_percent_off: i64,
    #[serde(default)]
    pricing_ladder: Option<PricingLadderPayload>,
    #[serde(default)]
    intro_offer: Option<IntroOffer>,
    #[serde(default)]
    plan_prices: Option<PlanPrices>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteAccountResponse {
    email: String,
    trial_started_at: Option<DateTime<Utc>>,
    trial_ends_at: Option<DateTime<Utc>>,
    #[serde(default)]
    trial_usage_days_left: Option<u32>,
    trial_active: bool,
    subscription_active: bool,
    subscription_tier: Option<HeadroomSubscriptionTier>,
    #[serde(default)]
    subscription_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    subscription_renews_at: Option<DateTime<Utc>>,
    #[serde(default)]
    subscription_amount_cents: Option<i64>,
    #[serde(default)]
    subscription_billing_period: Option<String>,
    #[serde(default)]
    subscription_discount_duration: Option<String>,
    #[serde(default)]
    subscription_discount_duration_in_months: Option<i64>,
    #[serde(default)]
    subscription_cancel_at_period_end: bool,
    #[serde(default)]
    subscription_ends_at: Option<DateTime<Utc>>,
    #[serde(default)]
    subscription_renewal_cents: Option<i64>,
    #[serde(default)]
    subscription_renewal_ends_at: Option<DateTime<Utc>>,
    #[serde(default)]
    subscription_pending_tier: Option<HeadroomSubscriptionTier>,
    #[serde(default)]
    subscription_pending_billing_period: Option<String>,
    #[serde(default)]
    subscription_pending_effective_at: Option<DateTime<Utc>>,
    invite_code: Option<String>,
    accepted_invites_count: usize,
    invite_bonus_percent: f64,
    #[serde(default)]
    upgrade_action: Option<String>,
    #[serde(default)]
    recommended_tier: Option<HeadroomSubscriptionTier>,
    #[serde(default)]
    grandfathered: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyCodeResponse {
    session_token: String,
    account: RemoteAccountResponse,
    #[serde(default)]
    active_percent_off: i64,
    #[serde(default)]
    pricing_ladder: Option<PricingLadderPayload>,
    #[serde(default)]
    intro_offer: Option<IntroOffer>,
    #[serde(default)]
    plan_prices: Option<PlanPrices>,
}

/// The `pricingLadder` object headroom-web nests in account/config payloads.
/// Only the cohorts are consumed on the desktop; the active percent rides at
/// the envelope top level (`active_percent_off`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PricingLadderPayload {
    #[serde(default)]
    cohorts: Vec<PricingCohort>,
}

/// Promo state resolved from whichever payload the desktop fetched
/// (authenticated envelope or public config). The legacy founder-cohort
/// fields stay for outdated servers; current servers send `intro_offer`.
#[derive(Debug, Clone, Default)]
struct PricingPromo {
    active_percent_off: i64,
    cohorts: Vec<PricingCohort>,
    intro_offer: Option<IntroOffer>,
    plan_prices: Option<PlanPrices>,
}

fn build_promo(
    active_percent_off: i64,
    ladder: &Option<PricingLadderPayload>,
    intro_offer: &Option<IntroOffer>,
    plan_prices: &Option<PlanPrices>,
) -> PricingPromo {
    PricingPromo {
        active_percent_off,
        cohorts: ladder
            .as_ref()
            .map(|l| l.cohorts.clone())
            .unwrap_or_default(),
        intro_offer: intro_offer.clone(),
        plan_prices: plan_prices.clone(),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestCodeResponse {
    email: String,
    expires_in_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestCodePayload<'a> {
    email: &'a str,
    #[serde(flatten)]
    identity: IdentityPayload,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifyCodePayload<'a> {
    email: &'a str,
    code: &'a str,
    invite_code: Option<&'a str>,
    #[serde(flatten)]
    identity: IdentityPayload,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutSessionPayload {
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
    /// Always "intro": this build presents the intro offer, so the server
    /// attaches its per-period discount even before the public flip. Legacy
    /// builds omit the field and keep the cohort discount until then.
    pricing_model: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutSessionResponse {
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BillingPortalResponse {
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiErrorResponse {
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum RemoteAccountSyncError {
    Unauthorized,
    /// Anything that is not a 401, carrying what actually failed. RUST-8Z
    /// fired with a bare "Other" after 59 silent hours, which could not tell
    /// an HTTP 5xx from a transport error from a decode failure. The payload
    /// is read only via the derived Debug in the sync-failure log, which
    /// dead-code analysis deliberately ignores.
    Other(#[allow(dead_code)] String),
}

/// When any caller last ran `get_pricing_status`; the background pricing
/// loop in lib.rs only fetches when this is stale.
static LAST_STATUS_FETCH: parking_lot::Mutex<Option<std::time::Instant>> =
    parking_lot::Mutex::new(None);

pub fn status_fetched_within(window: std::time::Duration) -> bool {
    LAST_STATUS_FETCH
        .lock()
        .is_some_and(|at| at.elapsed() < window)
}

pub fn get_pricing_status(state: &AppState) -> Result<HeadroomPricingStatus, String> {
    *LAST_STATUS_FETCH.lock() = Some(std::time::Instant::now());
    let mut local_state = reconcile_local_state_with_server(state)?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let local_grace_active = Utc::now() < local_grace_ends_at;
    let identity = IdentityPayload::for_state(state);
    // A keychain read error propagates (callers fail open), but it also kills
    // every future authenticated call while the app keeps working — exactly
    // the silence the auth-silent alarm exists for.
    let session_token = read_session_token().inspect_err(|err| {
        maybe_report_auth_silent(
            &local_state,
            &identity,
            &format!("keychain read failed: {err}"),
        );
    })?;
    let (authenticated, account, account_sync_error, promo) =
        if let Some(token) = session_token.as_deref() {
            let envelope_result = fetch_remote_account(token, &identity);
            match &envelope_result {
                Ok(_) => stamp_account_sync_ok(&mut local_state),
                Err(err) => maybe_report_auth_silent(
                    &local_state,
                    &identity,
                    &format!("account sync failed: {err:?}"),
                ),
            }
            let promo = envelope_result
                .as_ref()
                .map(|e| {
                    build_promo(
                        e.active_percent_off,
                        &e.pricing_ladder,
                        &e.intro_offer,
                        &e.plan_prices,
                    )
                })
                .unwrap_or_default();
            let account_result = envelope_result.map(|e| e.account);
            let (auth, acc, err) = merge_background_account_sync(Some(token), account_result);
            (auth, acc, err, promo)
        } else {
            let promo = fetch_public_config()
                .map(|c| {
                    build_promo(
                        c.active_percent_off,
                        &c.pricing_ladder,
                        &c.intro_offer,
                        &c.plan_prices,
                    )
                })
                .unwrap_or_default();
            (false, None, None, promo)
        };

    let claude = detect_claude_profile(state);
    let last_known_good_plan_tier = state.last_known_good_plan_tier();
    // Merged profile (live bearer + auth.json), same source the identity
    // payload reports to headroom-web. `None` = no Codex evidence at all →
    // no recommendation; explicit `Unknown` (evidence, unclassifiable plan)
    // still maps conservatively to Max x20.
    let codex_plan = crate::client_adapters::is_codex_enabled()
        .then(|| state.cached_codex_profile().and_then(|p| p.plan_tier))
        .flatten();
    let tier_mismatch = resolve_tier_mismatch(account.as_ref(), &claude, codex_plan);
    // Captured before the mismatch is moved into the Claude evaluator. The clamp
    // is scoped per product: Codex is metered only when the Codex-implied tier
    // exceeds the paid one, and the Claude evaluator gates only on
    // `claude_undercovered` — a mismatch on one product never pauses the other.
    let subscription_clamped = tier_mismatch
        .as_ref()
        .is_some_and(|m| m.clamped && m.codex_undercovered);

    let mut status = evaluate_pricing_status_with_mismatch(
        authenticated,
        local_state.first_seen_at,
        local_grace_ends_at,
        local_grace_active,
        account_sync_error,
        account,
        claude,
        promo,
        last_known_good_plan_tier,
        tier_mismatch,
    );
    status.codex = fetch_codex_usage(state, status.account.as_ref(), subscription_clamped);
    status.codex_plan_tier = Some(state.codex_plan_tier());
    maybe_apply_fake_weekly_gate(&mut status);
    // Attach the signed-in account to the Sentry scope so later captures from
    // anywhere in the process (notably the proxy watchdog's auto-pause event)
    // carry the user's email and tier — without this, support can't map a crash
    // to the customer who reported it. Global scope: persists until overwritten.
    set_sentry_user(status.account.as_ref());
    Ok(status)
}

/// Set (or clear) the Sentry user scope from the Headroom account. Email is the
/// support-triage key; tier is added as a tag so issues can be filtered by plan.
fn set_sentry_user(account: Option<&HeadroomAccountProfile>) {
    sentry::configure_scope(|scope| match account {
        Some(acc) => {
            scope.set_user(Some(sentry::User {
                email: Some(acc.email.clone()),
                ..Default::default()
            }));
            scope.set_tag(
                "headroom.tier",
                acc.subscription_tier
                    .map(|t| format!("{t:?}"))
                    .unwrap_or_else(|| "none".into()),
            );
        }
        None => scope.set_user(None),
    });
}

/// Debug-only: force the weekly-limit nudge or gate so the savings-counterfactual
/// upgrade banner can be eyeballed without burning real weekly usage. Set
/// `HEADROOM_FAKE_WEEKLY_GATE=nudge` or `=gate`. No-op in release builds and when
/// the env var is unset/empty. Also seeds a reset 3 days out and a recommended
/// tier so the "pays for itself" anchor (item 1) and reset countdown (item 2)
/// both have inputs. Dollar figures additionally need `maybe_inject_fake_daily_savings`
/// (same env var) since the labels suppress $0. Opt-in via env, so it stays
/// dormant in shipped RC builds unless a tester sets the var.
fn maybe_apply_fake_weekly_gate(status: &mut HeadroomPricingStatus) {
    // Inert in stable: only RC versions (X.Y.Z-rc.N) honor the override env var.
    if !env!("CARGO_PKG_VERSION").contains("-rc") {
        return;
    }
    let mode = match std::env::var("HEADROOM_FAKE_WEEKLY_GATE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_lowercase(),
        _ => return,
    };
    status.needs_authentication = false;
    status.claude.weekly_resets_at = Some(Utc::now() + Duration::days(3));
    // Preserve the account's real recommendation (so the payback anchors to the
    // tier the user would actually buy); only seed a default when none exists.
    if status.recommended_subscription_tier.is_none() {
        status.recommended_subscription_tier = Some(HeadroomSubscriptionTier::Pro);
    }
    if mode == "gate" {
        status.optimization_allowed = false;
        status.should_nudge = false;
        status.gate_reason = Some(PricingGateReason::WeeklyUsageLimitReached);
        status.gate_message = "Weekly limit reached (forced for testing).".to_string();
        status.claude.weekly_utilization_pct = Some(60.0);
    } else {
        status.optimization_allowed = true;
        status.should_nudge = true;
        status.nudge_level = 1;
        status.gate_message = "Approaching your weekly limit (forced for testing).".to_string();
        status.claude.weekly_utilization_pct = Some(30.0);
    }
    log::debug!("[maybe_apply_fake_weekly_gate] forced weekly {mode}");
}

/// Weekly (secondary-window) utilization (%) at which the Codex gate pauses
/// optimization on a free Headroom account. Mirrors the Claude paid-plan
/// disable threshold so both gates stay in lockstep.
const CODEX_WEEKLY_DISABLE_THRESHOLD_PCT: f64 = 50.0;

/// Build the Codex subscription usage + gate from the latest rate-limit snapshot
/// the intercept proxy captured off live Codex traffic (`AppState::codex_rate_limits`,
/// populated by `proxy_intercept::parse_codex_rate_limit_headers`). Returns
/// `None` when the Codex connector is disabled or no snapshot has been captured
/// yet (no Codex response has flowed through the proxy).
///
/// `account` is the Headroom account state: the gate only nudges/pauses for free
/// accounts (no active subscription, no active trial), exactly like the Claude
/// paid-plan gate. The plan tier behind the recommendation comes from the Codex
/// OAuth JWT (`AppState::codex_plan_tier`).
/// Why the Codex gate is (or isn't) enforcing this cycle. Mirrors the Claude
/// branch activation exactly so the two surfaces gate under identical conditions.
enum CodexActivation {
    /// Grandfathered free account, or a clamped under-subscribed one: meter by
    /// the Codex plan tier via the shared weekly gate.
    Metered,
    /// Trial ended, no subscription, not grandfathered: hard-block.
    HardBlock,
    /// Trial active, correctly subscribed, or no account: usage shown for
    /// reference only, never gated.
    Ungated,
}

fn fetch_codex_usage(
    state: &AppState,
    account: Option<&HeadroomAccountProfile>,
    subscription_clamped: bool,
) -> Option<CodexUsage> {
    if !crate::client_adapters::is_codex_enabled() {
        return None;
    }
    let plan_tier = state.codex_plan_tier();
    // Identical activation ladder to the Claude branch: subscription (metered
    // only when clamped) > active trial > grandfathered (metered) > hard block.
    let (activation, invite_bonus) = match account {
        Some(a) if a.subscription_active => (
            if subscription_clamped {
                CodexActivation::Metered
            } else {
                CodexActivation::Ungated
            },
            a.invite_bonus_percent,
        ),
        Some(a) if a.trial_active => (CodexActivation::Ungated, a.invite_bonus_percent),
        Some(a) if a.grandfathered => (CodexActivation::Metered, a.invite_bonus_percent),
        Some(a) => (CodexActivation::HardBlock, a.invite_bonus_percent),
        None => (CodexActivation::Ungated, 0.0),
    };
    // The snapshot comes from Codex response headers seen in-process, so it
    // is empty after every relaunch until the first response. Metering needs
    // real windows; the hard block does not, and waiting for a snapshot ran
    // Codex fully optimized past the wall until then (and forever on hosts
    // whose responses never carry the headers).
    let snapshot = match (state.codex_rate_limits.lock().clone(), &activation) {
        (Some(snapshot), _) => snapshot,
        (None, CodexActivation::HardBlock) => CodexRateLimitSnapshot::default(),
        (None, _) => return None,
    };
    Some(codex_usage_from_snapshot(
        snapshot,
        plan_tier,
        activation,
        invite_bonus,
    ))
}

/// Shortest usage window the weekly ladder may be metered against, in minutes.
/// The 25%/50% thresholds are calibrated to a week; applying them to a 5-hour
/// window would pause a paying user within the first afternoon.
const MIN_METERED_WINDOW_MINUTES: i64 = 1_440;

/// The window the gate meters against: the longest one the plan reports, not
/// `secondary` specifically.
///
/// `secondary` is genuinely optional — a `GET /wham/usage` on a free ChatGPT
/// account returns a 30-day `primary_window` and a null `secondary_window` —
/// and reading it alone collapsed "this plan reports no weekly window" into
/// `WeeklyGateOutcome::NoData`, which allows. A clamped under-subscribed
/// account on such a plan was therefore never metered at all: the gate failed
/// open and silently.
///
/// Windows shorter than [`MIN_METERED_WINDOW_MINUTES`] are ignored rather than
/// metered harshly. A window with no declared length is kept (that is today's
/// behaviour for `secondary`) but sorts last, so a declared long window wins.
///
/// ponytail: a plan whose only window is shorter than a day still fails open.
/// The `X-Headroom-Codex-Usage-Windows` telemetry added alongside this exists
/// to find out whether such a plan is real before writing a rule for it.
fn metered_window(snapshot: &CodexRateLimitSnapshot) -> Option<&crate::models::CodexUsageWindow> {
    [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
        .filter(|w| {
            w.window_minutes
                .is_none_or(|m| m >= MIN_METERED_WINDOW_MINUTES)
        })
        .max_by_key(|w| w.window_minutes.unwrap_or(0))
}

/// Claude counterpart of `codex_usage_windows_summary`, same wire shape so the
/// server parses both with one regex. Window minutes are Anthropic's fixed
/// 5h / 7d.
fn claude_usage_windows_summary(usage: &crate::models::ClaudeUsage) -> String {
    let mut parts = Vec::new();
    if let Some(w) = usage.five_hour.as_ref() {
        parts.push(format!("five_hour={:.0}@300", w.utilization));
    }
    if let Some(w) = usage.seven_day.as_ref() {
        parts.push(format!("seven_day={:.0}@10080", w.utilization));
    }
    parts.join(";")
}

/// Compact wire summary of the Codex usage windows and credits, as
/// `primary=99@43200;secondary=12@10080;credits=812` (used percent @ window
/// minutes; credits balance or `unlimited`), or `none` for an empty snapshot.
/// Reported to headroom-web so we can see which ChatGPT plans actually publish
/// a weekly window — the gate depends on it and we previously had no field
/// visibility. Credits ride along to spot seats burning workspace credits
/// beyond their plan allowance (post-April-2026 credit-billed Codex): the
/// upsell cohort the plan claim alone can't identify. Field result after four
/// days (prod, 2026-08-22): 14 identities reported credits, 11 of them `0` on
/// consumer Plus/Pro, and only one Business row - Business seats publish
/// `primary=0@0` windows, so this string is all we will ever get from them.
fn codex_usage_windows_summary(snapshot: &CodexRateLimitSnapshot) -> String {
    let part = |name: &str, window: Option<&crate::models::CodexUsageWindow>| {
        window.map(|w| match w.window_minutes {
            Some(minutes) => format!("{name}={:.0}@{minutes}", w.used_percent),
            None => format!("{name}={:.0}", w.used_percent),
        })
    };
    let credits = if snapshot.credits_unlimited {
        Some("credits=unlimited".to_string())
    } else {
        // Upstream-controlled string headed into an HTTP header: keep only a
        // compact numeric-ish charset so a hostile value can't break the send.
        snapshot.credits_balance.as_deref().map(|balance| {
            let clean: String = balance
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .take(16)
                .collect();
            format!("credits={clean}")
        })
    };
    let parts: Vec<String> = [
        part("primary", snapshot.primary.as_ref()),
        part("secondary", snapshot.secondary.as_ref()),
        credits,
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(";")
    }
}

/// Derive the display-facing `CodexUsage` (gate state + copy) from a captured
/// snapshot, applying the activation-scoped weekly gate.
fn codex_usage_from_snapshot(
    snapshot: CodexRateLimitSnapshot,
    plan_tier: CodexPlanTier,
    activation: CodexActivation,
    invite_bonus_percent: f64,
) -> CodexUsage {
    let recommended_subscription_tier =
        Some(headroom_tier_for_codex_plan(&plan_tier).unwrap_or(HeadroomSubscriptionTier::Pro));
    // Report the window the gate actually used. Reading `secondary` here left
    // this null for every plan measured, which silently killed the Codex
    // forgone-savings upgrade line (it multiplies by this window's reset).
    let metered = metered_window(&snapshot);
    let weekly_used_percent = metered.map(|w| w.used_percent);
    let weekly_resets_in_seconds = metered.and_then(|w| w.seconds_until_reset);

    let gate = codex_plan_gate(
        weekly_used_percent,
        &plan_tier,
        invite_bonus_percent,
        activation,
    );

    CodexUsage {
        limit_name: snapshot.limit_name,
        primary: snapshot.primary,
        secondary: snapshot.secondary,
        credits_balance: snapshot.credits_balance,
        credits_unlimited: snapshot.credits_unlimited,
        optimization_allowed: gate.optimization_allowed,
        should_nudge: gate.should_nudge,
        nudge_level: gate.nudge_level,
        gate_reason: gate.gate_reason,
        recommended_subscription_tier,
        weekly_used_percent,
        weekly_resets_in_seconds,
        gate_message: gate.gate_message,
        effective_nudge_thresholds_percent: gate.nudge_thresholds_percent.to_vec(),
        effective_disable_threshold_percent: gate.disable_threshold_percent,
    }
}

struct CodexGate {
    optimization_allowed: bool,
    should_nudge: bool,
    nudge_level: u8,
    gate_reason: Option<PricingGateReason>,
    gate_message: String,
    nudge_thresholds_percent: [f64; 3],
    disable_threshold_percent: f64,
}

/// Codex weekly-usage gate, the Codex-side wrapper around the shared
/// `evaluate_weekly_gate`. Keyed off `pricing_policy_for_codex_plan` so its
/// per-tier caps are identical to Claude's: Free ungated (100%), Go/Plus/
/// Team/Business 50%, Pro/Enterprise and Unknown 25%. Enforcement is scoped to Codex
/// traffic via `AppState::codex_bypass`, so it never pauses Claude optimization.
fn codex_plan_gate(
    weekly_used_percent: Option<f64>,
    plan_tier: &CodexPlanTier,
    invite_bonus_percent: f64,
    activation: CodexActivation,
) -> CodexGate {
    let policy = pricing_policy_for_codex_plan(plan_tier);
    // Reference thresholds for the UI even when ungated. Free has no policy, so
    // fall back to the Pro ladder purely for display — it is never enforced.
    let (disable_display, nudges_display) = match &policy {
        Some(p) => (p.disable_threshold_percent, p.nudge_thresholds_percent),
        None => (CODEX_WEEKLY_DISABLE_THRESHOLD_PCT, NUDGE_THRESHOLDS_PERCENT),
    };

    match activation {
        // Post-trial, no subscription, not grandfathered: hard-block, mirroring
        // the Claude branch. No weekly-metered free tier for this cohort.
        CodexActivation::HardBlock => CodexGate {
            optimization_allowed: false,
            should_nudge: true,
            nudge_level: u8::MAX,
            gate_reason: Some(PricingGateReason::TrialEnded),
            gate_message:
                "Your Headroom trial has ended. Upgrade to keep Headroom optimizing Codex.".into(),
            nudge_thresholds_percent: nudges_display,
            disable_threshold_percent: disable_display,
        },
        // Grandfathered free or clamped subscriber: meter by plan tier. Free
        // (policy None) resolves to NoData and stays allowed (100%).
        CodexActivation::Metered => {
            let decision = evaluate_weekly_gate(policy, weekly_used_percent, invite_bonus_percent);
            let gate_reason = matches!(decision.outcome, WeeklyGateOutcome::Paused { .. })
                .then_some(PricingGateReason::CodexWeeklyUsageLimitReached);
            let gate_message = match decision.outcome {
                WeeklyGateOutcome::Paused { weekly_usage } => format!(
                    "Headroom is paused because you've reached {:.1}% of weekly Codex usage. Upgrade to raise your limit.",
                    weekly_usage
                ),
                WeeklyGateOutcome::Nudging {
                    weekly_usage,
                    disable,
                    level,
                } => format_nudge_message("Codex", weekly_usage, disable, level),
                WeeklyGateOutcome::Active {
                    first_nudge,
                    disable,
                } => format!(
                    "Headroom is active. It will start nudging at {:.1}% and pause at {:.1}% of weekly Codex usage for your detected plan.",
                    first_nudge, disable
                ),
                WeeklyGateOutcome::NoData => {
                    "Send a Codex prompt through Headroom to sync your current weekly usage window."
                        .into()
                }
            };
            CodexGate {
                optimization_allowed: decision.optimization_allowed,
                should_nudge: decision.should_nudge,
                nudge_level: decision.nudge_level,
                gate_reason,
                gate_message,
                nudge_thresholds_percent: nudges_display,
                disable_threshold_percent: decision
                    .effective_disable_threshold_percent
                    .unwrap_or(disable_display),
            }
        }
        // Active trial / correctly subscribed / no account: reference only.
        CodexActivation::Ungated => {
            let gate_message = match weekly_used_percent {
                Some(weekly_usage) => {
                    format!("Codex weekly usage is at {weekly_usage:.0}% of the current window.")
                }
                None => {
                    "Send a Codex prompt through Headroom to sync your current weekly usage window."
                        .into()
                }
            };
            CodexGate {
                optimization_allowed: true,
                should_nudge: false,
                nudge_level: 0,
                gate_reason: None,
                gate_message,
                nudge_thresholds_percent: nudges_display,
                disable_threshold_percent: disable_display,
            }
        }
    }
}

pub fn request_auth_code(state: &AppState, email: &str) -> Result<HeadroomAuthCodeRequest, String> {
    request_auth_code_with_base_url(state, email, &api_base_url())
}

/// Test-only seam: `request_auth_code` against a parameterized base URL so a
/// canned-response test server can stand in for headroom-web.
pub(crate) fn request_auth_code_with_base_url(
    state: &AppState,
    email: &str,
    base_url: &str,
) -> Result<HeadroomAuthCodeRequest, String> {
    let trimmed = email.trim().to_ascii_lowercase();
    if trimmed.is_empty() || !trimmed.contains('@') {
        return Err("Enter a valid email address.".into());
    }

    let response = http_client()?
        .post(join_url(base_url, "desktop/auth/request_code"))
        .json(&RequestCodePayload {
            email: &trimmed,
            identity: IdentityPayload::for_state(state),
        })
        .send()
        .map_err(|err| {
            let msg = transport_failure("request a sign-in code", &err);
            capture_transport_failure("auth-request-code", &msg, &err);
            msg
        })?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let msg = format!("Could not request sign-in code (status {status}).");
        if status >= 500 {
            capture_http_status("auth-request-code", &msg, status);
        }
        return Err(msg);
    }

    let body: RequestCodeResponse = response.json().map_err(|err| {
        let msg = format!("Could not parse sign-in response: {err}");
        sentry::capture_message(&msg, sentry::Level::Error);
        msg
    })?;

    Ok(HeadroomAuthCodeRequest {
        email: body.email,
        expires_in_seconds: body.expires_in_seconds.max(1).min(AUTH_CODE_EXPIRY_SECONDS),
    })
}

pub fn verify_auth_code(
    state: &AppState,
    email: &str,
    code: &str,
    invite_code: Option<&str>,
) -> Result<HeadroomPricingStatus, String> {
    verify_auth_code_with_base_url(state, email, code, invite_code, &api_base_url())
}

/// Test-only seam: `verify_auth_code` against a parameterized base URL so a
/// canned-response test server can stand in for headroom-web.
pub(crate) fn verify_auth_code_with_base_url(
    state: &AppState,
    email: &str,
    code: &str,
    invite_code: Option<&str>,
    base_url: &str,
) -> Result<HeadroomPricingStatus, String> {
    let trimmed_email = email.trim().to_ascii_lowercase();
    let trimmed_code = code.trim();
    if trimmed_email.is_empty() || !trimmed_email.contains('@') {
        return Err("Enter a valid email address.".into());
    }
    if trimmed_code.is_empty() {
        return Err("Enter the authentication code from your email.".into());
    }

    let response = http_client()?
        .post(join_url(base_url, "desktop/auth/verify_code"))
        .json(&VerifyCodePayload {
            email: &trimmed_email,
            code: trimmed_code,
            invite_code: invite_code.map(str::trim).filter(|value| !value.is_empty()),
            identity: IdentityPayload::for_state(state),
        })
        .send()
        .map_err(|err| {
            let msg = transport_failure("verify your sign-in code", &err);
            capture_transport_failure("auth-verify-code", &msg, &err);
            msg
        })?;

    if !response.status().is_success() {
        return Err(format!(
            "Could not verify sign-in code (status {}).",
            response.status().as_u16()
        ));
    }

    let body: VerifyCodeResponse = response
        .json()
        .map_err(|err| format!("Could not parse verification response: {err}"))?;

    write_session_token(&body.session_token)?;

    let local_state = reconcile_local_state_with_server(state)?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let claude = detect_claude_profile(state);
    let last_known_good_plan_tier = state.last_known_good_plan_tier();
    let account = remote_account_to_profile(body.account);
    // Merged profile (live bearer + auth.json), same source the identity
    // payload reports to headroom-web. `None` = no Codex evidence at all →
    // no recommendation; explicit `Unknown` (evidence, unclassifiable plan)
    // still maps conservatively to Max x20.
    let codex_plan = crate::client_adapters::is_codex_enabled()
        .then(|| state.cached_codex_profile().and_then(|p| p.plan_tier))
        .flatten();
    let tier_mismatch = resolve_tier_mismatch(Some(&account), &claude, codex_plan);

    Ok(evaluate_pricing_status_with_mismatch(
        true,
        local_state.first_seen_at,
        local_grace_ends_at,
        Utc::now() < local_grace_ends_at,
        None,
        Some(account),
        claude,
        build_promo(
            body.active_percent_off,
            &body.pricing_ladder,
            &body.intro_offer,
            &body.plan_prices,
        ),
        last_known_good_plan_tier,
        tier_mismatch,
    ))
}

pub fn sign_out() -> Result<(), String> {
    clear_session_token()
}

/// How long to wait before the single activation retry.
///
/// headroom-web answers this endpoint in ~8ms at p50, so a slow attempt is not
/// load, it is a deploy switchover: Railway's edge holds the connection until
/// its 15s cap while the replacement instance comes up, which the 8s
/// per-attempt timeout in `http_client` turns into a guaranteed client-side
/// failure (Sentry RUST-8X, and the "hit during a headroom-web deploy
/// switchover" note on `transport_failure`). One retry a second later lands
/// after the switchover instead of surfacing an error the user cannot act on.
const ACTIVATE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Gateway-class statuses worth the one retry: the edge could not hand the
/// request to a healthy instance, so it never reached the activation handler.
/// 500 and 504 are deliberately excluded - there the handler did run, and may
/// already have applied its write.
fn is_retryable_gateway_status(status: u16) -> bool {
    matches!(status, 502 | 503)
}

/// POST the activation request, retrying once past a deploy switchover.
///
/// Only the final outcome reaches Sentry: a first attempt that fails and then
/// succeeds is a switchover the user never saw, and capturing it would keep
/// exactly the noise the fingerprinting above is meant to cut.
///
/// Retrying a POST is safe here because activation is idempotent server-side -
/// it marks an account active and returns that account's current state - so a
/// duplicate reaching the handler is a no-op, the same reasoning that lets
/// `post_funnel_step_with_retries` retry `desktop/grace/start`.
fn send_activation_request(
    identity: &IdentityPayload,
    token: &str,
    base_url: &str,
    payload: &serde_json::Value,
    retry_backoff: std::time::Duration,
) -> Result<reqwest::blocking::Response, String> {
    let mut retried = false;
    loop {
        let builder = http_client()?
            .post(join_url(base_url, "desktop/account/activate"))
            .header("Authorization", format!("Bearer {token}"));
        let result = identity.apply_headers(builder).json(payload).send();

        let retryable = match &result {
            Err(err) => is_transient_transport_error(err),
            Ok(response) => is_retryable_gateway_status(response.status().as_u16()),
        };
        if retryable && !retried {
            retried = true;
            std::thread::sleep(retry_backoff);
            continue;
        }

        return result.map_err(|err| {
            let msg = transport_failure("activate Headroom desktop access", &err);
            capture_transport_failure("account-activate", &msg, &err);
            msg
        });
    }
}

pub fn activate_account(
    state: &AppState,
    lifetime_tokens_saved: u64,
) -> Result<HeadroomPricingStatus, String> {
    activate_account_with_base_url(state, lifetime_tokens_saved, &api_base_url())
}

/// Test-only seam: `activate_account` against a parameterized base URL so a
/// canned-response test server can stand in for headroom-web.
pub(crate) fn activate_account_with_base_url(
    state: &AppState,
    lifetime_tokens_saved: u64,
    base_url: &str,
) -> Result<HeadroomPricingStatus, String> {
    activate_account_with_retry_backoff(
        state,
        lifetime_tokens_saved,
        base_url,
        ACTIVATE_RETRY_BACKOFF,
    )
}

/// Test-only seam: `activate_account_with_base_url` with the switchover retry
/// backoff parameterized, so a test can exercise the retry without sleeping the
/// production delay.
pub(crate) fn activate_account_with_retry_backoff(
    state: &AppState,
    lifetime_tokens_saved: u64,
    base_url: &str,
    retry_backoff: std::time::Duration,
) -> Result<HeadroomPricingStatus, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before activating desktop access.".to_string())?;
    let identity = IdentityPayload::for_state(state);
    let payload = serde_json::json!({ "lifetime_tokens_saved": lifetime_tokens_saved });
    let response = send_activation_request(&identity, &token, base_url, &payload, retry_backoff)?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let msg = format!("Could not activate Headroom desktop access (status {status}).");
        if status >= 500 {
            capture_http_status("account-activate", &msg, status);
        }
        return Err(msg);
    }

    // Read text first: reqwest's `.json()` collapses body-read failures and
    // serde mismatches into one opaque "error decoding response body"
    // (Sentry RUST-58). Splitting them makes the serde error name the field
    // and lets us see the raw body when the server didn't send JSON at all.
    let raw = response.text().map_err(|err| {
        let msg = format!("Could not read Headroom activation response: {err}");
        if !is_transient_transport_error(&err) {
            sentry::capture_message(&msg, sentry::Level::Error);
        }
        msg
    })?;
    let body: RemoteAccountEnvelope = serde_json::from_str(&raw).map_err(|err| {
        let msg = format!("Could not parse Headroom activation response: {err}");
        sentry::with_scope(
            |scope| {
                // Serde errors vary by line/column; one fingerprint keeps them
                // a single issue.
                scope.set_fingerprint(Some(&["activation-parse-error"]));
                // Snippet only when the body isn't JSON (HTML error page,
                // empty body); a valid-JSON schema mismatch is described by
                // `err` itself and may carry account data.
                if serde_json::from_str::<serde_json::Value>(&raw).is_err() {
                    let snippet: String = raw.chars().take(300).collect();
                    scope.set_extra("body_snippet", snippet.into());
                }
            },
            || sentry::capture_message(&msg, sentry::Level::Error),
        );
        msg
    })?;
    let local_state = reconcile_local_state_with_server(state)?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let claude = detect_claude_profile(state);
    let last_known_good_plan_tier = state.last_known_good_plan_tier();
    let account = remote_account_to_profile(body.account);
    // Merged profile (live bearer + auth.json), same source the identity
    // payload reports to headroom-web. `None` = no Codex evidence at all →
    // no recommendation; explicit `Unknown` (evidence, unclassifiable plan)
    // still maps conservatively to Max x20.
    let codex_plan = crate::client_adapters::is_codex_enabled()
        .then(|| state.cached_codex_profile().and_then(|p| p.plan_tier))
        .flatten();
    let tier_mismatch = resolve_tier_mismatch(Some(&account), &claude, codex_plan);

    Ok(evaluate_pricing_status_with_mismatch(
        true,
        local_state.first_seen_at,
        local_grace_ends_at,
        Utc::now() < local_grace_ends_at,
        None,
        Some(account),
        claude,
        build_promo(
            body.active_percent_off,
            &body.pricing_ladder,
            &body.intro_offer,
            &body.plan_prices,
        ),
        last_known_good_plan_tier,
        tier_mismatch,
    ))
}

/// Lifetime + recent-daily savings facts for the admin profile, sent alongside
/// the milestone/heartbeat post. Raw counters only: the server owns the single
/// copy of the rate formulas. `output_reduction_percent` is the exception --
/// it is a counterfactual estimate produced by the shaper's own estimator, not
/// something recomputable from counters, so it travels with its method label.
///
/// snake_case on purpose (Rails params), unlike the camelCase view models in
/// `models.rs`.
///
/// The server derives the billable-input rate from the DOLLAR fields, never the
/// token counts. `total_input_tokens` is our local tokenizer's count of the
/// forwarded prompt while `cache_read_tokens` is the provider's own count, so
/// differencing them mixes tokenizer scales and can go negative (see
/// `outcome.py`: "must never be differenced"). The dollar pair comes from one
/// pricing function and is safe. Tokens travel for display only.
#[derive(Debug, Clone, Serialize)]
pub struct SavingsReport {
    pub lifetime_savings_usd: f64,
    pub lifetime_tokens_saved: u64,
    pub total_input_tokens: u64,
    pub cache_read_tokens: u64,
    /// What input actually cost, cache reads included.
    pub total_input_cost_usd: f64,
    /// The read DISCOUNT earned, not the read cost. Read cost = this / 9.
    pub cache_savings_usd: f64,
    pub output_reduction_percent: Option<f64>,
    pub output_reduction_method: Option<String>,
    /// How much of the machine's shaped traffic that percentage actually
    /// covers. Without it the server cannot tell a solid 43% from a 97% drawn
    /// from 3% of requests, and both render identically in the admin.
    pub output_reduction_requests: Option<u64>,
    pub output_reduction_coverage_percent: Option<f64>,
    /// Retrieval-churn gauges (see `DashboardState`): how much compressed-away
    /// content came back. The over-compression tripwire behind "context filled
    /// up faster with Headroom" reports.
    pub reread_tokens: Option<u64>,
    pub reread_compressed_tokens: Option<u64>,
    pub ccr_retrievals: Option<u64>,
    pub days: Vec<SavingsDay>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavingsDay {
    pub date: String,
    /// UTC end boundary of this local calendar day, used for trial-day accounting.
    pub day_ends_at: Option<DateTime<Utc>>,
    /// Input compression only. The app's chart headline sums this WITH
    /// `output_savings_usd` (both Headroom layers), so the server has to carry
    /// them separately to show the same total and still rate the input layer.
    pub savings_usd: f64,
    pub output_savings_usd: f64,
    pub tokens_saved: u64,
    pub tokens_sent: u64,
    pub actual_cost_usd: f64,
    pub cache_read_tokens: Option<u64>,
    /// None on buckets with no cache coverage; the server then has no billable
    /// baseline for the day and reports "-" rather than a scale-mixed guess.
    pub cache_savings_usd: Option<f64>,
    pub output_sampled_tokens_saved: Option<u64>,
    pub output_baseline_tokens: Option<u64>,
    /// Aggregate per-client counters from the intercept proxy (local day
    /// keys; see usage_counters.rs for the join caveat). None on days
    /// observed by builds predating the counters.
    pub client_requests: Option<std::collections::BTreeMap<String, u64>>,
    pub rate_limit_429s: Option<std::collections::BTreeMap<String, u64>>,
}

/// Fire-and-forget: reports a milestone to the server so it can trigger
/// the feedback email for users who were below the threshold at sign-up.
/// Silently no-ops if the user is not signed in or the request fails.
pub fn report_milestone(milestone_tokens_saved: u64, savings: Option<&SavingsReport>) {
    let token = match read_session_token() {
        Ok(Some(t)) => t,
        _ => return,
    };
    let client = match http_client() {
        Ok(c) => c,
        Err(_) => return,
    };
    let identity = IdentityPayload::device_only();
    let builder = client
        .post(api_url("desktop/milestones"))
        .header("Authorization", format!("Bearer {token}"));
    let _ = identity
        .apply_headers(builder)
        .json(&serde_json::json!({
            "milestone_tokens_saved": milestone_tokens_saved,
            "savings": savings,
        }))
        .send();
}

/// The weekly-limit nudge the desktop should report, with the disable threshold
/// of whichever provider/tier tripped (50% for Pro/Plus, 25% for Max/Pro-x).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeeklyLimitNudge {
    pub status: &'static str,
    /// Free-plan weekly cap that gated this provider; `None` if unknown.
    pub cap_percent: Option<f64>,
}

/// Maps a freshly evaluated pricing status to the weekly-limit nudge the
/// desktop should report, or `None`. `"reached"` when the weekly cap has paused
/// optimization (Claude or Codex); `"approaching"` when nudging near it but not
/// yet paused. The cap reflects whichever provider tripped — Claude's tier-aware
/// threshold, or the fixed Codex cap. Subscriber filtering and per-window
/// de-duplication are the server's job (headroom-web
/// `POST /api/v1/desktop/weekly_limit`), so this stays a pure mapping.
pub fn weekly_limit_signal(status: &HeadroomPricingStatus) -> Option<WeeklyLimitNudge> {
    // A trial-ended hard block is not a weekly-cap event: there is no free plan
    // to be close to, and the branch sets `should_nudge` purely to surface the
    // upgrade prompt. Reporting it would email "you're close to your weekly
    // limit on the free plan" to someone whose trial simply expired.
    if matches!(status.gate_reason, Some(PricingGateReason::TrialEnded)) {
        return None;
    }

    let claude_cap = status.effective_disable_threshold_percent;
    let codex_cap = Some(CODEX_WEEKLY_DISABLE_THRESHOLD_PCT);

    let claude_reached = !status.optimization_allowed
        && matches!(
            status.gate_reason,
            Some(PricingGateReason::WeeklyUsageLimitReached)
        );
    if claude_reached {
        return Some(WeeklyLimitNudge {
            status: "reached",
            cap_percent: claude_cap,
        });
    }
    let codex_reached = status.codex.as_ref().is_some_and(|codex| {
        !codex.optimization_allowed
            && !matches!(codex.gate_reason, Some(PricingGateReason::TrialEnded))
    });
    if codex_reached {
        return Some(WeeklyLimitNudge {
            status: "reached",
            cap_percent: codex_cap,
        });
    }

    if status.should_nudge {
        return Some(WeeklyLimitNudge {
            status: "approaching",
            cap_percent: claude_cap,
        });
    }
    let codex_approaching = status
        .codex
        .as_ref()
        .is_some_and(|codex| codex.should_nudge);
    if codex_approaching {
        return Some(WeeklyLimitNudge {
            status: "approaching",
            cap_percent: codex_cap,
        });
    }
    None
}

/// Fire-and-forget: reports a weekly-limit nudge event ("approaching" or
/// "reached"), with the free-plan cap that tripped, so the server can email
/// free-plan users. Mirrors `report_milestone`: no-ops if the user is not
/// signed in or the request fails.
pub fn report_weekly_limit(status: &str, cap_percent: Option<f64>) {
    let token = match read_session_token() {
        Ok(Some(t)) => t,
        _ => return,
    };
    let client = match http_client() {
        Ok(c) => c,
        Err(_) => return,
    };
    let identity = IdentityPayload::device_only();
    let builder = client
        .post(api_url("desktop/weekly_limit"))
        .header("Authorization", format!("Bearer {token}"));
    let _ = identity
        .apply_headers(builder)
        .json(&serde_json::json!({
            "status": status,
            "cap_percent": cap_percent.map(|p| p.round() as i64),
        }))
        .send();
}

pub fn create_checkout_session(
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
) -> Result<String, String> {
    create_checkout_session_with_base_url(subscription_tier, billing_period, &api_base_url())
}

/// Test-only seam: `create_checkout_session` against a parameterized base URL.
pub(crate) fn create_checkout_session_with_base_url(
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
    base_url: &str,
) -> Result<String, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before starting checkout.".to_string())?;
    let response = http_client()?
        .post(join_url(base_url, "desktop/checkout"))
        .header("Authorization", format!("Bearer {token}"))
        // Same device id the config fetch sends: the server uses it to decide
        // whether this checkout is in the paywall-first cohort (7-day trial).
        .header("X-Headroom-Device-Id", device::current().machine_id_digest)
        .json(&CheckoutSessionPayload {
            subscription_tier,
            billing_period,
            pricing_model: "intro",
        })
        .send()
        .map_err(|err| transport_failure("start checkout", &err))?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let api_error = response
            .json::<ApiErrorResponse>()
            .ok()
            .and_then(|body| body.error)
            .filter(|value| !value.trim().is_empty());
        return Err(api_error
            .unwrap_or_else(|| format!("Could not create checkout session (status {status}).")));
    }

    response
        .json::<CheckoutSessionResponse>()
        .map(|body| body.url)
        .map_err(|err| format!("Could not parse checkout response: {err}"))
}

pub fn change_subscription_plan(
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
) -> Result<(), String> {
    change_subscription_plan_with_base_url(subscription_tier, billing_period, &api_base_url())
}

/// Test-only seam: `change_subscription_plan` against a parameterized base URL.
pub(crate) fn change_subscription_plan_with_base_url(
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
    base_url: &str,
) -> Result<(), String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before changing your plan.".to_string())?;
    let response = http_client()?
        .post(join_url(base_url, "desktop/subscriptions/change_plan"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&CheckoutSessionPayload {
            subscription_tier,
            billing_period,
            pricing_model: "intro",
        })
        .send()
        .map_err(|err| transport_failure("change your plan", &err))?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let api_error = response
            .json::<ApiErrorResponse>()
            .ok()
            .and_then(|body| body.error)
            .filter(|value| !value.trim().is_empty());
        return Err(api_error
            .unwrap_or_else(|| format!("Could not change subscription plan (status {status}).")));
    }

    Ok(())
}

pub fn reactivate_subscription() -> Result<(), String> {
    reactivate_subscription_with_base_url(&api_base_url())
}

pub(crate) fn reactivate_subscription_with_base_url(base_url: &str) -> Result<(), String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before reactivating your plan.".to_string())?;
    let response = http_client()?
        .post(join_url(base_url, "desktop/subscriptions/reactivate"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|err| transport_failure("reactivate your subscription", &err))?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let api_error = response
            .json::<ApiErrorResponse>()
            .ok()
            .and_then(|body| body.error)
            .filter(|value| !value.trim().is_empty());
        return Err(api_error
            .unwrap_or_else(|| format!("Could not reactivate subscription (status {status}).")));
    }

    Ok(())
}

pub fn get_billing_portal_url(target: Option<String>) -> Result<String, String> {
    get_billing_portal_url_with_base_url(&api_base_url(), target.as_deref())
}

/// Test-only seam: `get_billing_portal_url` against a parameterized base URL.
pub(crate) fn get_billing_portal_url_with_base_url(
    base_url: &str,
    target: Option<&str>,
) -> Result<String, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before accessing billing.".to_string())?;
    let mut request = http_client()?
        .get(join_url(base_url, "desktop/billing_portal"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(target_value) = target {
        request = request.query(&[("target", target_value)]);
    }
    let response = request
        .send()
        .map_err(|err| transport_failure("open the billing portal", &err))?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let api_error = response
            .json::<ApiErrorResponse>()
            .ok()
            .and_then(|body| body.error)
            .filter(|value| !value.trim().is_empty());
        return Err(api_error
            .unwrap_or_else(|| format!("Could not open billing portal (status {status}).")));
    }

    response
        .json::<BillingPortalResponse>()
        .map(|body| body.url)
        .map_err(|err| format!("Could not parse billing portal response: {err}"))
}

/// Records why someone is cancelling before the client proceeds to the
/// billing portal. One round trip, so a user who bails after this point is
/// still counted.
pub fn submit_cancellation_intent(reason: &str, note: &str) -> Result<(), String> {
    submit_cancellation_intent_with_base_url(&api_base_url(), reason, note)
}

pub(crate) fn submit_cancellation_intent_with_base_url(
    base_url: &str,
    reason: &str,
    note: &str,
) -> Result<(), String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before managing your plan.".to_string())?;
    let response = http_client()?
        .post(join_url(base_url, "desktop/cancellation_intent"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "reason": reason, "note": note }))
        .send()
        .map_err(|err| transport_failure("send your note", &err))?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let api_error = response
            .json::<ApiErrorResponse>()
            .ok()
            .and_then(|body| body.error)
            .filter(|value| !value.trim().is_empty());
        return Err(api_error
            .unwrap_or_else(|| format!("Could not start the cancel flow (status {status}).")));
    }

    Ok(())
}

pub fn fetch_claude_usage(state: &AppState) -> Result<ClaudeUsage, String> {
    let access_token = state.current_bearer_token().ok_or_else(|| {
        "No Claude AI token captured yet — make sure Claude Code is running and authenticated via Claude AI (not an API key), then try again after the first request passes through the proxy.".to_string()
    })?;

    let resp = http_client()?
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .send()
        .map_err(|e| format!("Request failed: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    parse_claude_usage_response(&body)
}

/// Pure parser for the Anthropic OAuth usage endpoint response. Extracted so
/// schema-drift tests don't need a live HTTP server.
fn parse_claude_usage_response(body: &serde_json::Value) -> Result<ClaudeUsage, String> {
    use chrono::DateTime;

    if let Some(err) = body.get("error") {
        return Err(format!(
            "API error: {}",
            err["message"].as_str().unwrap_or("unknown")
        ));
    }

    let parse_window = |v: &serde_json::Value| -> Option<ClaudeUsageWindow> {
        let utilization = v.get("utilization")?.as_f64()?;
        let resets_at_str = v.get("resets_at")?.as_str()?;
        let resets_at = DateTime::parse_from_rfc3339(resets_at_str).ok()?.to_utc();
        Some(ClaudeUsageWindow {
            utilization,
            resets_at,
        })
    };

    let five_hour = body.get("five_hour").and_then(parse_window);
    let seven_day = body.get("seven_day").and_then(parse_window);

    let extra_usage = body.get("extra_usage").and_then(|e| {
        Some(crate::models::ClaudeExtraUsage {
            is_enabled: e.get("is_enabled")?.as_bool()?,
            monthly_limit: e.get("monthly_limit").and_then(|v| v.as_f64()),
            used_credits: e.get("used_credits").and_then(|v| v.as_f64()),
            utilization: e.get("utilization").and_then(|v| v.as_f64()),
        })
    });

    Ok(ClaudeUsage {
        five_hour,
        seven_day,
        extra_usage,
    })
}

fn evaluate_pricing_status_with_mismatch(
    authenticated: bool,
    local_grace_started_at: DateTime<Utc>,
    local_grace_ends_at: DateTime<Utc>,
    local_grace_active: bool,
    account_sync_error: Option<String>,
    account: Option<HeadroomAccountProfile>,
    claude: ClaudeAccountProfile,
    promo: PricingPromo,
    _last_known_good_plan_tier: Option<ClaudePlanTier>,
    tier_mismatch: Option<TierMismatch>,
) -> HeadroomPricingStatus {
    #[cfg(debug_assertions)]
    let local_grace_active = if INDEFINITE_TRIAL {
        true
    } else {
        local_grace_active
    };
    let needs_authentication = !authenticated && !local_grace_active;
    let mut optimization_allowed = true;
    let mut should_nudge = false;
    let mut nudge_level: u8 = 0;
    let mut gate_reason = None;
    let gate_message: String;
    let mut nudge_threshold_percent = None;
    let mut effective_nudge_thresholds_percent: Option<Vec<f64>> = None;
    let mut disable_threshold_percent = None;
    let mut effective_disable_threshold_percent = None;
    let mut recommended_subscription_tier = None;

    if needs_authentication {
        optimization_allowed = false;
        gate_reason = Some(PricingGateReason::SignInRequired);
        gate_message =
            "Create a Headroom account to unlock your 7-day trial and keep optimization enabled."
                .into();
    } else if let Some(account) = account.as_ref() {
        if account.subscription_active {
            // Clamp Claude only when the Claude-implied tier itself exceeds the
            // paid one. A Codex-only mismatch is enforced by the Codex gate
            // (`fetch_codex_usage`), never here — pausing Claude for a Codex
            // shortfall would break "unlimited with Claude Pro" for a user
            // whose Claude plan matches what they pay for.
            if tier_mismatch
                .as_ref()
                .is_some_and(|m| m.clamped && m.claude_undercovered)
            {
                let gate = paid_plan_gate(
                    &claude.plan_tier,
                    claude.weekly_utilization_pct,
                    account.invite_bonus_percent,
                );
                optimization_allowed = gate.optimization_allowed;
                should_nudge = gate.should_nudge;
                nudge_level = gate.nudge_level;
                gate_reason = gate.gate_reason;
                nudge_threshold_percent = gate.nudge_threshold_percent;
                effective_nudge_thresholds_percent = gate.effective_nudge_thresholds_percent;
                disable_threshold_percent = gate.disable_threshold_percent;
                effective_disable_threshold_percent = gate.effective_disable_threshold_percent;
                recommended_subscription_tier = gate.recommended_subscription_tier;
                gate_message = gate.gate_message;
            } else {
                recommended_subscription_tier = tier_mismatch.as_ref().map(|m| m.recommended_tier);
                gate_message =
                    "Headroom subscription active. Optimization stays fully enabled.".into();
            }
        } else if account.trial_active {
            gate_message = "Your Headroom trial is active with unlimited optimization.".into();
        } else if account.grandfathered {
            // Grandfathered early adopter: capped free tier instead of the hard
            // block, metered by Claude plan tier via the shared gate (Free ->
            // 100%, Pro -> 50%, Max/Unknown -> 25%). Same adoption as the
            // clamped-subscriber path above.
            let gate = paid_plan_gate(
                &claude.plan_tier,
                claude.weekly_utilization_pct,
                account.invite_bonus_percent,
            );
            optimization_allowed = gate.optimization_allowed;
            should_nudge = gate.should_nudge;
            nudge_level = gate.nudge_level;
            gate_reason = gate.gate_reason;
            nudge_threshold_percent = gate.nudge_threshold_percent;
            effective_nudge_thresholds_percent = gate.effective_nudge_thresholds_percent;
            disable_threshold_percent = gate.disable_threshold_percent;
            effective_disable_threshold_percent = gate.effective_disable_threshold_percent;
            recommended_subscription_tier = gate.recommended_subscription_tier;
            gate_message = gate.gate_message;
        } else {
            // Trial ended, no subscription, not grandfathered: hard block. There
            // is no usable free plan post-trial, so optimization stops for every
            // Claude tier until the user upgrades.
            optimization_allowed = false;
            should_nudge = true;
            nudge_level = u8::MAX;
            gate_reason = Some(PricingGateReason::TrialEnded);
            recommended_subscription_tier = headroom_tier_for_claude_plan(&claude.plan_tier);
            gate_message =
                "Your Headroom trial has ended. Upgrade to keep Headroom optimizing your prompts."
                    .into();
        }
    } else if authenticated {
        gate_message =
            "Headroom account connected, but pricing status could not be synced right now. Optimization stays enabled for now."
                .into();
    } else {
        gate_message =
            "Headroom is active during your first 72 hours. Create an account to unlock the 7-day trial before this grace period ends."
                .into();
    }

    // Server-computed usage-band pitch for API-billed orgs: the server fills
    // account.recommendedTier only for api_* organizations (band over
    // user_daily_savings at subscriber-typical thresholds, >=5:1 savings ROI
    // clamp). It outranks every local guess - the Api gate policy's Max
    // ceiling would otherwise pitch Max20x at a light API user.
    if let Some(tier) = account
        .as_ref()
        .and_then(|account| account.recommended_tier)
    {
        recommended_subscription_tier = Some(tier);
    }

    HeadroomPricingStatus {
        authenticated,
        local_grace_started_at,
        local_grace_ends_at,
        local_grace_active,
        account_sync_error,
        needs_authentication,
        optimization_allowed,
        should_nudge,
        nudge_level,
        gate_reason,
        gate_message,
        nudge_threshold_percent,
        effective_nudge_thresholds_percent,
        disable_threshold_percent,
        effective_disable_threshold_percent,
        recommended_subscription_tier,
        tier_mismatch,
        claude,
        codex: None,
        codex_plan_tier: None,
        account,
        launch_discount_active: promo.active_percent_off > 0,
        active_percent_off: promo.active_percent_off,
        pricing_cohorts: promo.cohorts,
        intro_offer: promo.intro_offer,
        plan_prices: promo.plan_prices,
    }
}

/// Pure comparison: returns `(paid_tier, recommended_tier)` when an active
/// subscriber's paid tier is below the tier their confidently-detected Claude
/// plan implies. Uses the live `claude.plan_tier` only — `Unknown`/`Free`
/// (and any cached fallback) yield no recommended tier, so no mismatch fires.
/// Codex differs: `codex_plan` must be `None` when there is no Codex evidence
/// at all (callers pass the merged `cached_codex_profile` tier), while an
/// explicit `Some(Unknown)` — real Codex evidence with an unclassifiable plan
/// — deliberately maps to Max x20 via `headroom_tier_for_codex_plan`.
fn detect_tier_mismatch(
    account: &HeadroomAccountProfile,
    claude: &ClaudeAccountProfile,
    codex_plan: Option<CodexPlanTier>,
) -> Option<(
    HeadroomSubscriptionTier,
    HeadroomSubscriptionTier,
    TierRecommendationSource,
)> {
    if !account.subscription_active {
        return None;
    }
    let paid = account.subscription_tier?;
    // Take the higher-ranked of the Claude- and Codex-implied tiers so a user
    // who routes both is recommended the plan that covers their bigger account.
    let claude_rec = headroom_tier_for_claude_plan(&claude.plan_tier);
    let codex_rec = codex_plan.and_then(|plan| headroom_tier_for_codex_plan(&plan));
    let (recommended, source) = match (claude_rec, codex_rec) {
        (Some(c), Some(x)) if c.rank() > x.rank() => (c, TierRecommendationSource::Claude),
        (Some(c), Some(x)) if x.rank() > c.rank() => (x, TierRecommendationSource::Codex),
        (Some(c), Some(_)) => (c, TierRecommendationSource::Both),
        (Some(c), None) => (c, TierRecommendationSource::Claude),
        (None, Some(x)) => (x, TierRecommendationSource::Codex),
        (None, None) => return None,
    };
    (recommended.rank() > paid.rank()).then_some((paid, recommended, source))
}

/// Detects the mismatch and manages the persisted grace clock. Sets
/// `mismatch_since` on first detection, clears it once resolved, and reports
/// `clamped` after the grace window elapses.
fn resolve_tier_mismatch(
    account: Option<&HeadroomAccountProfile>,
    claude: &ClaudeAccountProfile,
    codex_plan: Option<CodexPlanTier>,
) -> Option<TierMismatch> {
    let (paid_tier, recommended_tier, recommended_source) =
        match account.and_then(|a| detect_tier_mismatch(a, claude, codex_plan)) {
            Some(triple) => triple,
            None => {
                // Clear the grace clock only on an affirmative "no mismatch"
                // (account profile present). A failed account fetch also lands
                // here, and clearing on those used to restart the 14-day clamp
                // window on every transient blip — one flaky poll per two
                // weeks meant under-subscribed users were never clamped.
                if account.is_some() {
                    if let Ok(mut local) = load_or_initialize_local_state() {
                        if local.mismatch_since.is_some() {
                            local.mismatch_since = None;
                            let _ = write_local_state(&local);
                        }
                    }
                }
                return None;
            }
        };

    let mut local = load_or_initialize_local_state().ok()?;
    let since = match local.mismatch_since {
        Some(since) => since,
        None => {
            let now = Utc::now();
            local.mismatch_since = Some(now);
            let _ = write_local_state(&local);
            now
        }
    };

    let grace_ends_at = since + Duration::days(TIER_MISMATCH_GRACE_DAYS);
    // Per-product scope for the clamp. `recommended_source` names the higher
    // recommendation, which is not the same thing: paid Pro with Claude->Max20x
    // and Codex->Max5x reports source Claude, yet both products are
    // undercovered. Recompute each product against the paid tier instead.
    let claude_undercovered = headroom_tier_for_claude_plan(&claude.plan_tier)
        .is_some_and(|rec| rec.rank() > paid_tier.rank());
    let codex_undercovered = codex_plan
        .and_then(|plan| headroom_tier_for_codex_plan(&plan))
        .is_some_and(|rec| rec.rank() > paid_tier.rank());
    Some(TierMismatch {
        paid_tier,
        recommended_tier,
        recommended_source,
        grace_ends_at,
        clamped: Utc::now() > grace_ends_at,
        claude_undercovered,
        codex_undercovered,
    })
}

struct PaidPlanGate {
    optimization_allowed: bool,
    should_nudge: bool,
    nudge_level: u8,
    gate_reason: Option<PricingGateReason>,
    gate_message: String,
    nudge_threshold_percent: Option<f64>,
    effective_nudge_thresholds_percent: Option<Vec<f64>>,
    disable_threshold_percent: Option<f64>,
    effective_disable_threshold_percent: Option<f64>,
    recommended_subscription_tier: Option<HeadroomSubscriptionTier>,
}

/// Which message a weekly gate should render, plus the numbers it needs. The
/// numeric decision is surface-agnostic; each surface (Claude / Codex) formats
/// its own copy from this so the threshold math can never drift between them.
enum WeeklyGateOutcome {
    /// No usable policy (Free tier) or no synced usage yet: allowed, "sync" copy.
    NoData,
    /// Below the first nudge: allowed, "will nudge at X, pause at Y" copy.
    Active { first_nudge: f64, disable: f64 },
    /// Between a nudge threshold and the disable cutoff: allowed, nudge copy.
    Nudging {
        weekly_usage: f64,
        disable: f64,
        level: u8,
    },
    /// At or past the disable cutoff: optimization paused.
    Paused { weekly_usage: f64 },
}

struct WeeklyGateDecision {
    optimization_allowed: bool,
    should_nudge: bool,
    nudge_level: u8,
    nudge_threshold_percent: Option<f64>,
    effective_nudge_thresholds_percent: Option<Vec<f64>>,
    disable_threshold_percent: Option<f64>,
    effective_disable_threshold_percent: Option<f64>,
    recommended_subscription_tier: Option<HeadroomSubscriptionTier>,
    outcome: WeeklyGateOutcome,
}

/// Shared weekly-usage gate math for both Claude and Codex. Applies the invite
/// bonus, compares weekly utilization against the effective nudge/disable
/// thresholds, and returns the numeric decision plus a copy-agnostic outcome.
/// A `None` policy (Free tier) yields `NoData` and stays allowed (100%).
fn evaluate_weekly_gate(
    policy: Option<PricingPolicy>,
    weekly_utilization_pct: Option<f64>,
    invite_bonus_percent: f64,
) -> WeeklyGateDecision {
    let bonus = invite_bonus_percent.clamp(0.0, 50.0);
    let nudge_threshold_percent = policy
        .as_ref()
        .map(|policy| policy.nudge_thresholds_percent[0]);
    let effective_nudge_thresholds_percent: Option<Vec<f64>> = policy.as_ref().map(|policy| {
        policy
            .nudge_thresholds_percent
            .iter()
            .map(|n| n + bonus)
            .collect()
    });
    let disable_threshold_percent = policy
        .as_ref()
        .map(|policy| policy.disable_threshold_percent);
    let effective_disable_threshold_percent = policy.as_ref().map(|policy| {
        (policy.disable_threshold_percent + invite_bonus_percent)
            .min(policy.disable_threshold_percent + 50.0)
    });
    let recommended_subscription_tier = policy
        .as_ref()
        .map(|policy| policy.recommended_tier.clone());

    let mut optimization_allowed = true;
    let mut should_nudge = false;
    let mut nudge_level: u8 = 0;
    let outcome;

    if let (Some(weekly_usage), Some(nudges), Some(disable)) = (
        weekly_utilization_pct,
        effective_nudge_thresholds_percent.as_ref(),
        effective_disable_threshold_percent,
    ) {
        if weekly_usage >= disable {
            optimization_allowed = false;
            outcome = WeeklyGateOutcome::Paused { weekly_usage };
        } else {
            nudge_level = nudges.iter().filter(|t| weekly_usage >= **t).count() as u8;
            should_nudge = nudge_level > 0;
            outcome = if should_nudge {
                WeeklyGateOutcome::Nudging {
                    weekly_usage,
                    disable,
                    level: nudge_level,
                }
            } else {
                WeeklyGateOutcome::Active {
                    first_nudge: nudges[0],
                    disable,
                }
            };
        }
    } else {
        outcome = WeeklyGateOutcome::NoData;
    }

    WeeklyGateDecision {
        optimization_allowed,
        should_nudge,
        nudge_level,
        nudge_threshold_percent,
        effective_nudge_thresholds_percent,
        disable_threshold_percent,
        effective_disable_threshold_percent,
        recommended_subscription_tier,
        outcome,
    }
}

fn paid_plan_gate(
    tier: &ClaudePlanTier,
    weekly_utilization_pct: Option<f64>,
    invite_bonus_percent: f64,
) -> PaidPlanGate {
    let decision = evaluate_weekly_gate(
        pricing_policy_for_plan(tier),
        weekly_utilization_pct,
        invite_bonus_percent,
    );
    let gate_reason = matches!(decision.outcome, WeeklyGateOutcome::Paused { .. })
        .then_some(PricingGateReason::WeeklyUsageLimitReached);
    let gate_message = match decision.outcome {
        WeeklyGateOutcome::Paused { weekly_usage } => format!(
            "Headroom is paused because you've reached {:.1}% of weekly Claude usage. Upgrade to raise your limit.",
            weekly_usage
        ),
        WeeklyGateOutcome::Nudging {
            weekly_usage,
            disable,
            level,
        } => format_nudge_message("Claude", weekly_usage, disable, level),
        WeeklyGateOutcome::Active {
            first_nudge,
            disable,
        } => format!(
            "Headroom is active. It will start nudging at {:.1}% and pause at {:.1}% of weekly Claude usage for your detected plan.",
            first_nudge, disable
        ),
        WeeklyGateOutcome::NoData => "Headroom is active. Send a Claude Code message through Headroom to sync your current weekly usage and pricing threshold.".into(),
    };

    PaidPlanGate {
        optimization_allowed: decision.optimization_allowed,
        should_nudge: decision.should_nudge,
        nudge_level: decision.nudge_level,
        gate_reason,
        gate_message,
        nudge_threshold_percent: decision.nudge_threshold_percent,
        effective_nudge_thresholds_percent: decision.effective_nudge_thresholds_percent,
        disable_threshold_percent: decision.disable_threshold_percent,
        effective_disable_threshold_percent: decision.effective_disable_threshold_percent,
        recommended_subscription_tier: decision.recommended_subscription_tier,
    }
}

fn format_nudge_message(product: &str, weekly_usage: f64, disable: f64, level: u8) -> String {
    match level {
        1 => format!(
            "You're at {:.1}% of weekly {product} usage. Upgrade Headroom to keep optimization through {:.1}%.",
            weekly_usage, disable
        ),
        2 => format!(
            "You're at {:.1}% of weekly {product} usage. Headroom pauses at {:.1}% on the free plan — upgrade now to keep going.",
            weekly_usage, disable
        ),
        _ => format!(
            "You're at {:.1}% of weekly {product} usage. Headroom will pause at {:.1}% — upgrade now to avoid losing optimization.",
            weekly_usage, disable
        ),
    }
}

pub fn detect_claude_profile(state: &AppState) -> ClaudeAccountProfile {
    state.cached_claude_profile()
}

/// Decode a JWT's payload segment (no signature verification) into JSON. Codex
/// id/access tokens are base64url without padding; tolerate either form.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let trimmed = payload_b64.trim_end_matches('=');
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// Codex billing-type derivation, mirroring Claude's raw `billing_type` slot.
/// Free/Unknown carry nothing; paid personal plans are `"subscription"`; org
/// seats report the plan string so the server can distinguish Business seats.
fn codex_billing_type(plan: &CodexPlanTier, has_org: bool) -> Option<String> {
    match plan {
        CodexPlanTier::Free | CodexPlanTier::Unknown => None,
        CodexPlanTier::Team
        | CodexPlanTier::Business
        | CodexPlanTier::SelfServeBusinessUsageBased
        | CodexPlanTier::SelfServeBusinessProlite
        | CodexPlanTier::Enterprise
        | CodexPlanTier::EnterpriseCbpUsageBased
        | CodexPlanTier::Edu
        | CodexPlanTier::Education => Some(plan.as_header_str().to_string()),
        CodexPlanTier::Go | CodexPlanTier::Plus | CodexPlanTier::ProLite | CodexPlanTier::Pro => {
            Some(if has_org {
                plan.as_header_str().to_string()
            } else {
                "subscription".to_string()
            })
        }
    }
}

/// Sanitize an unrecognized `chatgpt_plan_type` claim for the identity header:
/// lowercase, `[a-z0-9_-]` only, capped at 64 chars. `None` when nothing
/// survives — a claim that is all junk carries no signal.
fn sanitize_plan_claim(raw: &str) -> Option<String> {
    let clean: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    (!clean.is_empty()).then_some(clean)
}

/// Build a [`CodexAccountProfile`] from `~/.codex/auth.json`. `plan_tier` and
/// `account_uuid` are also available from live traffic (`state.codex_plan_tier`
/// + the access-token bearer), so this prefers a live, classified plan tier
/// over the on-disk id_token when present. `email` and `organization_type` only
/// exist in the id_token, so they require the file. Returns `None` only when
/// nothing at all is known (no file and no live capture).
pub fn detect_codex_profile(state: &AppState) -> Option<CodexAccountProfile> {
    let live_tier = state.codex_plan_tier();
    let path = dirs::home_dir()?.join(".codex").join("auth.json");
    let on_disk = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());

    // No file: fall back to whatever the live access-token decode captured.
    let Some(root) = on_disk else {
        if matches!(live_tier, CodexPlanTier::Unknown) {
            return None;
        }
        return Some(CodexAccountProfile {
            plan_tier: Some(live_tier),
            plan_detection_source: Some("access_token".to_string()),
            ..Default::default()
        });
    };

    let tokens = root.get("tokens");
    let id_token = tokens
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str());
    let payload = id_token.and_then(decode_jwt_payload);
    let auth = payload
        .as_ref()
        .and_then(|p| p.get("https://api.openai.com/auth"));

    let email = payload
        .as_ref()
        .and_then(|p| p.get("email"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let account_uuid = auth
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            tokens
                .and_then(|t| t.get("account_id"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string);

    let organization_type = auth
        .and_then(|a| a.get("organizations"))
        .and_then(|v| v.as_array())
        .and_then(|orgs| orgs.first())
        .and_then(|o| o.get("role"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Prefer the live, classified tier; fall back to the id_token claim.
    let raw_claim = auth
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str());
    let (plan_tier, source) = if !matches!(live_tier, CodexPlanTier::Unknown) {
        (live_tier, "access_token")
    } else {
        match raw_claim.map(CodexPlanTier::from_claim) {
            Some(tier) => (tier, "id_token"),
            None => (CodexPlanTier::Unknown, "none"),
        }
    };
    // Keep the raw claim only when it exists but decodes to Unknown: that is
    // a plan value OpenAI ships and we don't know yet (Business Premium seats
    // are the expected next one). Known tiers carry nothing extra.
    let plan_raw = matches!(plan_tier, CodexPlanTier::Unknown)
        .then(|| raw_claim.and_then(sanitize_plan_claim))
        .flatten();

    let billing_type = codex_billing_type(&plan_tier, organization_type.is_some());

    Some(CodexAccountProfile {
        email,
        account_uuid,
        plan_tier: Some(plan_tier),
        plan_raw,
        organization_type,
        rate_limit_tier: None,
        billing_type,
        plan_detection_source: Some(source.to_string()),
    })
}

/// Result of a live profile detection. `error_is_transient` is true when the
/// fetch failed for a reason that clears itself once a fresh bearer flows
/// through the proxy (stale-token 401/403, 5xx, or a network blip), signalling
/// the caller to keep serving the last known-good profile rather than flashing
/// a banner.
pub struct ProfileDetection {
    pub profile: ClaudeAccountProfile,
    pub error_is_transient: bool,
}

pub fn detect_claude_profile_uncached(state: &AppState) -> ProfileDetection {
    let Some(token) = state.current_bearer_token() else {
        // No token yet — proxy hasn't seen a request through. Return a minimal
        // profile so the app can show "send a message first" messaging.
        return ProfileDetection {
            profile: ClaudeAccountProfile {
                auth_method: ClaudeAuthMethod::Unknown,
                email: None,
                display_name: None,
                account_uuid: None,
                organization_uuid: None,
                billing_type: None,
                account_created_at: None,
                subscription_created_at: None,
                has_extra_usage_enabled: false,
                plan_tier: ClaudePlanTier::Unknown,
                plan_detection_source: None,
                organization_type: None,
                rate_limit_tier: None,
                user_rate_limit_tier: None,
                seat_tier: None,
                weekly_utilization_pct: None,
                weekly_resets_at: None,
                five_hour_utilization_pct: None,
                extra_usage_monthly_limit: None,
                profile_fetch_error: None,
            },
            error_is_transient: false,
        };
    };

    let (profile, profile_fetch_error, error_is_transient) = match fetch_oauth_profile(&token) {
        Ok(p) => (Some(p), None, false),
        Err(err) => (None, Some(err.message), err.transient),
    };
    let usage = fetch_claude_usage(state).ok();
    // Keep the latest window shape for the identity touch (same idea as
    // `codex_rate_limits`); a failed fetch keeps the last known value.
    if let Some(u) = usage.as_ref() {
        *state.claude_usage_windows.lock() = Some(claude_usage_windows_summary(u));
    }

    let (plan_tier, plan_detection_source) = if let Some(ref p) = profile {
        detect_plan_tier_from_profile(p)
    } else {
        (ClaudePlanTier::Unknown, None)
    };

    // Persist the classifier output when it carries real signal so the
    // pricing gate can fall back to it next time Anthropic returns a sparse
    // profile and we'd otherwise classify as Unknown. The helper filters
    // Unknown internally.
    state.record_known_good_plan_tier(&plan_tier);

    let detected = ClaudeAccountProfile {
        auth_method: ClaudeAuthMethod::ClaudeAiOauth,
        email: profile.as_ref().and_then(|p| p.account.email.clone()),
        display_name: profile
            .as_ref()
            .and_then(|p| p.account.display_name.clone()),
        account_uuid: profile.as_ref().and_then(|p| p.account.uuid.clone()),
        organization_uuid: profile
            .as_ref()
            .and_then(|p| p.organization.as_ref().and_then(|o| o.uuid.clone())),
        billing_type: profile
            .as_ref()
            .and_then(|p| p.organization.as_ref().and_then(|o| o.billing_type.clone())),
        account_created_at: profile.as_ref().and_then(|p| p.account.created_at),
        subscription_created_at: profile.as_ref().and_then(|p| {
            p.organization
                .as_ref()
                .and_then(|o| o.subscription_created_at)
        }),
        has_extra_usage_enabled: profile
            .as_ref()
            .and_then(|p| p.organization.as_ref().map(|o| o.has_extra_usage_enabled))
            .unwrap_or(false),
        plan_tier,
        plan_detection_source,
        organization_type: profile.as_ref().and_then(|p| {
            p.organization
                .as_ref()
                .and_then(|o| o.organization_type.clone())
        }),
        rate_limit_tier: profile.as_ref().and_then(|p| {
            p.organization
                .as_ref()
                .and_then(|o| o.rate_limit_tier.clone())
        }),
        user_rate_limit_tier: profile.as_ref().and_then(|p| {
            p.organization
                .as_ref()
                .and_then(|o| o.user_rate_limit_tier.clone())
        }),
        seat_tier: profile
            .as_ref()
            .and_then(|p| p.organization.as_ref().and_then(|o| o.seat_tier.clone())),
        weekly_utilization_pct: usage
            .as_ref()
            .and_then(|u| u.seven_day.as_ref().map(|w| w.utilization)),
        weekly_resets_at: usage
            .as_ref()
            .and_then(|u| u.seven_day.as_ref().map(|w| w.resets_at)),
        five_hour_utilization_pct: usage
            .as_ref()
            .and_then(|u| u.five_hour.as_ref().map(|w| w.utilization)),
        extra_usage_monthly_limit: usage
            .as_ref()
            .and_then(|u| u.extra_usage.as_ref().and_then(|e| e.monthly_limit)),
        profile_fetch_error,
    };

    ProfileDetection {
        profile: detected,
        error_is_transient,
    }
}

/// Failure from the OAuth profile fetch. `transient` marks the conditions
/// that resolve on their own once a fresh bearer flows through the proxy
/// (network blip, 5xx, or a 401/403 from a stale captured token during the
/// token-rotation gap). Callers suppress the banner for transient errors and
/// keep serving the last known-good profile instead.
struct ProfileFetchError {
    message: String,
    transient: bool,
}

fn fetch_oauth_profile(token: &str) -> Result<ClaudeOauthProfile, ProfileFetchError> {
    let response = http_client()
        .map_err(|message| ProfileFetchError {
            message,
            transient: true,
        })?
        .get("https://api.anthropic.com/api/oauth/profile")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .send()
        .map_err(|_| ProfileFetchError {
            message: "Couldn't reach Anthropic to refresh your Claude plan. Check your internet \
                      connection and we'll try again shortly."
                .to_string(),
            transient: true,
        })?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let (message, transient) = if status >= 500 {
            (
                format!(
                    "Anthropic is having trouble serving your Claude plan right now (HTTP \
                     {status}). We'll keep trying."
                ),
                true,
            )
        } else if status == 401 || status == 403 {
            (
                "Anthropic rejected our request for your Claude plan. Try signing out of Claude \
                 Code and back in."
                    .to_string(),
                true,
            )
        } else {
            (
                format!(
                    "Anthropic returned an unexpected response for your Claude plan (HTTP \
                     {status}). We'll try again shortly."
                ),
                false,
            )
        };
        return Err(ProfileFetchError { message, transient });
    }

    // Same split as the activation path above (RUST-58): `.json()` collapses a
    // body-read failure and a serde mismatch into one opaque "error decoding
    // response body" (RUST-7J). They need opposite handling -- a dropped
    // connection mid-body is transient and must NOT be a permanent error the
    // user has to report, while a schema change from Anthropic is exactly the
    // thing we want to see.
    let raw = response.text().map_err(|err| {
        if !is_transient_transport_error(&err) {
            sentry::capture_message(
                &format!("Could not read Claude OAuth profile: {err}"),
                sentry::Level::Error,
            );
        }
        ProfileFetchError {
            message: "Couldn't finish reading your Claude plan from Anthropic. We'll try again \
                      shortly."
                .to_string(),
            transient: true,
        }
    })?;
    let body: serde_json::Value = serde_json::from_str(&raw).map_err(|err| {
        sentry::with_scope(
            |scope| {
                // Serde errors vary by line/column; one fingerprint keeps them
                // a single issue.
                scope.set_fingerprint(Some(&["claude-oauth-profile-parse-error"]));
                // Snippet only when the body isn't JSON at all (HTML error page,
                // empty body, captive portal). Valid JSON is described by `err`
                // and carries the user's account details.
                if serde_json::from_str::<serde_json::Value>(&raw).is_err() {
                    let snippet: String = raw.chars().take(300).collect();
                    scope.set_extra("body_snippet", snippet.into());
                }
            },
            || {
                sentry::capture_message(
                    &format!("Could not parse Claude OAuth profile: {err}"),
                    sentry::Level::Error,
                )
            },
        );
        ProfileFetchError {
            message: "We couldn't read the response from Anthropic for your Claude plan. Please \
                      report this if it keeps happening."
                .to_string(),
            transient: false,
        }
    })?;

    parse_oauth_profile_value(&body).ok_or_else(|| ProfileFetchError {
        message: "Anthropic's response didn't include your Claude account details. Please report \
                  this if it keeps happening."
            .to_string(),
        transient: false,
    })
}

fn parse_oauth_profile_value(value: &serde_json::Value) -> Option<ClaudeOauthProfile> {
    let root = value
        .get("profile")
        .or_else(|| value.get("data"))
        .unwrap_or(value);
    let account_value = root.get("account").unwrap_or(root);

    Some(ClaudeOauthProfile {
        account: ClaudeOauthProfileAccount {
            uuid: json_string(account_value, &["uuid", "account_uuid"]),
            email: json_string(account_value, &["email", "email_address"]),
            display_name: json_string(account_value, &["display_name", "displayName"]),
            created_at: json_datetime(account_value, &["created_at", "createdAt"]),
        },
        organization: root
            .get("organization")
            .and_then(parse_oauth_profile_organization),
    })
}

fn parse_oauth_profile_organization(
    value: &serde_json::Value,
) -> Option<ClaudeOauthProfileOrganization> {
    Some(ClaudeOauthProfileOrganization {
        uuid: json_string(value, &["uuid", "organization_uuid"]),
        billing_type: json_string(value, &["billing_type", "billingType"]),
        subscription_created_at: json_datetime(
            value,
            &["subscription_created_at", "subscriptionCreatedAt"],
        ),
        has_extra_usage_enabled: json_bool(
            value,
            &["has_extra_usage_enabled", "hasExtraUsageEnabled"],
        )
        .unwrap_or(false),
        organization_type: json_string(value, &["organization_type", "organizationType"]),
        rate_limit_tier: json_string(value, &["rate_limit_tier", "rateLimitTier"]),
        user_rate_limit_tier: json_string(value, &["user_rate_limit_tier", "userRateLimitTier"]),
        seat_tier: json_string(value, &["seat_tier", "seatTier"]),
    })
}

/// Map Anthropic's per-seat `seat_tier` taxonomy to a plan tier. Values
/// observed in the wild (Claude Code parses the field as an opaque string, so
/// this list grows via trial-identity telemetry): `team_standard` seats carry
/// Pro-scale Claude limits, `team_tier_1` (premium) carries Max-5x-equivalent
/// limits, `team_tier_2` is the higher premium level. Unknown values return
/// `None` so callers fall through to the pre-seat-tier behavior.
fn plan_tier_from_seat_tier(seat_tier: &str) -> Option<ClaudePlanTier> {
    match seat_tier.trim().to_ascii_lowercase().as_str() {
        "team_standard" => Some(ClaudePlanTier::Pro),
        "team_tier_1" => Some(ClaudePlanTier::Max5x),
        "team_tier_2" => Some(ClaudePlanTier::Max20x),
        _ => None,
    }
}

fn detect_plan_tier_from_profile(profile: &ClaudeOauthProfile) -> (ClaudePlanTier, Option<String>) {
    let Some(org) = profile.organization.as_ref() else {
        return (ClaudePlanTier::Free, Some("oauth_profile.account".into()));
    };

    if let Some(rate_limit_tier) = org.rate_limit_tier.as_deref() {
        let normalized = rate_limit_tier.trim().to_ascii_lowercase();
        // Anthropic ships both orderings in the wild: "claude_max_20x" and
        // "default_claude_max_x20" (same for 5x/x5). Match either.
        if normalized.contains("20x") || normalized.contains("x20") {
            return (
                ClaudePlanTier::Max20x,
                Some("oauth_profile.organization.rateLimitTier".into()),
            );
        }
        if normalized.contains("5x") || normalized.contains("x5") {
            return (
                ClaudePlanTier::Max5x,
                Some("oauth_profile.organization.rateLimitTier".into()),
            );
        }
        // Per-seat entitlement on Team orgs, checked after the explicit
        // multiplier strings above (an explicit quota tier wins) and before
        // the org-wide "raven" fallback. Grounded in observed captures: a
        // `team_tier_1` (premium) seat reports `default_claude_max_5x`
        // limits, while `team_standard` seats get Pro-scale limits.
        if let Some(tier) = org.seat_tier.as_deref().and_then(plan_tier_from_seat_tier) {
            return (tier, Some("oauth_profile.organization.seatTier".into()));
        }
        // Anthropic's internal label for Team-plan rate limits, reached only
        // when `seat_tier` is absent or unrecognized. Show Max20x pricing
        // rather than falling through to Pro. NOTE: this is intentionally NOT
        // parity with Codex Team/Business (-> Max x5). A Claude Team seat
        // grants Claude usage at Max-tier limits, whereas a Codex/ChatGPT
        // Business seat grants a far smaller Codex allowance, so the right
        // recommendation differs by product. Do not "unify" them.
        if normalized.contains("raven") {
            return (
                ClaudePlanTier::Max20x,
                Some("oauth_profile.organization.rateLimitTier".into()),
            );
        }
        if normalized == "default_claude_ai" {
            let organization_type = org.organization_type.as_deref().unwrap_or_default();
            if organization_type.eq_ignore_ascii_case("claude_max") {
                return (
                    ClaudePlanTier::Max5x,
                    Some("oauth_profile.organization.organizationType".into()),
                );
            }
            if organization_type.eq_ignore_ascii_case("claude_enterprise") {
                return (
                    ClaudePlanTier::Max20x,
                    Some("oauth_profile.organization.organizationType".into()),
                );
            }
            if organization_type.eq_ignore_ascii_case("claude_pro") {
                return (
                    ClaudePlanTier::Pro,
                    Some("oauth_profile.organization.organizationType".into()),
                );
            }
        }
    }

    // Team profiles whose `rate_limit_tier` is missing or unrecognized can
    // still classify from the seat alone.
    if let Some(tier) = org.seat_tier.as_deref().and_then(plan_tier_from_seat_tier) {
        return (tier, Some("oauth_profile.organization.seatTier".into()));
    }

    if let Some(organization_type) = org.organization_type.as_deref() {
        let normalized = organization_type.trim().to_ascii_lowercase();
        if normalized == "claude_max" {
            return (
                ClaudePlanTier::Max5x,
                Some("oauth_profile.organization.organizationType".into()),
            );
        }
        if normalized == "claude_enterprise" {
            return (
                ClaudePlanTier::Max20x,
                Some("oauth_profile.organization.organizationType".into()),
            );
        }
        if normalized == "claude_pro" {
            return (
                ClaudePlanTier::Pro,
                Some("oauth_profile.organization.organizationType".into()),
            );
        }
        // Anthropic API console orgs (api_individual, api_*): pay-per-token
        // accounts, not Claude subscriptions. Both observed shapes land here:
        // prepaid with subscription_created_at set (previously fell through to
        // Unknown and fired plan_tier_unknown - RUST-B2) and
        // auto_api_evaluation without subscription_created_at (previously
        // misread as Free). The server prices these by usage band; see
        // HeadroomAccountProfile::recommended_tier.
        if normalized.starts_with("api") {
            return (
                ClaudePlanTier::Api,
                Some("oauth_profile.organization.organizationType".into()),
            );
        }
        if normalized == "claude_free" || normalized == "free" {
            return (
                ClaudePlanTier::Free,
                Some("oauth_profile.organization.organizationType".into()),
            );
        }
    }

    if org.subscription_created_at.is_none() {
        return (
            ClaudePlanTier::Free,
            Some("oauth_profile.organization.subscriptionCreatedAt".into()),
        );
    }

    log_unknown_plan_tier_once(profile);
    (
        ClaudePlanTier::Unknown,
        Some("oauth_profile.organization".into()),
    )
}

/// Capture the raw classification fields whenever `detect_plan_tier_from_profile`
/// falls into the `Unknown` branch — i.e., the user has an Anthropic
/// organization with `subscription_created_at` set but neither
/// `organization_type` nor `rate_limit_tier` matches our enum. Almost
/// certainly Team/Workspace/Enterprise plans we haven't enumerated.
///
/// Currently those users bypass the pricing gate entirely, which means
/// paying Anthropic customers get Headroom for free. Goal of this telemetry
/// is to learn which taxonomy strings to add to the detection (or to a new
/// "treat as Pro" fallback) before changing the gate policy.
///
/// Deduped on content — Sentry sees one event per distinct
/// (organization_type, rate_limit_tier, has_subscription, billing_type)
/// combo across the lifetime of the desktop process.
fn log_unknown_plan_tier_once(profile: &ClaudeOauthProfile) {
    use std::collections::HashSet;
    use std::hash::{DefaultHasher, Hash, Hasher};
    use std::sync::OnceLock;

    static SEEN: OnceLock<parking_lot::Mutex<HashSet<u64>>> = OnceLock::new();

    let org = profile.organization.as_ref();
    let organization_type = org
        .and_then(|o| o.organization_type.as_deref())
        .unwrap_or("");
    let rate_limit_tier = org.and_then(|o| o.rate_limit_tier.as_deref()).unwrap_or("");
    let user_rate_limit_tier = org
        .and_then(|o| o.user_rate_limit_tier.as_deref())
        .unwrap_or("");
    let seat_tier = org.and_then(|o| o.seat_tier.as_deref()).unwrap_or("");
    let billing_type = org.and_then(|o| o.billing_type.as_deref()).unwrap_or("");
    let has_subscription_created_at = org
        .and_then(|o| o.subscription_created_at.as_ref())
        .is_some();

    let mut hasher = DefaultHasher::new();
    organization_type.hash(&mut hasher);
    rate_limit_tier.hash(&mut hasher);
    user_rate_limit_tier.hash(&mut hasher);
    seat_tier.hash(&mut hasher);
    billing_type.hash(&mut hasher);
    has_subscription_created_at.hash(&mut hasher);
    let key = hasher.finish();

    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(HashSet::new()));
    if !seen.lock().insert(key) {
        return;
    }

    let payload = serde_json::json!({
        "organization_type": organization_type,
        "rate_limit_tier": rate_limit_tier,
        "user_rate_limit_tier": user_rate_limit_tier,
        "seat_tier": seat_tier,
        "billing_type": billing_type,
        "has_subscription_created_at": has_subscription_created_at,
    });
    sentry::capture_message(
        &format!("plan_tier_unknown: {payload}"),
        sentry::Level::Warning,
    );
}

fn json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|entry| entry.as_str()))
        .map(str::to_string)
}

fn json_bool(value: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|entry| entry.as_bool()))
}

fn json_datetime(value: &serde_json::Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(|entry| entry.as_str())
            .and_then(|entry| DateTime::parse_from_rfc3339(entry).ok())
            .map(|entry| entry.to_utc())
    })
}

fn remote_account_to_profile(value: RemoteAccountResponse) -> HeadroomAccountProfile {
    HeadroomAccountProfile {
        email: value.email,
        trial_started_at: value.trial_started_at,
        trial_ends_at: value.trial_ends_at,
        trial_usage_days_left: value.trial_usage_days_left,
        trial_active: value.trial_active,
        subscription_active: value.subscription_active,
        subscription_tier: value.subscription_tier,
        subscription_started_at: value.subscription_started_at,
        subscription_renews_at: value.subscription_renews_at,
        subscription_amount_cents: value.subscription_amount_cents,
        subscription_billing_period: value.subscription_billing_period,
        subscription_discount_duration: value.subscription_discount_duration,
        subscription_discount_duration_in_months: value.subscription_discount_duration_in_months,
        subscription_cancel_at_period_end: value.subscription_cancel_at_period_end,
        subscription_ends_at: value.subscription_ends_at,
        subscription_renewal_cents: value.subscription_renewal_cents,
        subscription_renewal_ends_at: value.subscription_renewal_ends_at,
        subscription_pending_tier: value.subscription_pending_tier,
        subscription_pending_billing_period: value.subscription_pending_billing_period,
        subscription_pending_effective_at: value.subscription_pending_effective_at,
        invite_code: value.invite_code,
        accepted_invites_count: value.accepted_invites_count,
        invite_bonus_percent: value.invite_bonus_percent.min(50.0).max(0.0),
        upgrade_action: value.upgrade_action,
        recommended_tier: value.recommended_tier,
        grandfathered: value.grandfathered,
    }
}

/// Consecutive background polls answered 401. Tolerating a single 401 keeps a
/// user signed in through server blips, but a *revoked* session answers 401
/// forever — without an escalation path the app showed "authenticated" with a
/// permanent confusing banner until reinstall.
// Alarms for installs that keep running while the backend never hears from
// them (user 861, Aug 2026: the app worked daily for five days while
// extraheadroom.com was unreachable from the machine, so last_active_at froze
// and a check-in email went to a happy customer). Pricing fails open on both
// failure classes, so nothing is visible to the user; Sentry is the only
// channel left, and it lives on a different host than the blocked backend.
const SILENT_ALARM_HOURS: i64 = 24;
/// How long the current process must have been failing before the
/// server-silent alarm may fire, so a laptop waking from a weekend sleep
/// doesn't alarm on the one refresh that runs before Wi-Fi reassociates.
const SERVER_SILENT_MIN_FAILING_SECS: u64 = 15 * 60;
/// Successful contacts are persisted at most this often; the pricing poll
/// would otherwise rewrite the state file on every refresh.
const CONTACT_STAMP_MIN_INTERVAL_HOURS: i64 = 1;

static SERVER_SILENT_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static AUTH_SILENT_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static GRACE_FAILING_SINCE: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// Hours the backend has been unreachable, if past the alarm window. Falls
/// back to first_seen_at so a machine that never reached us still alarms
/// (install-stall class) without firing on a fresh install.
fn server_silent_hours(local: &LocalPricingState, now: DateTime<Utc>) -> Option<i64> {
    let baseline = local.last_server_contact_at.unwrap_or(local.first_seen_at);
    let hours = (now - baseline).num_hours();
    (hours >= SILENT_ALARM_HOURS).then_some(hours)
}

/// Hours the authenticated sync has been failing while the backend itself is
/// provably reachable, if past the alarm window. None while the backend is
/// unreachable or was never reached: the server-silent alarm owns that case.
fn auth_silent_hours(local: &LocalPricingState, now: DateTime<Utc>) -> Option<i64> {
    let reachable = local
        .last_server_contact_at
        .is_some_and(|t| (now - t).num_hours() < SILENT_ALARM_HOURS);
    if !reachable {
        return None;
    }
    let baseline = local.last_account_sync_ok_at.unwrap_or(local.first_seen_at);
    let hours = (now - baseline).num_hours();
    (hours >= SILENT_ALARM_HOURS).then_some(hours)
}

fn maybe_report_server_silent(local: &LocalPricingState, identity: &IdentityPayload, err: &str) {
    let failing_long_enough = {
        let mut since = GRACE_FAILING_SINCE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let start = since.get_or_insert_with(std::time::Instant::now);
        start.elapsed().as_secs() >= SERVER_SILENT_MIN_FAILING_SECS
    };
    if !failing_long_enough {
        return;
    }
    let Some(hours) = server_silent_hours(local, Utc::now()) else {
        return;
    };
    if SERVER_SILENT_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    report_silent(
        "desktop running but backend-silent past the alarm window",
        hours,
        identity,
        err,
    );
}

fn maybe_report_auth_silent(local: &LocalPricingState, identity: &IdentityPayload, err: &str) {
    let Some(hours) = auth_silent_hours(local, Utc::now()) else {
        return;
    };
    if AUTH_SILENT_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    report_silent(
        "desktop auth-silent: backend reachable but authenticated sync failing",
        hours,
        identity,
        err,
    );
}

/// Fixed message + fingerprint per class so each stays one Sentry issue; the
/// variable detail rides in extras (same pattern as activation-parse-error).
/// claude_email is included so support can map the event to a customer — the
/// usual sentry user tag is absent here, since set_sentry_user needs the very
/// account fetch that is failing.
fn report_silent(message: &str, hours: i64, identity: &IdentityPayload, err: &str) {
    sentry::with_scope(
        |scope| {
            scope.set_fingerprint(Some(&[message]));
            scope.set_extra("hours_silent", hours.into());
            scope.set_extra("error", err.to_string().into());
            if let Some(email) = identity.claude_email.as_deref() {
                scope.set_extra("claude_email", email.to_string().into());
            }
        },
        || sentry::capture_message(message, sentry::Level::Warning),
    );
}

/// Stamp a successful authenticated sync (which also proves reachability),
/// persisting at most once per CONTACT_STAMP_MIN_INTERVAL_HOURS.
fn stamp_account_sync_ok(local: &mut LocalPricingState) {
    let now = Utc::now();
    let due = local
        .last_account_sync_ok_at
        .is_none_or(|t| now - t > Duration::hours(CONTACT_STAMP_MIN_INTERVAL_HOURS));
    if !due {
        return;
    }
    local.last_account_sync_ok_at = Some(now);
    local.last_server_contact_at = Some(now);
    if let Err(err) = write_local_state(local) {
        log::warn!("could not persist account sync stamp: {err}");
    }
}

static CONSECUTIVE_UNAUTHORIZED_SYNCS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);
const MAX_CONSECUTIVE_UNAUTHORIZED_SYNCS: u32 = 3;

fn merge_background_account_sync(
    session_token: Option<&str>,
    sync_result: Result<RemoteAccountResponse, RemoteAccountSyncError>,
) -> (bool, Option<HeadroomAccountProfile>, Option<String>) {
    use std::sync::atomic::Ordering;

    if session_token.is_none() {
        return (false, None, None);
    }

    match sync_result {
        // Background polling should not silently drop the locally stored session.
        // Explicit auth-required actions still clear the token if the server says it
        // is expired, but passive refreshes keep the user signed in locally.
        Ok(account) => {
            CONSECUTIVE_UNAUTHORIZED_SYNCS.store(0, Ordering::Relaxed);
            (true, Some(remote_account_to_profile(account)), None)
        }
        Err(RemoteAccountSyncError::Unauthorized) => {
            let unauthorized = CONSECUTIVE_UNAUTHORIZED_SYNCS.fetch_add(1, Ordering::Relaxed) + 1;
            if unauthorized >= MAX_CONSECUTIVE_UNAUTHORIZED_SYNCS {
                return (
                    false,
                    None,
                    Some("Your Headroom session has expired. Please sign in again.".into()),
                );
            }
            (
                true,
                None,
                Some("Headroom account connected, but your plan details could not be refreshed. Sign in again if this keeps happening.".into()),
            )
        }
        // Network failures carry no evidence about the session; never count
        // them toward escalation.
        Err(RemoteAccountSyncError::Other(_)) => (
            true,
            None,
            Some(
                "Headroom account connected, but your plan details are unavailable right now."
                    .into(),
            ),
        ),
    }
}

/// Cached paywall-first flag: false until a config fetch has ever said
/// otherwise. False on any failure = exact legacy onboarding (revert path).
pub fn paywall_first_flag() -> bool {
    load_or_initialize_local_state()
        .ok()
        .and_then(|s| s.paywall_first)
        .unwrap_or(false)
}

/// Same cached flag, but on the first ever read waits for one bounded config
/// fetch. Keeps cold launches from missing their server bucket just because the
/// background warmer has not finished yet.
pub fn paywall_first_flag_or_refresh() -> bool {
    let Ok(local) = load_or_initialize_local_state() else {
        return false;
    };
    if let Some(flag) = local.paywall_first {
        return flag;
    }
    refresh_paywall_first_flag();
    paywall_first_flag()
}

/// Refresh the paywall-first flag from the unauthenticated config endpoint.
/// Called once from `setup()` on a background thread, and synchronously only
/// when the frontend asks for launch flags before any cache exists.
pub fn refresh_paywall_first_flag() {
    let Some(config) = fetch_public_config() else {
        return;
    };
    if let Ok(mut local) = load_or_initialize_local_state() {
        if local.paywall_first != Some(config.paywall_first) {
            local.paywall_first = Some(config.paywall_first);
            let _ = write_local_state(&local);
        }
    }
}

fn load_or_initialize_local_state() -> Result<LocalPricingState, String> {
    let path = local_state_path();
    if let Ok(bytes) = std::fs::read(&path) {
        match serde_json::from_slice::<LocalPricingState>(&bytes) {
            Ok(state) => return Ok(state),
            // Only reachable now for a truncated/non-JSON file: every field
            // defaults, so a schema change alone parses. write_local_state
            // below would overwrite it, so keep a copy first.
            Err(err) => crate::client_adapters::quarantine_unparsable(
                &path,
                &format!("pricing state: {err}"),
            ),
        }
    }

    let state = LocalPricingState {
        first_seen_at: Utc::now(),
        reconcile_with_server: true,
        mismatch_since: None,
        paywall_first: None,
        last_server_contact_at: None,
        last_account_sync_ok_at: None,
    };
    write_local_state(&state)?;
    Ok(state)
}

fn write_local_state(state: &LocalPricingState) -> Result<(), String> {
    let path = local_state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create pricing config directory {}: {err}",
                parent.display()
            )
        })?;
    }
    // Atomic: a crash mid-write used to truncate the file, silently resetting
    // trial/grace clocks on the next load.
    crate::client_adapters::atomic_write(
        &path,
        &serde_json::to_vec_pretty(state)
            .map_err(|err| format!("Failed to serialize pricing state: {err}"))?,
    )
    // `{err:#}` not `{err}`: atomic_write returns an anyhow chain whose outer
    // context is only "writing <tmp path>". Display alone dropped the io::Error
    // underneath, so Sentry RUST-6R reported a truncated message that could not
    // distinguish ENOSPC from EACCES.
    .map_err(|err| format!("Failed to write pricing state {}: {err:#}", path.display()))
}

/// Minimum spacing between grace/start POSTs from one process.
///
/// grace/start is a heartbeat whose response (first_seen_at) is effectively
/// immutable, but the server throttles it at 10/device/hour. The paywall screen
/// polls get_pricing_status every 3s, which burns that budget in half a minute
/// and leaves the device 429 for the rest of every hour: last_server_contact_at
/// then never advances, the account's last_active_at freezes, and the
/// server-silent alarm fires with hours_silent in the hundreds (RUST-78) on
/// machines whose network is perfectly fine.
///
/// The spacing lives here, not at the callers, because every one of them (UI
/// poll, pricing gate check, watchdog restart, deep link, liveness ping) routes
/// through this function.
const GRACE_START_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Last grace/start attempt, successful or not: a device already inside the 429
/// window has to stop asking, not retry sooner.
static LAST_GRACE_START_ATTEMPT: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// Whether a grace/start attempt is due, given how long ago the last one was.
fn grace_start_due(since_last: Option<std::time::Duration>) -> bool {
    since_last.is_none_or(|elapsed| elapsed >= GRACE_START_MIN_INTERVAL)
}

/// Whether this process may POST grace/start now, stamping the attempt if so.
fn grace_start_attempt_due() -> bool {
    let mut last = LAST_GRACE_START_ATTEMPT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !grace_start_due(last.map(|at| at.elapsed())) {
        return false;
    }
    *last = Some(std::time::Instant::now());
    true
}

fn reconcile_local_state_with_server(state: &AppState) -> Result<LocalPricingState, String> {
    let mut local = load_or_initialize_local_state()?;
    // Nothing in the response changes between calls, so a skipped POST costs
    // the caller nothing: local state is already the answer.
    if !grace_start_attempt_due() {
        return Ok(local);
    }
    let identity = IdentityPayload::for_state(state);
    match fetch_grace_start(&identity) {
        Ok(response) => {
            // Record the fingerprint we just successfully posted so the
            // bearer-pusher worker doesn't immediately repost the same data.
            state.record_pushed_identity_fingerprint(IdentityFingerprint::from_payload(&identity));
            *GRACE_FAILING_SINCE
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            let server_first_seen = response.first_seen_at;
            let new_first_seen = if local.reconcile_with_server {
                server_first_seen.min(local.first_seen_at)
            } else {
                server_first_seen
            };
            // Reachability heartbeat for the server-silent alarm, piggybacked
            // on the write below when first_seen also changed.
            let now = Utc::now();
            let contact_stamp_due = local
                .last_server_contact_at
                .is_none_or(|t| now - t > Duration::hours(CONTACT_STAMP_MIN_INTERVAL_HOURS));
            if contact_stamp_due {
                local.last_server_contact_at = Some(now);
            }
            if new_first_seen != local.first_seen_at
                || local.reconcile_with_server
                || contact_stamp_due
            {
                local.first_seen_at = new_first_seen;
                local.reconcile_with_server = false;
                if let Err(err) = write_local_state(&local) {
                    sentry::capture_message(
                        // {err:#} not {err}: atomic_write puts the rename's os
                        // error in the anyhow chain as a source, but {err} shows
                        // only the top context ("renaming .tmp -> ...") and drops
                        // the errno -- leaving RUST-4W events with no way to tell
                        // a lingering race from environmental ENOSPC/read-only FS.
                        &format!("Could not persist reconciled grace state: {err:#}"),
                        sentry::Level::Warning,
                    );
                }
            }
        }
        Err(err) => {
            // Server unreachable; keep whatever we have locally. reconcile_with_server
            // stays set if this is a fresh install so the next successful call wins.
            maybe_report_server_silent(&local, &identity, &err);
        }
    }
    Ok(local)
}

fn fetch_grace_start(identity: &IdentityPayload) -> Result<GraceResponse, String> {
    let builder = http_client()?.post(api_url("desktop/grace/start"));
    let response = identity
        .apply_headers(builder)
        .json(identity)
        .send()
        .map_err(|err| format!("grace/start request failed: {err}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "grace/start returned {}",
            response.status().as_u16()
        ));
    }

    response
        .json::<GraceResponse>()
        .map_err(|err| format!("grace/start parse failed: {err}"))
}

fn local_state_path() -> PathBuf {
    config_file(&app_data_dir(), "headroom-pricing-state.json")
}

fn read_session_token() -> Result<Option<String>, String> {
    keychain::read_secret(
        HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
        HEADROOM_ACCOUNT_SESSION_ACCOUNT,
    )
    .map(|value| value.and_then(non_empty_string))
}

fn write_session_token(token: &str) -> Result<(), String> {
    keychain::write_secret(
        HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
        HEADROOM_ACCOUNT_SESSION_ACCOUNT,
        token.trim(),
    )
}

fn clear_session_token() -> Result<(), String> {
    keychain::delete_secret(
        HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
        HEADROOM_ACCOUNT_SESSION_ACCOUNT,
    )
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicConfig {
    #[serde(default)]
    active_percent_off: i64,
    #[serde(default)]
    pricing_ladder: Option<PricingLadderPayload>,
    #[serde(default)]
    intro_offer: Option<IntroOffer>,
    #[serde(default)]
    plan_prices: Option<PlanPrices>,
    #[serde(default)]
    paywall_first: bool,
}

fn fetch_public_config() -> Option<PublicConfig> {
    let response = http_client()
        .ok()?
        .get(api_url("desktop/config"))
        // Device id drives server-side bucketing of the paywall-first
        // experiment; the endpoint stays unauthenticated.
        .header("X-Headroom-Device-Id", device::current().machine_id_digest)
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<PublicConfig>().ok()
}

fn fetch_remote_account(
    token: &str,
    identity: &IdentityPayload,
) -> Result<RemoteAccountEnvelope, RemoteAccountSyncError> {
    let builder = http_client()
        .map_err(RemoteAccountSyncError::Other)?
        .get(api_url("desktop/account"))
        .header("Authorization", format!("Bearer {token}"));
    let response = identity
        .apply_headers(builder)
        .send()
        .map_err(|err| RemoteAccountSyncError::Other(format!("send: {err}")))?;

    if response.status().as_u16() == 401 {
        return Err(RemoteAccountSyncError::Unauthorized);
    }

    if !response.status().is_success() {
        return Err(RemoteAccountSyncError::Other(format!(
            "http {}",
            response.status()
        )));
    }

    response
        .json::<RemoteAccountEnvelope>()
        .map_err(|err| RemoteAccountSyncError::Other(format!("decode: {err}")))
}

fn http_client() -> Result<Client, String> {
    // proxy-ok: extraheadroom.com account/pricing API, not loopback
    Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|err| format!("Could not build HTTP client: {err}"))
}

fn api_url(path: &str) -> String {
    join_url(&api_base_url(), path)
}

fn api_base_url() -> String {
    // Runtime override is only honored in debug builds. In release builds an
    // attacker with persistence on the user's machine (e.g. a launchd plist)
    // could otherwise redirect every billing/auth call to a rogue host.
    #[cfg(debug_assertions)]
    let runtime_env = std::env::var("HEADROOM_ACCOUNT_API_BASE_URL").ok();
    #[cfg(not(debug_assertions))]
    let runtime_env: Option<String> = None;

    resolve_account_api_base_url(runtime_env, option_env!("HEADROOM_ACCOUNT_API_BASE_URL"))
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn resolve_account_api_base_url(
    runtime_env: Option<String>,
    compile_time_env: Option<&str>,
) -> String {
    runtime_env
        .and_then(non_empty_string)
        .or_else(|| compile_time_env.and_then(|value| non_empty_string(value.to_string())))
        .unwrap_or_else(|| DEFAULT_ACCOUNT_API_BASE_URL.to_string())
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

const NUDGE_THRESHOLDS_PERCENT: [f64; 3] = [25.0, 35.0, 45.0];
// Max tiers pause at 25% (vs 50% for Pro), so their nudges sit below that
// cutoff to keep the warn-then-pause cadence instead of pausing with no notice.
const MAX_TIER_NUDGE_THRESHOLDS_PERCENT: [f64; 3] = [10.0, 15.0, 20.0];

#[derive(Debug, Clone)]
struct PricingPolicy {
    nudge_thresholds_percent: [f64; 3],
    disable_threshold_percent: f64,
    recommended_tier: HeadroomSubscriptionTier,
}

/// Shared per-tier threshold policy. Both the Claude and Codex gates route
/// through this so their caps can never drift: Pro pauses at 50%, Max at 25%.
/// The Free (ungated / 100%) case is handled by the plan->policy mappers
/// returning `None`.
fn policy_for_paid_tier(tier: HeadroomSubscriptionTier) -> PricingPolicy {
    match tier {
        HeadroomSubscriptionTier::Pro => PricingPolicy {
            nudge_thresholds_percent: NUDGE_THRESHOLDS_PERCENT,
            disable_threshold_percent: 50.0,
            recommended_tier: HeadroomSubscriptionTier::Pro,
        },
        HeadroomSubscriptionTier::Max5x => PricingPolicy {
            nudge_thresholds_percent: MAX_TIER_NUDGE_THRESHOLDS_PERCENT,
            disable_threshold_percent: 25.0,
            recommended_tier: HeadroomSubscriptionTier::Max5x,
        },
        HeadroomSubscriptionTier::Max20x => PricingPolicy {
            nudge_thresholds_percent: MAX_TIER_NUDGE_THRESHOLDS_PERCENT,
            disable_threshold_percent: 25.0,
            recommended_tier: HeadroomSubscriptionTier::Max20x,
        },
    }
}

fn pricing_policy_for_plan(plan: &ClaudePlanTier) -> Option<PricingPolicy> {
    match plan {
        // Free -> ungated (100%). Never metered.
        ClaudePlanTier::Free => None,
        ClaudePlanTier::Pro => Some(policy_for_paid_tier(HeadroomSubscriptionTier::Pro)),
        ClaudePlanTier::Max5x => Some(policy_for_paid_tier(HeadroomSubscriptionTier::Max5x)),
        ClaudePlanTier::Max20x => Some(policy_for_paid_tier(HeadroomSubscriptionTier::Max20x)),
        // API-billed org: metered at the Max ceiling like Unknown - per-token
        // billing carries no weekly quota to key a softer ladder off, and the
        // ceiling stops an API org from riding free. The PITCHED tier comes
        // from the server usage band (account.recommended_tier), not this
        // policy's recommended_tier.
        ClaudePlanTier::Api => Some(policy_for_paid_tier(HeadroomSubscriptionTier::Max20x)),
        // Undecodable plan is metered at the Max ceiling (25%) rather than left
        // ungated, so an obscured plan can't buy unlimited free optimization.
        ClaudePlanTier::Unknown => Some(policy_for_paid_tier(HeadroomSubscriptionTier::Max20x)),
    }
}

/// Codex mirror of `pricing_policy_for_plan`, keyed off the same shared
/// `policy_for_paid_tier`. Free -> ungated (100%); Unknown -> Max ceiling (25%)
/// via `headroom_tier_for_codex_plan` (which already maps Unknown -> Max x20).
fn pricing_policy_for_codex_plan(plan: &CodexPlanTier) -> Option<PricingPolicy> {
    crate::models::headroom_tier_for_codex_plan(plan).map(policy_for_paid_tier)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{
        codex_billing_type, decode_jwt_payload, detect_plan_tier_from_profile,
        detect_tier_mismatch, evaluate_pricing_status_with_mismatch, is_identity_complete,
        merge_background_account_sync, parse_oauth_profile_value, plan_tier_header_value,
        remote_account_to_profile, resolve_account_api_base_url, ClaudeOauthProfile,
        ClaudeOauthProfileAccount, ClaudeOauthProfileOrganization, HeadroomSubscriptionTier,
        IdentityFingerprint, IdentityPayload, LocalPricingState, PricingPromo,
        RemoteAccountResponse, RemoteAccountSyncError, CONSECUTIVE_UNAUTHORIZED_SYNCS,
        DEFAULT_ACCOUNT_API_BASE_URL, MAX_CONSECUTIVE_UNAUTHORIZED_SYNCS,
    };
    use crate::models::{
        BillingPeriod, ClaudeAccountProfile, ClaudeAuthMethod, ClaudePlanTier, CodexPlanTier,
        HeadroomAccountProfile, HeadroomPricingStatus, PricingGateReason, TierMismatch,
        TierRecommendationSource,
    };

    #[test]
    fn public_config_parses_with_and_without_paywall_first() {
        let with: super::PublicConfig =
            serde_json::from_str(r#"{"activePercentOff":10,"paywallFirst":true}"#).unwrap();
        assert!(with.paywall_first);
        // Old server payload: field absent -> false (legacy flow).
        let without: super::PublicConfig =
            serde_json::from_str(r#"{"activePercentOff":10}"#).unwrap();
        assert!(!without.paywall_first);
    }

    #[test]
    fn local_pricing_state_round_trips_old_json_without_paywall_first() {
        // Pre-experiment state file: missing field must parse as None, not
        // wipe the trial/grace clocks.
        let old = r#"{"first_seen_at":"2026-01-01T00:00:00Z"}"#;
        let state: LocalPricingState = serde_json::from_str(old).unwrap();
        assert_eq!(state.paywall_first, None);

        let mut state = state;
        state.paywall_first = Some(true);
        let json = serde_json::to_string(&state).unwrap();
        let back: LocalPricingState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.paywall_first, Some(true));
        assert_eq!(back.first_seen_at, state.first_seen_at);
    }

    #[test]
    fn silence_alarms_pick_the_right_class() {
        use super::{auth_silent_hours, server_silent_hours};
        use chrono::Duration;

        let now = Utc::now();
        let stale = Some(now - Duration::hours(30));
        let fresh = Some(now - Duration::hours(1));
        let mut local = LocalPricingState {
            first_seen_at: now - Duration::days(30),
            reconcile_with_server: false,
            mismatch_since: None,
            paywall_first: None,
            last_server_contact_at: stale,
            last_account_sync_ok_at: stale,
        };

        // Network class: no backend contact for 30h -> server alarm only.
        assert_eq!(server_silent_hours(&local, now), Some(30));
        assert_eq!(auth_silent_hours(&local, now), None);

        // Keychain/401 class: backend reachable, auth stale -> auth alarm only.
        local.last_server_contact_at = fresh;
        assert_eq!(server_silent_hours(&local, now), None);
        assert_eq!(auth_silent_hours(&local, now), Some(30));

        // Healthy: both fresh -> silent.
        local.last_account_sync_ok_at = fresh;
        assert_eq!(server_silent_hours(&local, now), None);
        assert_eq!(auth_silent_hours(&local, now), None);

        // Never-contacted install: baseline falls back to first_seen_at, so a
        // fresh offline install stays quiet but an install-stall alarms.
        local.last_server_contact_at = None;
        local.last_account_sync_ok_at = None;
        local.first_seen_at = now - Duration::hours(2);
        assert_eq!(server_silent_hours(&local, now), None);
        local.first_seen_at = now - Duration::days(3);
        assert_eq!(server_silent_hours(&local, now), Some(72));
        assert_eq!(auth_silent_hours(&local, now), None);
    }

    #[test]
    fn grace_start_attempts_are_spaced_within_a_process() {
        // RUST-78: the paywall screen polls get_pricing_status every 3s, and
        // every call used to POST grace/start -- 1200 an hour against the
        // server's 10/device/hour throttle. The devices that reported it had
        // been 429 for weeks, so last_server_contact_at never advanced and the
        // silent alarm fired at hours_silent=1342 on a healthy network.
        assert!(super::grace_start_due(None), "the first attempt must go");
        assert!(
            !super::grace_start_due(Some(std::time::Duration::from_secs(3))),
            "the 3s paywall poll must not reach the server"
        );
        assert!(super::grace_start_due(Some(
            super::GRACE_START_MIN_INTERVAL
        )));
    }

    #[test]
    fn local_pricing_state_survives_schema_drift_in_either_direction() {
        // A newer build writes a field this build doesn't know: must be
        // ignored, not fail the parse. (Failing would reset first_seen_at and
        // hand the user a fresh 72h grace window on every launch.)
        let newer = r#"{"first_seen_at":"2026-01-01T00:00:00Z","paywall_first":true,"some_future_flag":42}"#;
        let state: LocalPricingState = serde_json::from_str(newer).unwrap();
        assert_eq!(
            state.first_seen_at.to_rfc3339(),
            "2026-01-01T00:00:00+00:00"
        );
        assert_eq!(state.paywall_first, Some(true));

        // And a build that drops/renames first_seen_at must still salvage the
        // remaining fields rather than erroring out to the reset path.
        let dropped = r#"{"paywall_first":false,"reconcile_with_server":true}"#;
        let state: LocalPricingState = serde_json::from_str(dropped).unwrap();
        assert_eq!(state.paywall_first, Some(false));
        assert!(state.reconcile_with_server);
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_pricing_status(
        authenticated: bool,
        local_grace_started_at: DateTime<Utc>,
        local_grace_ends_at: DateTime<Utc>,
        local_grace_active: bool,
        account_sync_error: Option<String>,
        account: Option<HeadroomAccountProfile>,
        claude: ClaudeAccountProfile,
        launch_discount_active: bool,
        last_known_good_plan_tier: Option<ClaudePlanTier>,
    ) -> HeadroomPricingStatus {
        let promo = if launch_discount_active {
            PricingPromo {
                active_percent_off: 50,
                cohorts: vec![],
                intro_offer: None,
                plan_prices: None,
            }
        } else {
            PricingPromo::default()
        };
        evaluate_pricing_status_with_mismatch(
            authenticated,
            local_grace_started_at,
            local_grace_ends_at,
            local_grace_active,
            account_sync_error,
            account,
            claude,
            promo,
            last_known_good_plan_tier,
            None,
        )
    }

    fn mismatch(recommended: HeadroomSubscriptionTier, clamped: bool) -> TierMismatch {
        TierMismatch {
            paid_tier: HeadroomSubscriptionTier::Pro,
            recommended_tier: recommended,
            recommended_source: TierRecommendationSource::Claude,
            grace_ends_at: Utc::now(),
            clamped,
            claude_undercovered: true,
            codex_undercovered: false,
        }
    }

    fn active_subscriber(tier: HeadroomSubscriptionTier) -> HeadroomAccountProfile {
        let mut account = trial_account();
        account.trial_active = false;
        account.subscription_active = true;
        account.subscription_tier = Some(tier);
        account
    }

    fn sample_remote_account() -> RemoteAccountResponse {
        RemoteAccountResponse {
            email: "user@example.com".into(),
            trial_started_at: Some(Utc::now()),
            trial_ends_at: Some(Utc::now()),
            trial_usage_days_left: None,
            trial_active: true,
            subscription_active: true,
            subscription_tier: Some(HeadroomSubscriptionTier::Pro),
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            subscription_cancel_at_period_end: false,
            subscription_ends_at: None,
            subscription_renewal_cents: None,
            subscription_renewal_ends_at: None,
            subscription_pending_tier: None,
            subscription_pending_billing_period: None,
            subscription_pending_effective_at: None,
            invite_code: Some("invite-code".into()),
            accepted_invites_count: 2,
            invite_bonus_percent: 10.0,
            upgrade_action: None,
            recommended_tier: None,
            grandfathered: false,
        }
    }

    #[test]
    fn identity_payload_serializes_with_camelcase_keys_and_skips_nulls() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            claude_account_uuid: Some("claude-uuid".into()),
            claude_plan_tier: Some(ClaudePlanTier::Pro),
            ..Default::default()
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["deviceId"], "abc123");
        assert_eq!(json["claudeAccountUuid"], "claude-uuid");
        assert_eq!(json["claudePlanTier"], "pro");
        assert!(json.get("chopratejasInstanceId").is_none());
        assert!(json.get("claudeEmail").is_none());
    }

    #[test]
    fn identity_payload_carries_the_claude_desktop_verdict() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            claude_desktop: Some(crate::client_adapters::claude_desktop_verdict()),
            ..Default::default()
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert!(
            ["absent", "only", "with_agent"].contains(&json["claudeDesktop"].as_str().unwrap()),
            "unexpected verdict: {}",
            json["claudeDesktop"]
        );
    }

    #[test]
    fn identity_payload_skips_plan_when_none() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert!(json.get("claudePlanTier").is_none());
    }

    #[test]
    fn plan_tier_header_value_covers_all_variants() {
        assert_eq!(plan_tier_header_value(&ClaudePlanTier::Free), "free");
        assert_eq!(plan_tier_header_value(&ClaudePlanTier::Pro), "pro");
        assert_eq!(plan_tier_header_value(&ClaudePlanTier::Max5x), "max5x");
        assert_eq!(plan_tier_header_value(&ClaudePlanTier::Max20x), "max20x");
        assert_eq!(plan_tier_header_value(&ClaudePlanTier::Unknown), "unknown");
    }

    #[test]
    fn apply_headers_sends_raw_claim_for_unknown_codex_plan_only() {
        // Unknown + raw -> the raw value replaces "unknown".
        let unknown = IdentityPayload {
            device_id: "abc123".into(),
            codex_plan_tier: Some(CodexPlanTier::Unknown),
            codex_plan_raw: Some("business_premium".into()),
            ..Default::default()
        };
        let client = reqwest::blocking::Client::new();
        let req = unknown
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("X-Headroom-Codex-Plan").unwrap(),
            "business_premium"
        );

        // A classified tier always wins over a stale raw value.
        let known = IdentityPayload {
            device_id: "abc123".into(),
            codex_plan_tier: Some(CodexPlanTier::Business),
            codex_plan_raw: Some("stale".into()),
            ..Default::default()
        };
        let req = known
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("X-Headroom-Codex-Plan").unwrap(),
            "business"
        );
    }

    #[test]
    fn sanitize_plan_claim_lowercases_strips_and_caps() {
        assert_eq!(
            super::sanitize_plan_claim("  Business_Premium\r\n"),
            Some("business_premium".into())
        );
        assert_eq!(super::sanitize_plan_claim("!!\r\n  "), None);
        assert_eq!(
            super::sanitize_plan_claim(&"x".repeat(100)).map(|s| s.len()),
            Some(64)
        );
    }

    #[test]
    fn apply_headers_sets_plan_when_present() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            claude_plan_tier: Some(ClaudePlanTier::Max20x),
            ..Default::default()
        };
        let client = reqwest::blocking::Client::new();
        let req = identity
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("X-Headroom-Claude-Plan").unwrap(),
            "max20x"
        );
    }

    #[test]
    fn apply_headers_sets_app_version() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            ..Default::default()
        };
        let client = reqwest::blocking::Client::new();
        let req = identity
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("X-Headroom-App-Version").unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            req.headers().get("X-Headroom-Os").unwrap(),
            std::env::consts::OS
        );
    }

    #[test]
    fn apply_headers_omits_plan_when_none() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            ..Default::default()
        };
        let client = reqwest::blocking::Client::new();
        let req = identity
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        assert!(req.headers().get("X-Headroom-Claude-Plan").is_none());
    }

    #[test]
    fn apply_headers_sets_codex_fields_when_present() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            codex_account_uuid: Some("acct_1".into()),
            codex_email: Some("dev@example.com".into()),
            codex_plan_tier: Some(CodexPlanTier::Business),
            codex_organization_type: Some("owner".into()),
            codex_billing_type: Some("business".into()),
            ..Default::default()
        };
        let client = reqwest::blocking::Client::new();
        let req = identity
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(h.get("X-Headroom-Codex-Plan").unwrap(), "business");
        assert_eq!(h.get("X-Headroom-Codex-Uuid").unwrap(), "acct_1");
        assert_eq!(h.get("X-Headroom-Codex-Email").unwrap(), "dev@example.com");
        assert_eq!(
            h.get("X-Headroom-Codex-Organization-Type").unwrap(),
            "owner"
        );
        assert_eq!(h.get("X-Headroom-Codex-Billing-Type").unwrap(), "business");
        // rate_limit_tier has no source today, so the header is omitted.
        assert!(h.get("X-Headroom-Codex-Rate-Limit-Tier").is_none());
    }

    #[test]
    fn apply_headers_omits_codex_when_none() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            ..Default::default()
        };
        let client = reqwest::blocking::Client::new();
        let req = identity
            .apply_headers(client.get("http://example.test"))
            .build()
            .unwrap();
        assert!(req.headers().get("X-Headroom-Codex-Plan").is_none());
        assert!(req.headers().get("X-Headroom-Codex-Uuid").is_none());
    }

    #[test]
    fn codex_plan_header_value_covers_all_variants() {
        assert_eq!(CodexPlanTier::Free.as_header_str(), "free");
        assert_eq!(CodexPlanTier::Plus.as_header_str(), "plus");
        assert_eq!(CodexPlanTier::Pro.as_header_str(), "pro");
        assert_eq!(CodexPlanTier::Team.as_header_str(), "team");
        assert_eq!(CodexPlanTier::Business.as_header_str(), "business");
        assert_eq!(CodexPlanTier::Enterprise.as_header_str(), "enterprise");
        assert_eq!(CodexPlanTier::Edu.as_header_str(), "edu");
        assert_eq!(CodexPlanTier::Education.as_header_str(), "education");
        assert_eq!(
            CodexPlanTier::SelfServeBusinessProlite.as_header_str(),
            "self_serve_business_prolite"
        );
        assert_eq!(CodexPlanTier::Unknown.as_header_str(), "unknown");
    }

    #[test]
    fn codex_billing_type_derivation() {
        // Free/unknown carry nothing.
        assert_eq!(codex_billing_type(&CodexPlanTier::Free, false), None);
        assert_eq!(codex_billing_type(&CodexPlanTier::Unknown, true), None);
        // Paid personal plans report "subscription" when no org.
        assert_eq!(
            codex_billing_type(&CodexPlanTier::Plus, false),
            Some("subscription".into())
        );
        assert_eq!(
            codex_billing_type(&CodexPlanTier::Pro, false),
            Some("subscription".into())
        );
        // Org seats report the plan string.
        assert_eq!(
            codex_billing_type(&CodexPlanTier::Plus, true),
            Some("plus".into())
        );
        assert_eq!(
            codex_billing_type(&CodexPlanTier::Business, false),
            Some("business".into())
        );
        assert_eq!(
            codex_billing_type(&CodexPlanTier::Enterprise, true),
            Some("enterprise".into())
        );
    }

    #[test]
    fn decode_jwt_payload_extracts_codex_claims() {
        use base64::Engine;
        let payload_json = r#"{"email":"dev@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"acct_9","chatgpt_plan_type":"business","organizations":[{"role":"owner","is_default":true}]}}"#;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let token = format!("header.{b64}.sig");

        let payload = decode_jwt_payload(&token).expect("decodes");
        assert_eq!(payload["email"], "dev@example.com");
        let auth = &payload["https://api.openai.com/auth"];
        assert_eq!(auth["chatgpt_plan_type"], "business");
        assert_eq!(
            CodexPlanTier::from_claim(auth["chatgpt_plan_type"].as_str().unwrap()),
            CodexPlanTier::Business
        );
        assert_eq!(auth["organizations"][0]["role"], "owner");
    }

    fn complete_profile() -> ClaudeAccountProfile {
        ClaudeAccountProfile {
            auth_method: ClaudeAuthMethod::ClaudeAiOauth,
            email: Some("user@example.com".into()),
            display_name: Some("User".into()),
            account_uuid: Some("uuid-1".into()),
            organization_uuid: Some("org-1".into()),
            billing_type: Some("personal".into()),
            account_created_at: None,
            subscription_created_at: None,
            has_extra_usage_enabled: false,
            plan_tier: ClaudePlanTier::Pro,
            plan_detection_source: Some("oauth_profile.org.rate_limit_tier".into()),
            organization_type: Some("claude_pro".into()),
            rate_limit_tier: Some("default_claude_ai".into()),
            user_rate_limit_tier: None,
            seat_tier: None,
            weekly_utilization_pct: None,
            weekly_resets_at: None,
            five_hour_utilization_pct: None,
            extra_usage_monthly_limit: None,
            profile_fetch_error: None,
        }
    }

    #[test]
    fn is_identity_complete_requires_uuid_email_and_known_plan() {
        let mut profile = complete_profile();
        assert!(is_identity_complete(&profile));

        profile.account_uuid = None;
        assert!(!is_identity_complete(&profile));

        profile = complete_profile();
        profile.email = None;
        assert!(!is_identity_complete(&profile));

        profile = complete_profile();
        profile.plan_tier = ClaudePlanTier::Unknown;
        assert!(!is_identity_complete(&profile));

        profile = complete_profile();
        profile.plan_tier = ClaudePlanTier::Free;
        assert!(is_identity_complete(&profile));
    }

    #[test]
    fn identity_fingerprint_round_trips_payload_claude_fields() {
        let payload = IdentityPayload {
            device_id: "device-abc".into(),
            chopratejas_instance_id: Some("ignored".into()),
            claude_account_uuid: Some("uuid-1".into()),
            claude_email: Some("user@example.com".into()),
            claude_plan_tier: Some(ClaudePlanTier::Max20x),
            claude_organization_type: Some("claude_max".into()),
            claude_rate_limit_tier: Some("default_claude_max_x20".into()),
            claude_billing_type: Some("personal".into()),
            codex_account_uuid: Some("acct_1".into()),
            codex_plan_tier: Some(CodexPlanTier::Business),
            accepted_terms_version: Some(1),
            ..Default::default()
        };
        let fp = IdentityFingerprint::from_payload(&payload);

        // Same payload produces equal fingerprint.
        assert_eq!(fp, IdentityFingerprint::from_payload(&payload));

        // Mutating any Claude field changes the fingerprint.
        let mut other = payload.clone();
        other.claude_plan_tier = Some(ClaudePlanTier::Pro);
        assert_ne!(fp, IdentityFingerprint::from_payload(&other));

        // Mutating a fingerprinted Codex field also changes the fingerprint.
        let mut codex_change = payload.clone();
        codex_change.codex_plan_tier = Some(CodexPlanTier::Plus);
        assert_ne!(fp, IdentityFingerprint::from_payload(&codex_change));

        // device_id / chopratejas_instance_id are not part of the fingerprint.
        let mut device_only_diff = payload.clone();
        device_only_diff.device_id = "different-device".into();
        device_only_diff.chopratejas_instance_id = Some("different".into());
        assert_eq!(fp, IdentityFingerprint::from_payload(&device_only_diff));
    }

    #[test]
    fn identity_fingerprint_is_empty_when_no_claude_signal_captured() {
        // Bearer-not-yet-captured shape: no UUID, plan_tier defaulted to Unknown.
        let empty_unknown = IdentityFingerprint::from_payload(&IdentityPayload {
            device_id: "abc".into(),
            claude_plan_tier: Some(ClaudePlanTier::Unknown),
            ..Default::default()
        });
        assert!(empty_unknown.is_empty());

        // Device-only payload: no plan tier at all.
        let device_only = IdentityFingerprint::from_payload(&IdentityPayload {
            device_id: "abc".into(),
            ..Default::default()
        });
        assert!(device_only.is_empty());

        // Anything with a real plan tier OR a UUID is NOT empty.
        let with_plan = IdentityFingerprint::from_payload(&IdentityPayload {
            device_id: "abc".into(),
            claude_plan_tier: Some(ClaudePlanTier::Pro),
            ..Default::default()
        });
        assert!(!with_plan.is_empty());

        let with_uuid = IdentityFingerprint::from_payload(&IdentityPayload {
            device_id: "abc".into(),
            claude_account_uuid: Some("uuid".into()),
            claude_plan_tier: Some(ClaudePlanTier::Unknown),
            ..Default::default()
        });
        assert!(!with_uuid.is_empty());
    }

    #[test]
    fn local_pricing_state_back_compat_parses_old_payload_without_reconcile_flag() {
        let raw = r#"{"first_seen_at":"2026-04-10T00:00:00Z"}"#;
        let state: LocalPricingState = serde_json::from_str(raw).unwrap();
        assert!(!state.reconcile_with_server);
    }

    #[test]
    fn runtime_env_overrides_compile_time_env() {
        let resolved = resolve_account_api_base_url(
            Some("https://runtime.example/api/v1".into()),
            Some("https://compile.example/api/v1"),
        );

        assert_eq!(resolved, "https://runtime.example/api/v1");
    }

    #[test]
    fn compile_time_env_used_when_runtime_missing() {
        let resolved = resolve_account_api_base_url(None, Some("https://compile.example/api/v1"));

        assert_eq!(resolved, "https://compile.example/api/v1");
    }

    #[test]
    fn blank_values_fall_back_to_default() {
        let resolved = resolve_account_api_base_url(Some("   ".into()), Some(" "));

        assert_eq!(resolved, DEFAULT_ACCOUNT_API_BASE_URL);
    }

    #[test]
    #[serial_test::serial]
    fn unauthorized_background_sync_tolerates_blips_then_escalates() {
        CONSECUTIVE_UNAUTHORIZED_SYNCS.store(0, std::sync::atomic::Ordering::Relaxed);

        // Transient 401s keep the local session authenticated...
        for _ in 0..(MAX_CONSECUTIVE_UNAUTHORIZED_SYNCS - 1) {
            let (authenticated, account, error) = merge_background_account_sync(
                Some("session-token"),
                Err(RemoteAccountSyncError::Unauthorized),
            );
            assert!(authenticated);
            assert!(account.is_none());
            assert!(error.is_some());
        }

        // ...but a revoked session (consecutive 401s) escalates to signed-out.
        let (authenticated, _, error) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Unauthorized),
        );
        assert!(!authenticated);
        assert!(error.is_some());

        // A single success resets the tolerance window.
        let (authenticated, _, _) =
            merge_background_account_sync(Some("session-token"), Ok(sample_remote_account()));
        assert!(authenticated);
        let (authenticated, _, _) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Unauthorized),
        );
        assert!(authenticated);
    }

    #[test]
    fn transient_background_sync_error_keeps_local_session_authenticated() {
        let (authenticated, account, error) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Other("send: timed out".into())),
        );

        assert!(authenticated);
        assert!(account.is_none());
        assert!(error.is_some());
    }

    #[test]
    // Ok resets the unauthorized counter, so keep it off the escalation
    // test's timeline.
    #[serial_test::serial]
    fn successful_background_sync_returns_remote_account_profile() {
        let (authenticated, account, error) =
            merge_background_account_sync(Some("session-token"), Ok(sample_remote_account()));

        assert!(authenticated);
        assert!(error.is_none());
        assert_eq!(
            account.as_ref().map(|value| value.email.as_str()),
            Some("user@example.com")
        );
        assert!(matches!(
            account
                .as_ref()
                .and_then(|value| value.subscription_tier.clone()),
            Some(HeadroomSubscriptionTier::Pro)
        ));
    }

    #[test]
    fn release_default_points_at_production_api() {
        #[cfg(not(debug_assertions))]
        assert_eq!(
            DEFAULT_ACCOUNT_API_BASE_URL,
            "https://extraheadroom.com/api/v1"
        );
    }

    fn empty_claude_profile(plan_tier: ClaudePlanTier) -> ClaudeAccountProfile {
        ClaudeAccountProfile {
            auth_method: ClaudeAuthMethod::ClaudeAiOauth,
            email: None,
            display_name: None,
            account_uuid: None,
            organization_uuid: None,
            billing_type: None,
            account_created_at: None,
            subscription_created_at: None,
            has_extra_usage_enabled: false,
            plan_tier,
            plan_detection_source: None,
            organization_type: None,
            rate_limit_tier: None,
            user_rate_limit_tier: None,
            seat_tier: None,
            weekly_utilization_pct: None,
            weekly_resets_at: None,
            five_hour_utilization_pct: None,
            extra_usage_monthly_limit: None,
            profile_fetch_error: None,
        }
    }

    fn pro_profile_with_weekly(weekly: f64) -> ClaudeAccountProfile {
        let mut p = empty_claude_profile(ClaudePlanTier::Pro);
        p.weekly_utilization_pct = Some(weekly);
        p
    }

    fn unknown_profile_with_weekly(weekly: f64) -> ClaudeAccountProfile {
        let mut p = empty_claude_profile(ClaudePlanTier::Unknown);
        p.weekly_utilization_pct = Some(weekly);
        p
    }

    fn max5x_profile_with_weekly(weekly: f64) -> ClaudeAccountProfile {
        let mut p = empty_claude_profile(ClaudePlanTier::Max5x);
        p.weekly_utilization_pct = Some(weekly);
        p
    }

    fn trial_account() -> HeadroomAccountProfile {
        HeadroomAccountProfile {
            email: "user@example.com".into(),
            trial_started_at: Some(Utc::now()),
            trial_ends_at: Some(Utc::now()),
            trial_usage_days_left: None,
            trial_active: true,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            subscription_cancel_at_period_end: false,
            subscription_ends_at: None,
            subscription_renewal_cents: None,
            subscription_renewal_ends_at: None,
            subscription_pending_tier: None,
            subscription_pending_billing_period: None,
            subscription_pending_effective_at: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: 0.0,
            upgrade_action: None,
            recommended_tier: None,
            grandfathered: false,
        }
    }

    fn expired_account(invite_bonus: f64) -> HeadroomAccountProfile {
        HeadroomAccountProfile {
            email: "user@example.com".into(),
            trial_started_at: None,
            trial_ends_at: None,
            trial_usage_days_left: None,
            trial_active: false,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            subscription_cancel_at_period_end: false,
            subscription_ends_at: None,
            subscription_renewal_cents: None,
            subscription_renewal_ends_at: None,
            subscription_pending_tier: None,
            subscription_pending_billing_period: None,
            subscription_pending_effective_at: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: invite_bonus,
            upgrade_action: None,
            recommended_tier: None,
            grandfathered: false,
        }
    }

    /// Grandfathered early adopter: like `expired_account` (no sub, no trial) but
    /// keeps the capped free tier instead of the hard block.
    fn grandfathered_account() -> HeadroomAccountProfile {
        HeadroomAccountProfile {
            upgrade_action: None,
            recommended_tier: None,
            grandfathered: true,
            ..expired_account(0.0)
        }
    }

    fn grace() -> (DateTime<Utc>, DateTime<Utc>) {
        let now = Utc::now();
        (now, now + chrono::Duration::hours(72))
    }

    #[test]
    fn trial_active_allows_optimization_without_weekly_gating() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            true,
            None,
            Some(trial_account()),
            pro_profile_with_weekly(95.0),
            false,
            None,
        );
        assert!(status.optimization_allowed);
        assert!(!status.should_nudge);
        assert!(status.gate_reason.is_none());
    }

    #[test]
    fn active_subscription_allows_optimization_even_over_limit() {
        let (start, end) = grace();
        let mut account = trial_account();
        account.trial_active = false;
        account.subscription_active = true;
        account.subscription_tier = Some(HeadroomSubscriptionTier::Pro);
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            true,
            None,
            Some(account),
            pro_profile_with_weekly(99.0),
            false,
            None,
        );
        assert!(status.optimization_allowed);
        assert!(status.gate_reason.is_none());
    }

    #[test]
    fn paid_plan_gate_pro_ladder() {
        // Pro: nudges at 25/35/45, pause at 50.
        let blocked = super::paid_plan_gate(&ClaudePlanTier::Pro, Some(50.0), 0.0);
        assert!(!blocked.optimization_allowed);
        assert!(matches!(
            blocked.gate_reason,
            Some(PricingGateReason::WeeklyUsageLimitReached)
        ));

        let l1 = super::paid_plan_gate(&ClaudePlanTier::Pro, Some(25.0), 0.0);
        assert!(l1.optimization_allowed);
        assert!(l1.should_nudge);
        assert_eq!(l1.nudge_level, 1);

        let l3 = super::paid_plan_gate(&ClaudePlanTier::Pro, Some(45.0), 0.0);
        assert_eq!(l3.nudge_level, 3);

        let silent = super::paid_plan_gate(&ClaudePlanTier::Pro, Some(20.0), 0.0);
        assert!(silent.optimization_allowed);
        assert!(!silent.should_nudge);

        let no_signal = super::paid_plan_gate(&ClaudePlanTier::Pro, None, 0.0);
        assert!(no_signal.optimization_allowed);
        assert!(!no_signal.should_nudge);
    }

    #[test]
    fn paid_plan_gate_max_tier_pauses_at_25() {
        // Max tiers: nudges at 10/15/20, pause at 25.
        let blocked = super::paid_plan_gate(&ClaudePlanTier::Max5x, Some(25.0), 0.0);
        assert!(!blocked.optimization_allowed);
        assert_eq!(
            blocked.recommended_subscription_tier,
            Some(HeadroomSubscriptionTier::Max5x)
        );

        let l3 = super::paid_plan_gate(&ClaudePlanTier::Max20x, Some(20.0), 0.0);
        assert!(l3.optimization_allowed);
        assert_eq!(l3.nudge_level, 3);
    }

    #[test]
    fn paid_plan_gate_invite_bonus_shifts_and_caps() {
        // Pro + 10pt bonus: disable shifts to 60, so 55% still optimizes.
        let bonus = super::paid_plan_gate(&ClaudePlanTier::Pro, Some(55.0), 10.0);
        assert!(bonus.optimization_allowed);
        assert_eq!(bonus.effective_disable_threshold_percent, Some(60.0));

        // Bonus is capped at +50 points.
        let capped = super::paid_plan_gate(&ClaudePlanTier::Pro, None, 80.0);
        assert_eq!(capped.effective_disable_threshold_percent, Some(100.0));
    }

    #[test]
    fn paid_plan_gate_free_is_never_gated() {
        // Free -> ungated (100%), even past any threshold.
        let gate = super::paid_plan_gate(&ClaudePlanTier::Free, Some(99.0), 0.0);
        assert!(gate.optimization_allowed);
        assert!(!gate.should_nudge);
        assert!(gate.gate_reason.is_none());
    }

    #[test]
    fn paid_plan_gate_unknown_meters_at_max_ceiling() {
        // Undecodable plan is metered at the Max ceiling (25%) so an obscured
        // plan can't buy unlimited free optimization.
        let below = super::paid_plan_gate(&ClaudePlanTier::Unknown, Some(20.0), 0.0);
        assert!(below.optimization_allowed, "Unknown at 20% is below 25%");
        let paused = super::paid_plan_gate(&ClaudePlanTier::Unknown, Some(30.0), 0.0);
        assert!(!paused.optimization_allowed, "Unknown at 30% >= 25%");
        assert_eq!(paused.effective_disable_threshold_percent, Some(25.0));
    }

    #[test]
    fn format_nudge_message_renders_product_numbers_and_level_copy() {
        let l1 = super::format_nudge_message("Claude", 34.2, 50.0, 1);
        assert!(l1.contains("34.2% of weekly Claude usage"), "{l1}");
        assert!(
            l1.contains("through 50.0%"),
            "level 1 keeps-through copy: {l1}"
        );

        let l2 = super::format_nudge_message("Codex", 44.0, 50.0, 2);
        assert!(l2.contains("44.0% of weekly Codex usage"), "{l2}");
        assert!(l2.contains("pauses at 50.0%"), "level 2 pause copy: {l2}");

        // Any level > 2 falls through to the final "will pause" variant.
        let l3 = super::format_nudge_message("Claude", 48.5, 50.0, 3);
        assert!(l3.contains("48.5% of weekly Claude usage"), "{l3}");
        assert!(
            l3.contains("will pause at 50.0%"),
            "level 3 pause copy: {l3}"
        );
    }

    #[test]
    fn post_trial_hard_blocks_every_tier_with_no_free_plan() {
        // Trial ended, no subscription: optimization stops for every Claude
        // tier. There is no usable free plan post-trial.
        for tier in [
            ClaudePlanTier::Free,
            ClaudePlanTier::Pro,
            ClaudePlanTier::Max5x,
            ClaudePlanTier::Max20x,
            ClaudePlanTier::Unknown,
        ] {
            let (start, end) = grace();
            let status = evaluate_pricing_status(
                true,
                start,
                end,
                false,
                None,
                Some(expired_account(0.0)),
                empty_claude_profile(tier.clone()),
                false,
                None,
            );
            assert!(
                !status.optimization_allowed,
                "{tier:?}: post-trial must block optimization"
            );
            assert!(status.should_nudge);
            assert!(matches!(
                status.gate_reason,
                Some(PricingGateReason::TrialEnded)
            ));
        }
    }

    #[test]
    fn grandfathered_claude_gets_capped_free_tier() {
        let (start, end) = grace();
        // Pro plan: metered at 50% (not hard-blocked like a plain expired trial).
        let ok = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(grandfathered_account()),
            pro_profile_with_weekly(40.0),
            false,
            None,
        );
        assert!(ok.optimization_allowed, "grandfathered Pro at 40% stays on");
        assert!(ok.gate_reason.is_none());

        let paused = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(grandfathered_account()),
            pro_profile_with_weekly(60.0),
            false,
            None,
        );
        assert!(
            !paused.optimization_allowed,
            "grandfathered Pro at 60% pauses at 50%"
        );
        assert!(matches!(
            paused.gate_reason,
            Some(PricingGateReason::WeeklyUsageLimitReached)
        ));

        // Free plan: ungated (100%) even past every threshold.
        let mut free_profile = empty_claude_profile(ClaudePlanTier::Free);
        free_profile.weekly_utilization_pct = Some(95.0);
        let free = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(grandfathered_account()),
            free_profile,
            false,
            None,
        );
        assert!(
            free.optimization_allowed,
            "grandfathered Free is never paused"
        );
    }

    #[test]
    fn trial_ended_hard_block_reports_no_weekly_limit_nudge() {
        // The hard block sets should_nudge purely to surface the upgrade prompt;
        // reporting it would email "close to your weekly limit on the free plan"
        // to someone whose trial simply expired. Codex hard-block likewise.
        let (start, end) = grace();
        let mut status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            pro_profile_with_weekly(30.0),
            false,
            None,
        );
        assert!(status.should_nudge);
        assert!(super::weekly_limit_signal(&status).is_none());

        status.codex = Some(super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(80.0),
            crate::models::CodexPlanTier::Pro,
            super::CodexActivation::HardBlock,
            0.0,
        ));
        assert!(super::weekly_limit_signal(&status).is_none());
    }

    #[test]
    fn grandfathered_weekly_cap_still_reports_nudges() {
        let (start, end) = grace();
        let paused = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(grandfathered_account()),
            pro_profile_with_weekly(60.0),
            false,
            None,
        );
        let reached = super::weekly_limit_signal(&paused).expect("paused free tier reports");
        assert_eq!(reached.status, "reached");
        assert_eq!(reached.cap_percent, Some(50.0));

        let nudging = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(grandfathered_account()),
            pro_profile_with_weekly(30.0),
            false,
            None,
        );
        let approaching = super::weekly_limit_signal(&nudging).expect("nudging free tier reports");
        assert_eq!(approaching.status, "approaching");
        assert_eq!(approaching.cap_percent, Some(50.0));
    }

    #[test]
    fn authenticated_without_account_keeps_optimization_on() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            Some("transient".into()),
            None,
            empty_claude_profile(ClaudePlanTier::Pro),
            false,
            None,
        );
        assert!(status.optimization_allowed);
        assert!(!status.needs_authentication);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn unauthenticated_without_grace_requires_sign_in() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            false,
            start,
            end,
            false,
            None,
            None,
            empty_claude_profile(ClaudePlanTier::Pro),
            false,
            None,
        );
        assert!(status.needs_authentication);
        assert!(!status.optimization_allowed);
        assert!(matches!(
            status.gate_reason,
            Some(PricingGateReason::SignInRequired)
        ));
    }

    fn oauth_profile(
        rate_limit_tier: Option<&str>,
        organization_type: Option<&str>,
        subscription_created_at: Option<DateTime<Utc>>,
    ) -> ClaudeOauthProfile {
        ClaudeOauthProfile {
            account: ClaudeOauthProfileAccount {
                uuid: None,
                email: None,
                display_name: None,
                created_at: None,
            },
            organization: Some(ClaudeOauthProfileOrganization {
                uuid: None,
                billing_type: None,
                subscription_created_at,
                has_extra_usage_enabled: false,
                organization_type: organization_type.map(str::to_string),
                rate_limit_tier: rate_limit_tier.map(str::to_string),
                user_rate_limit_tier: None,
                seat_tier: None,
            }),
        }
    }

    #[test]
    fn parse_oauth_profile_captures_seat_fields() {
        let body = serde_json::json!({
            "account": { "uuid": "u-1", "email": "felix@example.com" },
            "organization": {
                "organization_type": "claude_team",
                "rate_limit_tier": "raven",
                "user_rate_limit_tier": "raven_standard_seat",
                "seat_tier": "standard",
            }
        });
        let profile = parse_oauth_profile_value(&body).expect("profile parses");
        let org = profile.organization.expect("organization present");
        assert_eq!(
            org.user_rate_limit_tier.as_deref(),
            Some("raven_standard_seat")
        );
        assert_eq!(org.seat_tier.as_deref(), Some("standard"));
    }

    #[test]
    fn parse_oauth_profile_seat_fields_absent_stay_none() {
        let body = serde_json::json!({
            "account": { "uuid": "u-1" },
            "organization": { "organization_type": "claude_pro" }
        });
        let profile = parse_oauth_profile_value(&body).expect("profile parses");
        let org = profile.organization.expect("organization present");
        assert!(org.user_rate_limit_tier.is_none());
        assert!(org.seat_tier.is_none());
    }

    #[test]
    fn detect_plan_tier_rate_limit_20x_wins() {
        let p = oauth_profile(Some("claude_max_20x"), Some("claude_pro"), Some(Utc::now()));
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max20x
        ));
    }

    #[test]
    fn detect_plan_tier_rate_limit_5x_wins() {
        let p = oauth_profile(Some("claude_max_5x"), Some("claude_pro"), Some(Utc::now()));
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max5x
        ));
    }

    #[test]
    fn detect_plan_tier_rate_limit_x5_variant_is_max5x() {
        let p = oauth_profile(
            Some("default_claude_max_x5"),
            Some("default_claude"),
            Some(Utc::now()),
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max5x
        ));
    }

    #[test]
    fn detect_plan_tier_rate_limit_x20_variant_is_max20x() {
        let p = oauth_profile(
            Some("default_claude_max_x20"),
            Some("default_claude"),
            Some(Utc::now()),
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max20x
        ));
    }

    fn with_seat_tier(mut p: ClaudeOauthProfile, seat: &str) -> ClaudeOauthProfile {
        p.organization.as_mut().unwrap().seat_tier = Some(seat.into());
        p
    }

    #[test]
    fn detect_plan_tier_default_raven_is_max20x() {
        let p = oauth_profile(Some("default_raven"), Some("claude_team"), Some(Utc::now()));
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max20x
        ));
    }

    #[test]
    fn detect_plan_tier_team_standard_seat_is_pro() {
        let p = with_seat_tier(
            oauth_profile(Some("raven"), Some("claude_team"), Some(Utc::now())),
            "team_standard",
        );
        let (tier, source) = detect_plan_tier_from_profile(&p);
        assert!(matches!(tier, ClaudePlanTier::Pro));
        assert_eq!(
            source.as_deref(),
            Some("oauth_profile.organization.seatTier")
        );
    }

    #[test]
    fn detect_plan_tier_team_tier_1_seat_is_max5x() {
        let p = with_seat_tier(
            oauth_profile(Some("raven"), Some("claude_team"), Some(Utc::now())),
            "team_tier_1",
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max5x
        ));
    }

    #[test]
    fn detect_plan_tier_team_tier_2_seat_is_max20x() {
        let p = with_seat_tier(
            oauth_profile(Some("raven"), Some("claude_team"), Some(Utc::now())),
            "team_tier_2",
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max20x
        ));
    }

    #[test]
    fn detect_plan_tier_explicit_multiplier_outranks_seat_tier() {
        let p = with_seat_tier(
            oauth_profile(
                Some("default_claude_max_5x"),
                Some("claude_team"),
                Some(Utc::now()),
            ),
            "team_standard",
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max5x
        ));
    }

    #[test]
    fn detect_plan_tier_unknown_seat_tier_keeps_raven_max20x() {
        let p = with_seat_tier(
            oauth_profile(Some("raven"), Some("claude_team"), Some(Utc::now())),
            "team_tier_9",
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max20x
        ));
    }

    #[test]
    fn detect_plan_tier_seat_tier_without_rate_limit_tier_classifies() {
        let p = with_seat_tier(
            oauth_profile(None, Some("claude_team"), Some(Utc::now())),
            "team_standard",
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Pro
        ));
    }

    #[test]
    fn detect_plan_tier_raven_substring_is_max20x() {
        let p = oauth_profile(
            Some("default_raven_x"),
            Some("claude_team"),
            Some(Utc::now()),
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max20x
        ));
    }

    #[test]
    fn detect_plan_tier_default_rate_limit_with_claude_max_is_max5x() {
        let p = oauth_profile(
            Some("default_claude_ai"),
            Some("claude_max"),
            Some(Utc::now()),
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max5x
        ));
    }

    #[test]
    fn detect_plan_tier_default_rate_limit_with_claude_pro_is_pro() {
        let p = oauth_profile(
            Some("default_claude_ai"),
            Some("claude_pro"),
            Some(Utc::now()),
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Pro
        ));
    }

    #[test]
    fn detect_plan_tier_organization_type_claude_free_is_free() {
        let p = oauth_profile(None, Some("claude_free"), Some(Utc::now()));
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Free
        ));
    }

    #[test]
    fn detect_plan_tier_missing_organization_is_free() {
        let p = ClaudeOauthProfile {
            account: ClaudeOauthProfileAccount {
                uuid: None,
                email: None,
                display_name: None,
                created_at: None,
            },
            organization: None,
        };
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Free
        ));
    }

    #[test]
    fn detect_plan_tier_no_subscription_created_at_is_free() {
        let p = oauth_profile(None, None, None);
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Free
        ));
    }

    #[test]
    fn detect_plan_tier_with_subscription_but_no_identifying_fields_is_unknown() {
        let p = oauth_profile(None, None, Some(Utc::now()));
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Unknown
        ));
    }

    /// RUST-B2 shape: prepaid API console org with subscription_created_at
    /// set. Previously fell through to Unknown and fired plan_tier_unknown.
    #[test]
    fn detect_plan_tier_api_org_with_subscription_is_api() {
        let p = oauth_profile(
            Some("auto_trust_tier_c"),
            Some("api_individual"),
            Some(Utc::now()),
        );
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Api
        ));
    }

    /// The other observed shape (user 1208): auto_api_evaluation without
    /// subscription_created_at. Previously misread as Free.
    #[test]
    fn detect_plan_tier_api_org_without_subscription_is_api_not_free() {
        let p = oauth_profile(Some("auto_api_evaluation"), Some("api_individual"), None);
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Api
        ));
    }

    #[test]
    fn api_plan_tier_is_ceiling_metered_and_server_pitched() {
        // Gate: Max ceiling like Unknown.
        let policy = super::pricing_policy_for_plan(&ClaudePlanTier::Api).unwrap();
        assert_eq!(policy.disable_threshold_percent, 25.0);
        // No local pitch: the server usage band owns the recommendation.
        assert!(crate::models::headroom_tier_for_claude_plan(&ClaudePlanTier::Api).is_none());
        assert_eq!(super::plan_tier_header_value(&ClaudePlanTier::Api), "api");
    }

    #[test]
    fn remote_account_clamps_invite_bonus_to_50() {
        let raw = RemoteAccountResponse {
            email: "a@b".into(),
            trial_started_at: None,
            trial_ends_at: None,
            trial_usage_days_left: None,
            trial_active: false,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            subscription_cancel_at_period_end: false,
            subscription_ends_at: None,
            subscription_renewal_cents: None,
            subscription_renewal_ends_at: None,
            subscription_pending_tier: None,
            subscription_pending_billing_period: None,
            subscription_pending_effective_at: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: 999.0,
            upgrade_action: None,
            recommended_tier: None,
            grandfathered: false,
        };
        assert_eq!(remote_account_to_profile(raw).invite_bonus_percent, 50.0);
    }

    #[test]
    fn remote_account_clamps_negative_invite_bonus_to_zero() {
        let raw = RemoteAccountResponse {
            email: "a@b".into(),
            trial_started_at: None,
            trial_ends_at: None,
            trial_usage_days_left: None,
            trial_active: false,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            subscription_cancel_at_period_end: false,
            subscription_ends_at: None,
            subscription_renewal_cents: None,
            subscription_renewal_ends_at: None,
            subscription_pending_tier: None,
            subscription_pending_billing_period: None,
            subscription_pending_effective_at: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: -10.0,
            upgrade_action: None,
            recommended_tier: None,
            grandfathered: false,
        };
        assert_eq!(remote_account_to_profile(raw).invite_bonus_percent, 0.0);
    }

    // ── Anthropic OAuth usage parser ────────────────────────────────────────

    #[test]
    fn parse_claude_usage_response_decodes_full_payload() {
        let body = serde_json::json!({
            "five_hour": {
                "utilization": 42.5,
                "resets_at": "2026-04-25T15:00:00Z"
            },
            "seven_day": {
                "utilization": 18.75,
                "resets_at": "2026-04-30T00:00:00Z"
            },
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 50.0,
                "used_credits": 12.5,
                "utilization": 25.0
            }
        });
        let usage = super::parse_claude_usage_response(&body).expect("parse usage");

        let five = usage.five_hour.expect("five-hour window");
        assert!((five.utilization - 42.5).abs() < f64::EPSILON);

        let seven = usage.seven_day.expect("seven-day window");
        assert!((seven.utilization - 18.75).abs() < f64::EPSILON);

        let extra = usage.extra_usage.expect("extra-usage block");
        assert!(extra.is_enabled);
        assert_eq!(extra.monthly_limit, Some(50.0));
        assert_eq!(extra.used_credits, Some(12.5));
        assert_eq!(extra.utilization, Some(25.0));
    }

    #[test]
    fn parse_claude_usage_response_returns_error_on_api_error_envelope() {
        let body = serde_json::json!({
            "error": { "message": "rate limit exceeded" }
        });
        let err = super::parse_claude_usage_response(&body).expect_err("api error");
        assert!(
            err.contains("rate limit exceeded"),
            "expected rate-limit message, got: {err}"
        );
    }

    #[test]
    fn parse_claude_usage_response_returns_error_with_unknown_message_when_message_missing() {
        let body = serde_json::json!({ "error": {} });
        let err = super::parse_claude_usage_response(&body).expect_err("api error");
        assert!(err.contains("unknown"));
    }

    #[test]
    fn parse_claude_usage_response_skips_windows_missing_required_fields() {
        // Schema-drift smoke: a window object missing `resets_at` should be
        // dropped rather than producing a panic.
        let body = serde_json::json!({
            "five_hour": { "utilization": 10.0 },
            "seven_day": { "resets_at": "2026-04-30T00:00:00Z" },
            "extra_usage": null
        });
        let usage = super::parse_claude_usage_response(&body).expect("parse usage");
        assert!(usage.five_hour.is_none(), "no resets_at → window dropped");
        assert!(usage.seven_day.is_none(), "no utilization → window dropped");
        assert!(usage.extra_usage.is_none());
    }

    #[test]
    fn parse_claude_usage_response_skips_extra_usage_missing_required_field() {
        let body = serde_json::json!({
            "extra_usage": { "monthly_limit": 50.0 }  // missing is_enabled
        });
        let usage = super::parse_claude_usage_response(&body).expect("parse");
        assert!(
            usage.extra_usage.is_none(),
            "extra_usage without is_enabled should be dropped"
        );
    }

    #[test]
    fn parse_claude_usage_response_skips_window_with_malformed_resets_at() {
        let body = serde_json::json!({
            "five_hour": { "utilization": 10.0, "resets_at": "not-a-date" }
        });
        let usage = super::parse_claude_usage_response(&body).expect("parse");
        assert!(usage.five_hour.is_none());
    }

    // ── codex usage + gate from captured snapshot ───────────────────────────
    fn codex_snapshot_with_weekly(secondary_used: f64) -> super::CodexRateLimitSnapshot {
        super::CodexRateLimitSnapshot {
            limit_name: Some("gpt-5.2-codex".into()),
            primary: Some(crate::models::CodexUsageWindow {
                used_percent: 42.5,
                window_label: Some("5h".into()),
                window_minutes: Some(300),
                seconds_until_reset: Some(7200),
            }),
            secondary: Some(crate::models::CodexUsageWindow {
                used_percent: secondary_used,
                window_label: Some("7d".into()),
                window_minutes: Some(10080),
                seconds_until_reset: Some(86400),
            }),
            credits_balance: Some("$5.00".into()),
            credits_unlimited: false,
        }
    }

    #[test]
    fn codex_usage_from_snapshot_builds_usage() {
        let snapshot = codex_snapshot_with_weekly(12.0);
        // Ungated (active subscription/trial): usage is shown for reference only.
        let usage = super::codex_usage_from_snapshot(
            snapshot,
            crate::models::CodexPlanTier::Plus,
            super::CodexActivation::Ungated,
            0.0,
        );
        assert_eq!(usage.limit_name.as_deref(), Some("gpt-5.2-codex"));
        let primary = usage.primary.expect("primary window");
        assert_eq!(primary.used_percent, 42.5);
        assert_eq!(usage.credits_balance.as_deref(), Some("$5.00"));
        assert!(usage.optimization_allowed);
        assert!(
            !usage.should_nudge,
            "12% weekly is below the first nudge threshold"
        );
        assert_eq!(usage.weekly_used_percent, Some(12.0));
        assert_eq!(
            usage.recommended_subscription_tier,
            Some(crate::models::HeadroomSubscriptionTier::Pro),
            "Plus maps to Headroom Pro"
        );
    }

    #[test]
    fn codex_gate_hard_blocks_post_trial_account() {
        // No usable free plan post-trial: a gated account (no subscription, no
        // active trial) hard-blocks Codex regardless of weekly usage, and
        // still surfaces the plan to upgrade to.
        for weekly in [8.0, 36.0, 80.0] {
            let usage = super::codex_usage_from_snapshot(
                codex_snapshot_with_weekly(weekly),
                crate::models::CodexPlanTier::Pro,
                super::CodexActivation::HardBlock,
                0.0,
            );
            assert!(
                !usage.optimization_allowed,
                "{weekly}% weekly: post-trial Codex must be blocked"
            );
            assert!(usage.should_nudge);
            assert!(matches!(
                usage.gate_reason,
                Some(crate::models::PricingGateReason::TrialEnded)
            ));
            assert_eq!(
                usage.recommended_subscription_tier,
                Some(crate::models::HeadroomSubscriptionTier::Max20x),
                "ChatGPT Pro maps to Headroom Max x20"
            );
        }
    }

    #[test]
    fn codex_gate_inactive_when_subscribed_or_trialing() {
        // Ungated (active subscription / trial) → never paused or nudged, even
        // past the disable threshold.
        let usage = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(80.0),
            crate::models::CodexPlanTier::Plus,
            super::CodexActivation::Ungated,
            0.0,
        );
        assert!(usage.optimization_allowed);
        assert!(!usage.should_nudge);
        assert!(usage.gate_reason.is_none());
    }

    #[test]
    fn codex_gate_meters_by_tier_when_metered() {
        // Grandfathered / clamped subscriber: metered identically to Claude.
        // Plus (Pro-equiv) pauses at 50%, Team (Max-equiv) at 25%, Free never.
        let plus_ok = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(40.0),
            crate::models::CodexPlanTier::Plus,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(plus_ok.optimization_allowed, "Plus at 40% is below 50%");

        let plus_paused = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(55.0),
            crate::models::CodexPlanTier::Plus,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(!plus_paused.optimization_allowed, "Plus at 55% >= 50%");
        assert!(matches!(
            plus_paused.gate_reason,
            Some(crate::models::PricingGateReason::CodexWeeklyUsageLimitReached)
        ));

        // Team/Business meter in the Pro (50%) band since the 2026-08-18 remap.
        let team_ok = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(30.0),
            crate::models::CodexPlanTier::Team,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(team_ok.optimization_allowed, "Team at 30% is below 50%");

        let pro_paused = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(30.0),
            crate::models::CodexPlanTier::Pro,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(!pro_paused.optimization_allowed, "Pro at 30% >= 25%");

        // Free is ungated (100%) even when metered and past any threshold.
        let free = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(95.0),
            crate::models::CodexPlanTier::Free,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(free.optimization_allowed, "Free Codex is never paused");
        assert!(free.gate_reason.is_none());
    }

    fn codex_window(
        used_percent: f64,
        window_minutes: Option<i64>,
    ) -> crate::models::CodexUsageWindow {
        crate::models::CodexUsageWindow {
            used_percent,
            window_label: None,
            window_minutes,
            seconds_until_reset: None,
        }
    }

    fn codex_snapshot(
        primary: Option<crate::models::CodexUsageWindow>,
        secondary: Option<crate::models::CodexUsageWindow>,
    ) -> super::CodexRateLimitSnapshot {
        super::CodexRateLimitSnapshot {
            limit_name: None,
            primary,
            secondary,
            credits_balance: None,
            credits_unlimited: false,
        }
    }

    #[test]
    fn codex_gate_meters_long_primary_when_plan_reports_no_weekly_window() {
        // Regression: `GET /wham/usage` returns a null `secondary_window` on
        // every plan measured so far, putting the long window in `primary`
        // instead (2026-08-17, live accounts: free = 30-day primary, Plus =
        // 7-day primary, both with secondary null). Metering `secondary` alone
        // made that indistinguishable from "under the limit", so a clamped
        // subscriber was never gated.
        let snapshot = codex_snapshot(Some(codex_window(30.0, Some(43_200))), None);
        let usage = super::codex_usage_from_snapshot(
            snapshot,
            crate::models::CodexPlanTier::Pro,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(
            !usage.optimization_allowed,
            "ChatGPT Pro at 30% of its only (30-day) window is past the 25% cap"
        );
        assert!(matches!(
            usage.gate_reason,
            Some(crate::models::PricingGateReason::CodexWeeklyUsageLimitReached)
        ));
        assert_eq!(
            usage.weekly_used_percent,
            Some(30.0),
            "the reported percent must be the window the gate used, not the empty secondary slot"
        );
    }

    #[test]
    fn codex_gate_meters_the_plus_weekly_window_reported_as_primary() {
        // Exact shape measured on a live ChatGPT Plus account 2026-08-17:
        // `primary_window` = 604800s (10080 minutes), `secondary_window` = null.
        // Plus is the largest paid Codex cohort in the fleet, so this shape --
        // not the assumed 5h-primary/weekly-secondary pair -- is the common case.
        let plus = |used| codex_snapshot(Some(codex_window(used, Some(10_080))), None);

        let under = super::codex_usage_from_snapshot(
            plus(40.0),
            crate::models::CodexPlanTier::Plus,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(
            under.optimization_allowed,
            "Plus at 40% is below its 50% cap"
        );

        let over = super::codex_usage_from_snapshot(
            plus(55.0),
            crate::models::CodexPlanTier::Plus,
            super::CodexActivation::Metered,
            0.0,
        );
        assert!(
            !over.optimization_allowed,
            "Plus at 55% of its weekly window must pause, even though it arrived as `primary`"
        );
        assert_eq!(
            super::codex_usage_windows_summary(&plus(55.0)),
            "primary=55@10080",
            "the telemetry must show the window length, since the slot name does not identify it"
        );
        assert_eq!(
            over.weekly_used_percent,
            Some(55.0),
            "a Plus user's weekly percent must be reported, not swallowed by the null secondary"
        );
    }

    #[test]
    fn codex_gate_prefers_the_longest_window_and_ignores_short_ones() {
        // A 5-hour primary must never drive the weekly ladder: 25% of five
        // hours would pause a paying user within the afternoon.
        let both = codex_snapshot(
            Some(codex_window(90.0, Some(300))),
            Some(codex_window(10.0, Some(10_080))),
        );
        assert!(
            super::codex_usage_from_snapshot(
                both,
                crate::models::CodexPlanTier::Pro,
                super::CodexActivation::Metered,
                0.0,
            )
            .optimization_allowed,
            "weekly window at 10% governs, not the 5h window at 90%"
        );

        // ponytail ceiling, asserted so it is a decision and not a surprise:
        // a plan whose only window is shorter than a day still fails open.
        let short_only = codex_snapshot(Some(codex_window(90.0, Some(300))), None);
        assert!(
            super::codex_usage_from_snapshot(
                short_only,
                crate::models::CodexPlanTier::Pro,
                super::CodexActivation::Metered,
                0.0,
            )
            .optimization_allowed,
            "no window of at least a day: no weekly signal, stays allowed"
        );

        // An undeclared window length keeps today's behaviour (secondary is
        // assumed weekly) rather than being dropped.
        let no_length = codex_snapshot(None, Some(codex_window(30.0, None)));
        assert!(
            !super::codex_usage_from_snapshot(
                no_length,
                crate::models::CodexPlanTier::Pro,
                super::CodexActivation::Metered,
                0.0,
            )
            .optimization_allowed,
            "secondary without a declared length still meters"
        );
    }

    #[test]
    fn codex_hard_block_needs_no_rate_limit_snapshot() {
        let usage = super::codex_usage_from_snapshot(
            crate::models::CodexRateLimitSnapshot::default(),
            CodexPlanTier::Plus,
            super::CodexActivation::HardBlock,
            0.0,
        );
        assert!(!usage.optimization_allowed);
        assert!(matches!(
            usage.gate_reason,
            Some(PricingGateReason::TrialEnded)
        ));
    }

    #[test]
    fn claude_usage_windows_summary_matches_codex_wire_shape() {
        let window = |utilization: f64| crate::models::ClaudeUsageWindow {
            utilization,
            resets_at: Utc::now(),
        };
        let usage = crate::models::ClaudeUsage {
            five_hour: Some(window(12.4)),
            seven_day: Some(window(63.6)),
            extra_usage: None,
        };
        assert_eq!(
            super::claude_usage_windows_summary(&usage),
            "five_hour=12@300;seven_day=64@10080"
        );
        let seven_only = crate::models::ClaudeUsage {
            five_hour: None,
            seven_day: Some(window(5.0)),
            extra_usage: None,
        };
        assert_eq!(
            super::claude_usage_windows_summary(&seven_only),
            "seven_day=5@10080"
        );
    }

    #[test]
    fn codex_usage_windows_summary_reports_window_shape() {
        assert_eq!(
            super::codex_usage_windows_summary(&codex_snapshot(
                Some(codex_window(99.4, Some(43_200))),
                Some(codex_window(12.0, Some(10_080))),
            )),
            "primary=99@43200;secondary=12@10080"
        );
        assert_eq!(
            super::codex_usage_windows_summary(&codex_snapshot(
                Some(codex_window(99.0, Some(43_200))),
                None
            )),
            "primary=99@43200"
        );
        assert_eq!(
            super::codex_usage_windows_summary(&codex_snapshot(
                None,
                Some(codex_window(12.0, None))
            )),
            "secondary=12"
        );
        assert_eq!(
            super::codex_usage_windows_summary(&codex_snapshot(None, None)),
            "none",
            "an empty snapshot reports that it carried nothing"
        );

        let with_credits = |balance: Option<&str>, unlimited| super::CodexRateLimitSnapshot {
            credits_balance: balance.map(str::to_string),
            credits_unlimited: unlimited,
            ..codex_snapshot(Some(codex_window(33.0, Some(10_080))), None)
        };
        assert_eq!(
            super::codex_usage_windows_summary(&with_credits(Some("812.5"), false)),
            "primary=33@10080;credits=812.5"
        );
        assert_eq!(
            super::codex_usage_windows_summary(&with_credits(None, true)),
            "primary=33@10080;credits=unlimited"
        );
        assert_eq!(
            super::codex_usage_windows_summary(&super::CodexRateLimitSnapshot {
                credits_balance: Some("100\r\nX-Evil: 1".into()),
                credits_unlimited: false,
                ..codex_snapshot(None, None)
            }),
            "credits=100X-Evil1",
            "credits-only snapshot reports credits; CR/LF/space/colon are stripped (`-` stays for negative balances)"
        );
    }

    #[test]
    fn codex_plan_mapping_is_price_parity() {
        use crate::models::{
            headroom_tier_for_codex_plan, CodexPlanTier, HeadroomSubscriptionTier,
        };
        for plan in [
            CodexPlanTier::Go,
            CodexPlanTier::Plus,
            CodexPlanTier::Team,
            CodexPlanTier::Business,
        ] {
            assert_eq!(
                headroom_tier_for_codex_plan(&plan),
                Some(HeadroomSubscriptionTier::Pro)
            );
        }
        for plan in [
            CodexPlanTier::SelfServeBusinessUsageBased,
            CodexPlanTier::Edu,
        ] {
            assert_eq!(
                headroom_tier_for_codex_plan(&plan),
                Some(HeadroomSubscriptionTier::Max5x)
            );
        }
        for plan in [
            CodexPlanTier::Pro,
            CodexPlanTier::Enterprise,
            CodexPlanTier::EnterpriseCbpUsageBased,
        ] {
            assert_eq!(
                headroom_tier_for_codex_plan(&plan),
                Some(HeadroomSubscriptionTier::Max20x)
            );
        }
        assert_eq!(headroom_tier_for_codex_plan(&CodexPlanTier::Free), None);
        assert_eq!(
            headroom_tier_for_codex_plan(&CodexPlanTier::Unknown),
            Some(HeadroomSubscriptionTier::Max20x)
        );
        // Free has no mapping, so the usage path falls back to Pro as the entry
        // upgrade.
        let usage = super::codex_usage_from_snapshot(
            codex_snapshot_with_weekly(10.0),
            CodexPlanTier::Free,
            super::CodexActivation::HardBlock,
            0.0,
        );
        assert_eq!(
            usage.recommended_subscription_tier,
            Some(HeadroomSubscriptionTier::Pro)
        );
    }

    // ── headroom-web auth contract tests ────────────────────────────────────

    fn temp_app_state() -> (crate::state::AppState, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("headroom-pricing-test-{}", uuid::Uuid::new_v4()));
        let state = crate::state::AppState::new_in(dir.clone()).expect("app state");
        (state, dir)
    }

    fn drop_state(dir: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn spawn_canned_response_server(
        body: serde_json::Value,
        status_line: &'static str,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind canned server");
        let port = listener.local_addr().unwrap().port();
        let body_bytes = body.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_bytes.len(),
                body_bytes
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (port, handle)
    }

    #[test]
    fn request_auth_code_decodes_headroom_web_response() {
        let body = serde_json::json!({
            "email": "user@example.com",
            "expiresInSeconds": 600
        });
        let (port, server) = spawn_canned_response_server(body, "HTTP/1.1 200 OK");
        let (state, dir) = temp_app_state();

        let result = super::request_auth_code_with_base_url(
            &state,
            "user@example.com",
            &format!("http://127.0.0.1:{port}"),
        )
        .expect("request_auth_code succeeds");

        server.join().unwrap();
        assert_eq!(result.email, "user@example.com");
        assert_eq!(result.expires_in_seconds, 600);
        drop_state(dir);
    }

    #[test]
    fn request_auth_code_clamps_expiry_to_documented_maximum() {
        let body = serde_json::json!({
            "email": "user@example.com",
            "expiresInSeconds": 99999
        });
        let (port, server) = spawn_canned_response_server(body, "HTTP/1.1 200 OK");
        let (state, dir) = temp_app_state();

        let result = super::request_auth_code_with_base_url(
            &state,
            "user@example.com",
            &format!("http://127.0.0.1:{port}"),
        )
        .expect("request_auth_code succeeds");

        server.join().unwrap();
        assert_eq!(
            result.expires_in_seconds,
            super::AUTH_CODE_EXPIRY_SECONDS,
            "expiry clamped to documented maximum"
        );
        drop_state(dir);
    }

    #[test]
    fn funnel_step_post_sends_step_name_in_header() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let body = "{\"firstSeenAt\":\"2026-07-07T00:00:00Z\",\"graceEndsAt\":\"2026-07-10T00:00:00Z\"}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });

        let identity = super::IdentityPayload::device_only();
        super::post_grace_start_with_step_to(
            &identity,
            "signup_gate_shown",
            &format!("http://127.0.0.1:{port}"),
        )
        .expect("funnel post succeeds");

        server.join().unwrap();
        let request = rx.recv().expect("captured request");
        assert!(
            request.contains("POST /desktop/grace/start"),
            "wrong path:\n{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("x-headroom-funnel-step: signup_gate_shown"),
            "missing funnel-step header:\n{request}"
        );
    }

    #[test]
    fn funnel_step_retries_until_a_post_lands() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for status in ["HTTP/1.1 500 Internal Server Error", "HTTP/1.1 200 OK"] {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = "{}";
                let response = format!(
                    "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let identity = super::IdentityPayload::device_only();
        super::post_funnel_step_with_retries(
            &identity,
            "bootstrap_abandoned",
            &format!("http://127.0.0.1:{port}"),
            &[std::time::Duration::from_millis(20)],
        )
        .expect("second attempt lands after one failure");
        server.join().unwrap();
    }

    #[test]
    fn request_auth_code_rejects_invalid_email_before_calling_server() {
        let (state, dir) = temp_app_state();
        let result = super::request_auth_code_with_base_url(
            &state,
            "  ",
            "http://127.0.0.1:1", // would fail if reached
        );
        assert!(matches!(result, Err(msg) if msg.contains("valid email")));
        drop_state(dir);
    }

    #[test]
    fn request_auth_code_returns_error_on_5xx_response() {
        let body = serde_json::json!({"error": "internal"});
        let (port, server) =
            spawn_canned_response_server(body, "HTTP/1.1 500 Internal Server Error");
        let (state, dir) = temp_app_state();

        let err = super::request_auth_code_with_base_url(
            &state,
            "user@example.com",
            &format!("http://127.0.0.1:{port}"),
        )
        .expect_err("5xx surfaces as error");

        server.join().unwrap();
        assert!(err.contains("status 500"));
        drop_state(dir);
    }

    #[test]
    #[serial_test::serial]
    fn verify_auth_code_decodes_and_writes_session_token() {
        let _home_lock = crate::test_env_lock::lock_home();
        // Override HOME / XDG_DATA_HOME so the keychain debug store and
        // app_data_dir live in a fresh tempdir, not the dev's real profile.
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        std::env::set_var("HOME", scratch.path());
        std::env::set_var("XDG_DATA_HOME", scratch.path().join(".local").join("share"));
        crate::storage::ensure_data_dirs(&crate::storage::app_data_dir())
            .expect("ensure_data_dirs in scratch");

        let body = serde_json::json!({
            "sessionToken": "session-xyz",
            "account": {
                "email": "user@example.com",
                "trialStartedAt": "2026-04-01T00:00:00Z",
                "trialEndsAt": "2026-04-15T00:00:00Z",
                "trialActive": true,
                "subscriptionActive": false,
                "subscriptionTier": null,
                "inviteCode": null,
                "acceptedInvitesCount": 0,
                "inviteBonusPercent": 0
            },
            "launchDiscountActive": false
        });
        let (port, server) = spawn_canned_response_server(body, "HTTP/1.1 200 OK");
        let (state, dir) = temp_app_state();

        let result = super::verify_auth_code_with_base_url(
            &state,
            "user@example.com",
            "123456",
            None,
            &format!("http://127.0.0.1:{port}"),
        )
        .expect("verify_auth_code succeeds");

        server.join().unwrap();
        assert!(result.authenticated);
        let account = result.account.expect("account profile populated");
        assert_eq!(account.email, "user@example.com");
        assert!(account.trial_active);

        // Session token should have been written to the (debug) keychain.
        let stored = crate::keychain::read_secret(
            super::HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
            super::HEADROOM_ACCOUNT_SESSION_ACCOUNT,
        )
        .expect("read session token");
        assert_eq!(stored.as_deref(), Some("session-xyz"));

        drop_state(dir);
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    fn verify_auth_code_rejects_blank_code_before_hitting_server() {
        let (state, dir) = temp_app_state();
        let err = super::verify_auth_code_with_base_url(
            &state,
            "user@example.com",
            "   ",
            None,
            "http://127.0.0.1:1",
        )
        .expect_err("blank code rejected");
        assert!(err.contains("authentication code"));
        drop_state(dir);
    }

    /// Issue #58: a headroom-web deploy switchover left the API unreachable for
    /// ~75s and the user was shown reqwest's raw
    /// "error sending request for url (https://...)". The message has to say
    /// what to do, and must not leak the endpoint.
    #[test]
    fn verify_auth_code_reports_an_unreachable_server_in_plain_language() {
        let _home_lock = crate::test_env_lock::lock_home();
        let (state, dir) = temp_app_state();
        let err = super::verify_auth_code_with_base_url(
            &state,
            "user@example.com",
            "123456",
            None,
            "http://127.0.0.1:1", // nothing listens here
        )
        .expect_err("unreachable server surfaces as error");

        assert!(
            err.contains("Check your connection"),
            "not actionable: {err}"
        );
        assert!(
            !err.contains("error sending request") && !err.contains("127.0.0.1"),
            "leaked reqwest internals: {err}"
        );
        drop_state(dir);
    }

    // ── activate_account / create_checkout_session / get_billing_portal_url ─

    /// Snapshot HOME / XDG_DATA_HOME, redirect them at a fresh tempdir,
    /// ensure_data_dirs, and seed a session token in the (debug) keychain
    /// so authenticated functions don't bail at the read_session_token step.
    /// Returns a guard whose Drop restores the original env vars.
    struct AuthedTestEnv {
        _scratch: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_xdg: Option<std::ffi::OsString>,
        prev_data_dir: Option<std::ffi::OsString>,
    }

    impl AuthedTestEnv {
        fn new(session_token: &str) -> Self {
            let scratch = tempfile::tempdir().expect("scratch tempdir");
            let prev_home = std::env::var_os("HOME");
            let prev_xdg = std::env::var_os("XDG_DATA_HOME");
            let prev_data_dir = std::env::var_os("HEADROOM_DATA_DIR");
            std::env::set_var("HOME", scratch.path());
            std::env::set_var("XDG_DATA_HOME", scratch.path().join(".local").join("share"));
            // Pin app_data_dir into the scratch dir: the debug keychain store
            // lives under it, and dirs::data_local_dir() ignores HOME/XDG on
            // macOS/Windows — under nextest (process per test) parallel tests
            // would otherwise share and race one real file-backed secret store.
            std::env::set_var(
                "HEADROOM_DATA_DIR",
                scratch.path().join(".local").join("share").join("Headroom"),
            );
            crate::storage::ensure_data_dirs(&crate::storage::app_data_dir())
                .expect("ensure_data_dirs in scratch");
            crate::keychain::write_secret(
                super::HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
                super::HEADROOM_ACCOUNT_SESSION_ACCOUNT,
                session_token,
            )
            .expect("seed session token");
            AuthedTestEnv {
                _scratch: scratch,
                prev_home,
                prev_xdg,
                prev_data_dir,
            }
        }
    }

    impl Drop for AuthedTestEnv {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_xdg.take() {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match self.prev_data_dir.take() {
                Some(v) => std::env::set_var("HEADROOM_DATA_DIR", v),
                None => std::env::remove_var("HEADROOM_DATA_DIR"),
            }
        }
    }

    fn sample_account_envelope_body() -> serde_json::Value {
        serde_json::json!({
            "account": {
                "email": "user@example.com",
                "trialStartedAt": "2026-04-01T00:00:00Z",
                "trialEndsAt": "2026-04-15T00:00:00Z",
                "trialActive": true,
                "subscriptionActive": false,
                "subscriptionTier": null,
                "inviteCode": null,
                "acceptedInvitesCount": 0,
                "inviteBonusPercent": 0
            },
            "launchDiscountActive": false
        })
    }

    #[test]
    #[serial_test::serial]
    fn activate_account_decodes_remote_envelope_and_returns_pricing_status() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(sample_account_envelope_body(), "HTTP/1.1 200 OK");
        let (state, dir) = temp_app_state();

        let result =
            super::activate_account_with_base_url(&state, 42, &format!("http://127.0.0.1:{port}"))
                .expect("activate_account succeeds");

        server.join().unwrap();
        assert!(result.authenticated);
        let account = result.account.expect("account profile populated");
        assert_eq!(account.email, "user@example.com");
        assert!(account.trial_active);
        drop_state(dir);
    }

    #[test]
    #[serial_test::serial]
    fn activate_account_reports_parse_error_on_non_json_body() {
        // A 200 whose body is an HTML error page (CDN/proxy interference,
        // Sentry RUST-58) must surface as a parse error, not a decode panic.
        use std::io::{Read, Write};
        let _env = AuthedTestEnv::new("session-xyz");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind raw server");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "<html><body>502 Bad Gateway</body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let (state, dir) = temp_app_state();

        let err =
            super::activate_account_with_base_url(&state, 0, &format!("http://127.0.0.1:{port}"))
                .expect_err("non-JSON body surfaces as parse error");
        server.join().unwrap();
        assert!(
            err.contains("Could not parse Headroom activation response"),
            "got: {err}"
        );
        drop_state(dir);
    }

    /// Serves `first_status` once, then `sample_account_envelope_body()` with a
    /// 200, so a test can assert the second attempt is the one that lands.
    fn spawn_flaky_activation_server(
        first_status: &'static str,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind flaky server");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let (status, body) = if attempt == 0 {
                    (first_status, "{}".to_string())
                } else {
                    (
                        "HTTP/1.1 200 OK",
                        sample_account_envelope_body().to_string(),
                    )
                };
                let response = format!(
                    "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (port, handle)
    }

    #[test]
    #[serial_test::serial]
    fn activate_account_retries_once_past_a_gateway_error() {
        // A 502 from the edge during a headroom-web deploy switchover must not
        // reach the user: the replacement instance answers the retry.
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_flaky_activation_server("HTTP/1.1 502 Bad Gateway");
        let (state, dir) = temp_app_state();

        let result = super::activate_account_with_retry_backoff(
            &state,
            0,
            &format!("http://127.0.0.1:{port}"),
            std::time::Duration::from_millis(20),
        )
        .expect("second attempt lands after one 502");

        server.join().unwrap();
        assert!(result.authenticated);
        assert_eq!(
            result.account.expect("account profile populated").email,
            "user@example.com"
        );
        drop_state(dir);
    }

    #[test]
    #[serial_test::serial]
    fn activate_account_does_not_retry_a_server_error_that_reached_the_handler() {
        // 500 means the handler ran and may already have written; only the
        // gateway-class statuses are safe to repeat. One attempt, then surface.
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_flaky_activation_server("HTTP/1.1 500 Internal Server Error");
        let (state, dir) = temp_app_state();

        let err = super::activate_account_with_retry_backoff(
            &state,
            0,
            &format!("http://127.0.0.1:{port}"),
            std::time::Duration::from_millis(20),
        )
        .expect_err("500 surfaces without a retry");

        assert!(err.contains("status 500"), "got: {err}");
        drop_state(dir);
        // The server thread still waits on its second connection; the test
        // asserting no retry happened is precisely why it is never made, so
        // let it be reaped at process exit rather than joining.
        drop(server);
    }

    #[test]
    fn retryable_gateway_statuses_are_limited_to_the_edge_class() {
        assert!(super::is_retryable_gateway_status(502));
        assert!(super::is_retryable_gateway_status(503));
        // Handler-side and success statuses must never be repeated.
        for status in [200, 401, 429, 500, 504] {
            assert!(
                !super::is_retryable_gateway_status(status),
                "{status} must not be retried"
            );
        }
    }

    #[test]
    fn transport_kind_slug_labels_a_connect_failure() {
        // The slug is the trailing fingerprint component, so a drift here
        // silently re-merges issues the split was added to separate.
        let connect = super::http_client()
            .expect("client")
            .post("http://127.0.0.1:1/desktop/account/activate")
            .send()
            .expect_err("port 1 refuses");
        assert_eq!(super::transport_kind_slug(&connect), "connect");
        // RUST-BE: the user-facing message is the same phrase for a refused
        // port, a DNS failure and a TLS-intercepting proxy, so the chain is
        // the only thing that tells them apart in Sentry.
        let chain = super::transport_cause_chain(&connect);
        assert!(chain.len() > connect.to_string().len(), "chain: {chain}");
        assert!(
            chain.chars().count() <= 400,
            "chain must stay bounded: {chain}"
        );
    }

    #[test]
    fn transient_transport_gate_caps_one_event_per_action_kind() {
        // RUST-7H: four offline machines re-reported the same captive portal
        // 229 times in 4 days. One event per (action, kind) per session keeps
        // users_impacted intact and drops the repetition.
        assert!(super::claim_transient_report_slot(
            "test-gate-alpha",
            "timeout"
        ));
        assert!(
            !super::claim_transient_report_slot("test-gate-alpha", "timeout"),
            "a repeat of the same failure must be dropped"
        );
        // A different kind on the same action is a different Sentry issue and
        // must still speak.
        assert!(super::claim_transient_report_slot(
            "test-gate-alpha",
            "connect"
        ));
        // Other actions are unaffected.
        assert!(super::claim_transient_report_slot(
            "test-gate-beta",
            "timeout"
        ));
    }

    #[test]
    #[serial_test::serial]
    fn activate_account_clears_session_and_returns_expired_error_on_401() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({}), "HTTP/1.1 401 Unauthorized");
        let (state, dir) = temp_app_state();

        let err =
            super::activate_account_with_base_url(&state, 0, &format!("http://127.0.0.1:{port}"))
                .expect_err("401 surfaces as expired session");
        server.join().unwrap();
        assert!(err.contains("session expired"));

        // Session token should be cleared after 401.
        let stored = crate::keychain::read_secret(
            super::HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
            super::HEADROOM_ACCOUNT_SESSION_ACCOUNT,
        )
        .expect("read after 401");
        assert!(stored.is_none(), "session token cleared after 401");

        drop_state(dir);
    }

    #[test]
    #[serial_test::serial]
    fn activate_account_requires_session_token() {
        let _home_lock = crate::test_env_lock::lock_home();
        // No AuthedTestEnv → no token in keychain. Override HOME so any
        // keychain read still goes to a tempdir, not the dev profile.
        let scratch = tempfile::tempdir().expect("scratch");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        let prev_data_dir = std::env::var_os("HEADROOM_DATA_DIR");
        std::env::set_var("HOME", scratch.path());
        std::env::set_var("XDG_DATA_HOME", scratch.path().join(".local").join("share"));
        // Pin app_data_dir too: the debug keychain store lives under it, and on
        // macOS/Windows dirs::data_local_dir() ignores HOME/XDG — a token
        // written by a parallel test process would otherwise be visible here.
        std::env::set_var(
            "HEADROOM_DATA_DIR",
            scratch.path().join(".local").join("share").join("Headroom"),
        );
        crate::storage::ensure_data_dirs(&crate::storage::app_data_dir()).unwrap();
        let (state, dir) = temp_app_state();

        let err = super::activate_account_with_base_url(&state, 0, "http://127.0.0.1:1")
            .expect_err("no session → error");
        assert!(err.contains("Sign in"));

        drop_state(dir);
        match prev_data_dir {
            Some(v) => std::env::set_var("HEADROOM_DATA_DIR", v),
            None => std::env::remove_var("HEADROOM_DATA_DIR"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn create_checkout_session_returns_url_from_response() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_canned_response_server(
            serde_json::json!({ "url": "https://buy.polar.sh/abc123" }),
            "HTTP/1.1 200 OK",
        );

        let url = super::create_checkout_session_with_base_url(
            HeadroomSubscriptionTier::Pro,
            BillingPeriod::Annual,
            &format!("http://127.0.0.1:{port}"),
        )
        .expect("checkout session succeeds");
        server.join().unwrap();

        assert_eq!(url, "https://buy.polar.sh/abc123");
    }

    #[test]
    #[serial_test::serial]
    fn create_checkout_session_surfaces_api_error_message() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_canned_response_server(
            serde_json::json!({ "error": "Plan unavailable in your region" }),
            "HTTP/1.1 400 Bad Request",
        );

        let err = super::create_checkout_session_with_base_url(
            HeadroomSubscriptionTier::Pro,
            BillingPeriod::Annual,
            &format!("http://127.0.0.1:{port}"),
        )
        .expect_err("4xx surfaces as error");
        server.join().unwrap();

        assert_eq!(err, "Plan unavailable in your region");
    }

    #[test]
    #[serial_test::serial]
    fn create_checkout_session_falls_back_to_status_message_when_no_api_error() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({}), "HTTP/1.1 502 Bad Gateway");

        let err = super::create_checkout_session_with_base_url(
            HeadroomSubscriptionTier::Pro,
            BillingPeriod::Annual,
            &format!("http://127.0.0.1:{port}"),
        )
        .expect_err("error");
        server.join().unwrap();

        assert!(err.contains("status 502"));
    }

    #[test]
    #[serial_test::serial]
    fn create_checkout_session_clears_session_on_401() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({}), "HTTP/1.1 401 Unauthorized");

        let err = super::create_checkout_session_with_base_url(
            HeadroomSubscriptionTier::Pro,
            BillingPeriod::Annual,
            &format!("http://127.0.0.1:{port}"),
        )
        .expect_err("401");
        server.join().unwrap();
        assert!(err.contains("session expired"));

        let stored = crate::keychain::read_secret(
            super::HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
            super::HEADROOM_ACCOUNT_SESSION_ACCOUNT,
        )
        .unwrap();
        assert!(stored.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn reactivate_subscription_succeeds_on_200() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({ "ok": true }), "HTTP/1.1 200 OK");

        super::reactivate_subscription_with_base_url(&format!("http://127.0.0.1:{port}"))
            .expect("reactivate succeeds");
        server.join().unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn reactivate_subscription_surfaces_api_error_message() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_canned_response_server(
            serde_json::json!({ "error": "Subscription is not scheduled for cancellation." }),
            "HTTP/1.1 422 Unprocessable Entity",
        );

        let err = super::reactivate_subscription_with_base_url(&format!("http://127.0.0.1:{port}"))
            .expect_err("4xx surfaces as error");
        server.join().unwrap();
        assert_eq!(err, "Subscription is not scheduled for cancellation.");
    }

    #[test]
    #[serial_test::serial]
    fn reactivate_subscription_clears_session_on_401() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({}), "HTTP/1.1 401 Unauthorized");

        let err = super::reactivate_subscription_with_base_url(&format!("http://127.0.0.1:{port}"))
            .expect_err("401");
        server.join().unwrap();
        assert!(err.contains("session expired"));

        let stored = crate::keychain::read_secret(
            super::HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
            super::HEADROOM_ACCOUNT_SESSION_ACCOUNT,
        )
        .unwrap();
        assert!(stored.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn get_billing_portal_url_returns_url_from_response() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_canned_response_server(
            serde_json::json!({ "url": "https://billing.polar.sh/customer/abc" }),
            "HTTP/1.1 200 OK",
        );

        let url =
            super::get_billing_portal_url_with_base_url(&format!("http://127.0.0.1:{port}"), None)
                .expect("billing portal succeeds");
        server.join().unwrap();

        assert_eq!(url, "https://billing.polar.sh/customer/abc");
    }

    #[test]
    #[serial_test::serial]
    fn get_billing_portal_url_surfaces_api_error_message() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) = spawn_canned_response_server(
            serde_json::json!({ "error": "Customer not found" }),
            "HTTP/1.1 404 Not Found",
        );

        let err =
            super::get_billing_portal_url_with_base_url(&format!("http://127.0.0.1:{port}"), None)
                .expect_err("404 surfaces as error");
        server.join().unwrap();

        assert_eq!(err, "Customer not found");
    }

    #[test]
    #[serial_test::serial]
    fn cancellation_intent_posts_the_reason() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({ "offer": null }), "HTTP/1.1 200 OK");

        super::submit_cancellation_intent_with_base_url(
            &format!("http://127.0.0.1:{port}"),
            "too_expensive",
            "a bit steep",
        )
        .expect("intent posts");
        server.join().unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn cancellation_intent_clears_session_on_401() {
        let _env = AuthedTestEnv::new("session-xyz");
        let (port, server) =
            spawn_canned_response_server(serde_json::json!({}), "HTTP/1.1 401 Unauthorized");

        let err = super::submit_cancellation_intent_with_base_url(
            &format!("http://127.0.0.1:{port}"),
            "switched",
            "",
        )
        .expect_err("401");
        server.join().unwrap();
        assert!(err.contains("session expired"));

        let stored = crate::keychain::read_secret(
            super::HEADROOM_ACCOUNT_KEYCHAIN_SERVICE,
            super::HEADROOM_ACCOUNT_SESSION_ACCOUNT,
        )
        .unwrap();
        assert!(stored.is_none());
    }

    #[test]
    fn detect_tier_mismatch_flags_under_subscribed_pro() {
        let account = active_subscriber(HeadroomSubscriptionTier::Pro);
        let claude = empty_claude_profile(ClaudePlanTier::Max20x);
        assert_eq!(
            detect_tier_mismatch(&account, &claude, None),
            Some((
                HeadroomSubscriptionTier::Pro,
                HeadroomSubscriptionTier::Max20x,
                TierRecommendationSource::Claude
            ))
        );
    }

    #[test]
    fn detect_tier_mismatch_ignores_matching_or_higher_paid_tier() {
        let claude = empty_claude_profile(ClaudePlanTier::Pro);
        // Equal tiers.
        assert!(detect_tier_mismatch(
            &active_subscriber(HeadroomSubscriptionTier::Pro),
            &claude,
            None
        )
        .is_none());
        // Paid higher than Claude plan.
        assert!(detect_tier_mismatch(
            &active_subscriber(HeadroomSubscriptionTier::Max20x),
            &claude,
            None
        )
        .is_none());
    }

    #[test]
    fn detect_tier_mismatch_requires_confident_paid_claude_plan() {
        let account = active_subscriber(HeadroomSubscriptionTier::Pro);
        // Free carries no recommended paid tier.
        assert!(
            detect_tier_mismatch(&account, &empty_claude_profile(ClaudePlanTier::Free), None)
                .is_none()
        );
        // Unknown is undecodable, not a confident plan -> no recommendation, so
        // a Pro subscriber with no other signal sees no mismatch nudge.
        assert!(detect_tier_mismatch(
            &account,
            &empty_claude_profile(ClaudePlanTier::Unknown),
            None,
        )
        .is_none());
    }

    #[test]
    fn detect_tier_mismatch_unknown_claude_defers_to_codex_plan() {
        // George's case: Headroom Pro + Codex Plus (-> Pro) + Claude Unknown.
        // Unknown must not push Max x20; the Codex Plus -> Pro recommendation
        // matches the paid tier, so no mismatch fires.
        let account = active_subscriber(HeadroomSubscriptionTier::Pro);
        let claude = empty_claude_profile(ClaudePlanTier::Unknown);
        assert!(detect_tier_mismatch(&account, &claude, Some(CodexPlanTier::Plus)).is_none());
    }

    #[test]
    fn detect_tier_mismatch_codex_free_and_absent_yield_no_recommendation() {
        // User 861's case: Max x5 subscriber, Claude Max 5x, auth.json says
        // Codex Free. Free maps to no tier; absent evidence (None) must not
        // fire either. Explicit Unknown (evidence, unclassifiable) still
        // recommends Max x20 by design.
        let account = active_subscriber(HeadroomSubscriptionTier::Max5x);
        let claude = empty_claude_profile(ClaudePlanTier::Max5x);
        assert!(detect_tier_mismatch(&account, &claude, Some(CodexPlanTier::Free)).is_none());
        assert!(detect_tier_mismatch(&account, &claude, None).is_none());
        assert_eq!(
            detect_tier_mismatch(&account, &claude, Some(CodexPlanTier::Unknown)),
            Some((
                HeadroomSubscriptionTier::Max5x,
                HeadroomSubscriptionTier::Max20x,
                TierRecommendationSource::Codex
            ))
        );
    }

    #[test]
    fn detect_tier_mismatch_uses_codex_plan_when_claude_has_none() {
        // Codex Pro -> Max x20; Claude Free carries no recommendation.
        let account = active_subscriber(HeadroomSubscriptionTier::Pro);
        let claude = empty_claude_profile(ClaudePlanTier::Free);
        assert_eq!(
            detect_tier_mismatch(&account, &claude, Some(CodexPlanTier::Pro)),
            Some((
                HeadroomSubscriptionTier::Pro,
                HeadroomSubscriptionTier::Max20x,
                TierRecommendationSource::Codex
            ))
        );
    }

    #[test]
    fn detect_tier_mismatch_takes_higher_of_claude_and_codex() {
        // Claude Pro -> Pro, Codex Pro -> Max x20; the higher (Codex) wins.
        let account = active_subscriber(HeadroomSubscriptionTier::Pro);
        let claude = empty_claude_profile(ClaudePlanTier::Pro);
        assert_eq!(
            detect_tier_mismatch(&account, &claude, Some(CodexPlanTier::Pro)),
            Some((
                HeadroomSubscriptionTier::Pro,
                HeadroomSubscriptionTier::Max20x,
                TierRecommendationSource::Codex
            ))
        );
    }

    #[test]
    fn detect_tier_mismatch_reports_both_when_tiers_match() {
        // Claude Max x20 and Codex Pro both imply Max x20.
        let account = active_subscriber(HeadroomSubscriptionTier::Pro);
        let claude = empty_claude_profile(ClaudePlanTier::Max20x);
        assert_eq!(
            detect_tier_mismatch(&account, &claude, Some(CodexPlanTier::Pro)),
            Some((
                HeadroomSubscriptionTier::Pro,
                HeadroomSubscriptionTier::Max20x,
                TierRecommendationSource::Both
            ))
        );
    }

    #[test]
    fn detect_tier_mismatch_ignores_inactive_subscription() {
        let mut account = active_subscriber(HeadroomSubscriptionTier::Pro);
        account.subscription_active = false;
        let claude = empty_claude_profile(ClaudePlanTier::Max20x);
        assert!(detect_tier_mismatch(&account, &claude, None).is_none());
    }

    #[test]
    fn within_grace_mismatch_keeps_optimization_unlimited() {
        let (start, end) = grace();
        let status = evaluate_pricing_status_with_mismatch(
            true,
            start,
            end,
            true,
            None,
            Some(active_subscriber(HeadroomSubscriptionTier::Pro)),
            pro_profile_with_weekly(99.0),
            PricingPromo::default(),
            None,
            Some(mismatch(HeadroomSubscriptionTier::Max20x, false)),
        );
        assert!(status.optimization_allowed);
        assert!(!status.should_nudge);
        assert!(status.tier_mismatch.is_some());
        assert_eq!(
            status.recommended_subscription_tier,
            Some(HeadroomSubscriptionTier::Max20x)
        );
    }

    #[test]
    fn clamped_mismatch_applies_standard_usage_gate() {
        let (start, end) = grace();
        let status = evaluate_pricing_status_with_mismatch(
            true,
            start,
            end,
            true,
            None,
            Some(active_subscriber(HeadroomSubscriptionTier::Pro)),
            pro_profile_with_weekly(99.0),
            PricingPromo::default(),
            None,
            Some(mismatch(HeadroomSubscriptionTier::Max20x, true)),
        );
        // Over the disable threshold, the standard paid gate pauses optimization.
        assert!(!status.optimization_allowed);
        assert!(matches!(
            status.gate_reason,
            Some(PricingGateReason::WeeklyUsageLimitReached)
        ));
        assert!(status.tier_mismatch.is_some_and(|m| m.clamped));
    }

    #[test]
    fn codex_only_clamped_mismatch_leaves_claude_ungated() {
        // A ChatGPT Business seat on a Headroom Pro plan (Felix, user 278):
        // the Codex-implied tier exceeds Pro, but the Claude plan matches it.
        // The clamp must meter Codex only — Claude stays unlimited even at 99%
        // weekly usage.
        let (start, end) = grace();
        let status = evaluate_pricing_status_with_mismatch(
            true,
            start,
            end,
            true,
            None,
            Some(active_subscriber(HeadroomSubscriptionTier::Pro)),
            pro_profile_with_weekly(99.0),
            PricingPromo::default(),
            None,
            Some(TierMismatch {
                paid_tier: HeadroomSubscriptionTier::Pro,
                recommended_tier: HeadroomSubscriptionTier::Max5x,
                recommended_source: TierRecommendationSource::Codex,
                grace_ends_at: Utc::now(),
                clamped: true,
                claude_undercovered: false,
                codex_undercovered: true,
            }),
        );
        assert!(status.optimization_allowed);
        assert!(status.gate_reason.is_none());
        // The banner still shows: the mismatch itself is real and upgradeable.
        assert!(status.tier_mismatch.is_some_and(|m| m.clamped));
    }
}
