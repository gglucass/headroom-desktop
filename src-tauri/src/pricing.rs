use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::device;
use crate::keychain;
use crate::models::{
    BillingPeriod, ClaudeAccountProfile, ClaudeAuthMethod, ClaudePlanTier, ClaudeUsage,
    ClaudeUsageWindow, HeadroomAccountProfile, HeadroomAuthCodeRequest, HeadroomPricingStatus,
    HeadroomSubscriptionTier, PricingGateReason,
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
// Set to true in dev builds to skip sign-in requirement (indefinite trial)
#[cfg(debug_assertions)]
const INDEFINITE_TRIAL: bool = true;
const AUTH_CODE_EXPIRY_SECONDS: u64 = 900;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalPricingState {
    first_seen_at: DateTime<Utc>,
    #[serde(default)]
    reconcile_with_server: bool,
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
}

impl IdentityPayload {
    fn for_state(state: &AppState) -> Self {
        let claude = state.cached_claude_profile();
        Self::build(Some(&claude))
    }

    fn device_only() -> Self {
        Self::build(None)
    }

    fn build(claude: Option<&ClaudeAccountProfile>) -> Self {
        let device = device::current();
        Self {
            device_id: device.machine_id_digest,
            chopratejas_instance_id: device.chopratejas_instance_id,
            claude_account_uuid: claude.and_then(|p| p.account_uuid.clone()),
            claude_email: claude.and_then(|p| p.email.clone()),
        }
    }

    fn apply_headers(&self, mut builder: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
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
        builder
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
    /// e.g. "default_claude_ai", "claude_max_5x", "claude_max_20x"
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteAccountEnvelope {
    account: RemoteAccountResponse,
    #[serde(default)]
    launch_discount_active: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteAccountResponse {
    email: String,
    trial_started_at: Option<DateTime<Utc>>,
    trial_ends_at: Option<DateTime<Utc>>,
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
    invite_code: Option<String>,
    accepted_invites_count: usize,
    invite_bonus_percent: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyCodeResponse {
    session_token: String,
    account: RemoteAccountResponse,
    #[serde(default)]
    launch_discount_active: bool,
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
    Other,
}

pub fn get_pricing_status(state: &AppState) -> Result<HeadroomPricingStatus, String> {
    let local_state = reconcile_local_state_with_server(state)?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let local_grace_active = Utc::now() < local_grace_ends_at;
    let session_token = read_session_token()?;
    let identity = IdentityPayload::for_state(state);
    let (authenticated, account, account_sync_error, launch_discount_active) =
        if let Some(token) = session_token.as_deref() {
            let envelope_result = fetch_remote_account(token, &identity);
            let launch_discount_active = envelope_result
                .as_ref()
                .map(|e| e.launch_discount_active)
                .unwrap_or(false);
            let account_result = envelope_result.map(|e| e.account);
            let (auth, acc, err) = merge_background_account_sync(Some(token), account_result);
            (auth, acc, err, launch_discount_active)
        } else {
            let launch_discount_active = fetch_public_config()
                .map(|c| c.launch_discount_active)
                .unwrap_or(false);
            (false, None, None, launch_discount_active)
        };

    let claude = detect_claude_profile(state);

    Ok(evaluate_pricing_status(
        authenticated,
        local_state.first_seen_at,
        local_grace_ends_at,
        local_grace_active,
        account_sync_error,
        account,
        claude,
        launch_discount_active,
    ))
}

pub fn request_auth_code(state: &AppState, email: &str) -> Result<HeadroomAuthCodeRequest, String> {
    let trimmed = email.trim().to_ascii_lowercase();
    if trimmed.is_empty() || !trimmed.contains('@') {
        return Err("Enter a valid email address.".into());
    }

    let response = http_client()?
        .post(api_url("desktop/auth/request_code"))
        .json(&RequestCodePayload {
            email: &trimmed,
            identity: IdentityPayload::for_state(state),
        })
        .send()
        .map_err(|err| {
            let msg = format!("Could not request sign-in code: {err}");
            sentry::capture_message(&msg, sentry::Level::Error);
            msg
        })?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let msg = format!("Could not request sign-in code (status {status}).");
        if status >= 500 {
            sentry::capture_message(&msg, sentry::Level::Error);
        }
        return Err(msg);
    }

    let body: RequestCodeResponse = response
        .json()
        .map_err(|err| {
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
    let trimmed_email = email.trim().to_ascii_lowercase();
    let trimmed_code = code.trim();
    if trimmed_email.is_empty() || !trimmed_email.contains('@') {
        return Err("Enter a valid email address.".into());
    }
    if trimmed_code.is_empty() {
        return Err("Enter the authentication code from your email.".into());
    }

    let response = http_client()?
        .post(api_url("desktop/auth/verify_code"))
        .json(&VerifyCodePayload {
            email: &trimmed_email,
            code: trimmed_code,
            invite_code: invite_code.map(str::trim).filter(|value| !value.is_empty()),
            identity: IdentityPayload::for_state(state),
        })
        .send()
        .map_err(|err| format!("Could not verify sign-in code: {err}"))?;

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

    Ok(evaluate_pricing_status(
        true,
        local_state.first_seen_at,
        local_grace_ends_at,
        Utc::now() < local_grace_ends_at,
        None,
        Some(remote_account_to_profile(body.account)),
        claude,
        body.launch_discount_active,
    ))
}

pub fn sign_out() -> Result<(), String> {
    clear_session_token()
}

pub fn activate_account(
    state: &AppState,
    lifetime_tokens_saved: u64,
) -> Result<HeadroomPricingStatus, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before activating desktop access.".to_string())?;
    let identity = IdentityPayload::for_state(state);
    let builder = http_client()?
        .post(api_url("desktop/account/activate"))
        .header("Authorization", format!("Bearer {token}"));
    let response = identity
        .apply_headers(builder)
        .json(&serde_json::json!({ "lifetime_tokens_saved": lifetime_tokens_saved }))
        .send()
        .map_err(|err| {
            let msg = format!("Could not activate Headroom desktop access: {err}");
            sentry::capture_message(&msg, sentry::Level::Error);
            msg
        })?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let msg = format!("Could not activate Headroom desktop access (status {status}).");
        if status >= 500 {
            sentry::capture_message(&msg, sentry::Level::Error);
        }
        return Err(msg);
    }

    let body: RemoteAccountEnvelope = response
        .json()
        .map_err(|err| {
            let msg = format!("Could not parse Headroom activation response: {err}");
            sentry::capture_message(&msg, sentry::Level::Error);
            msg
        })?;
    let local_state = reconcile_local_state_with_server(state)?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let claude = detect_claude_profile(state);

    Ok(evaluate_pricing_status(
        true,
        local_state.first_seen_at,
        local_grace_ends_at,
        Utc::now() < local_grace_ends_at,
        None,
        Some(remote_account_to_profile(body.account)),
        claude,
        body.launch_discount_active,
    ))
}

/// Fire-and-forget: reports a milestone to the server so it can trigger
/// the feedback email for users who were below the threshold at sign-up.
/// Silently no-ops if the user is not signed in or the request fails.
pub fn report_milestone(milestone_tokens_saved: u64) {
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
        .json(&serde_json::json!({ "milestone_tokens_saved": milestone_tokens_saved }))
        .send();
}

pub fn create_checkout_session(
    subscription_tier: HeadroomSubscriptionTier,
    billing_period: BillingPeriod,
) -> Result<String, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before starting checkout.".to_string())?;
    let response = http_client()?
        .post(api_url("desktop/checkout"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&CheckoutSessionPayload { subscription_tier, billing_period })
        .send()
        .map_err(|err| format!("Could not create checkout session: {err}"))?;

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

pub fn get_billing_portal_url() -> Result<String, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before accessing billing.".to_string())?;
    let response = http_client()?
        .get(api_url("desktop/billing_portal"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|err| format!("Could not reach billing portal: {err}"))?;

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

pub fn fetch_claude_usage(state: &AppState) -> Result<ClaudeUsage, String> {
    use chrono::DateTime;

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

fn evaluate_pricing_status(
    authenticated: bool,
    local_grace_started_at: DateTime<Utc>,
    local_grace_ends_at: DateTime<Utc>,
    local_grace_active: bool,
    account_sync_error: Option<String>,
    account: Option<HeadroomAccountProfile>,
    claude: ClaudeAccountProfile,
    launch_discount_active: bool,
) -> HeadroomPricingStatus {
    #[cfg(debug_assertions)]
    let local_grace_active = if INDEFINITE_TRIAL { true } else { local_grace_active };
    let needs_authentication = !authenticated && !local_grace_active;
    let mut optimization_allowed = true;
    let mut should_nudge = false;
    let mut gate_reason = None;
    let gate_message: String;
    let mut nudge_threshold_percent = None;
    let mut disable_threshold_percent = None;
    let mut effective_disable_threshold_percent = None;
    let mut recommended_subscription_tier = None;
    let mut recommended_subscription_price_usd = None;

    if needs_authentication {
        optimization_allowed = false;
        gate_reason = Some(PricingGateReason::SignInRequired);
        gate_message =
            "Create a Headroom account to unlock your 14-day trial and keep optimization enabled."
                .into();
    } else if let Some(account) = account.as_ref() {
        if account.subscription_active {
            gate_message = "Headroom subscription active. Optimization stays fully enabled.".into();
        } else if account.trial_active {
            gate_message =
                "Your 14-day Headroom trial is active with unlimited optimization.".into();
        } else {
            let pricing = pricing_policy_for_plan(&claude.plan_tier);
            nudge_threshold_percent = pricing
                .as_ref()
                .map(|policy| policy.nudge_threshold_percent);
            disable_threshold_percent = pricing
                .as_ref()
                .map(|policy| policy.disable_threshold_percent);
            effective_disable_threshold_percent = pricing.as_ref().map(|policy| {
                (policy.disable_threshold_percent + account.invite_bonus_percent)
                    .min(policy.disable_threshold_percent + 50.0)
            });
            recommended_subscription_tier = pricing
                .as_ref()
                .map(|policy| policy.recommended_tier.clone());
            recommended_subscription_price_usd =
                pricing.as_ref().map(|policy| policy.monthly_price_usd);

            match claude.plan_tier {
                ClaudePlanTier::Free => {
                    gate_message =
                        "Claude Free accounts can keep using Headroom without weekly usage gating."
                            .into();
                }
                ClaudePlanTier::Unknown => {
                    gate_message = "Send a Claude Code message through Headroom so we can detect your current Claude usage and apply the right pricing thresholds.".into();
                }
                _ => {
                    if let (Some(weekly_usage), Some(nudge), Some(disable)) = (
                        claude.weekly_utilization_pct,
                        nudge_threshold_percent,
                        effective_disable_threshold_percent,
                    ) {
                        if weekly_usage >= disable {
                            optimization_allowed = false;
                            gate_reason = Some(PricingGateReason::WeeklyUsageLimitReached);
                            gate_message = format!(
                                "Headroom is paused because you've reached {:.1}% of weekly Claude usage. Upgrade to raise your limit.",
                                weekly_usage
                            );
                        } else if weekly_usage >= nudge {
                            should_nudge = true;
                            gate_message = format!(
                                "You've reached {:.1}% of weekly Claude usage. Upgrade soon or invite others before Headroom pauses at {:.1}%.",
                                weekly_usage, disable
                            );
                        } else {
                            gate_message = format!(
                                "Headroom is active. It will start nudging at {:.1}% and pause at {:.1}% of weekly Claude usage for your detected plan.",
                                nudge, disable
                            );
                        }
                    } else {
                        gate_message = "Headroom is active. Send a Claude Code message through Headroom to sync your current weekly usage and pricing threshold.".into();
                    }
                }
            }
        }
    } else if authenticated {
        gate_message =
            "Headroom account connected, but pricing status could not be synced right now. Optimization stays enabled for now."
                .into();
    } else {
        gate_message =
            "Headroom is active during your first 72 hours. Create an account to unlock the 14-day trial before this grace period ends."
                .into();
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
        gate_reason,
        gate_message,
        nudge_threshold_percent,
        disable_threshold_percent,
        effective_disable_threshold_percent,
        recommended_subscription_tier,
        recommended_subscription_price_usd,
        claude,
        account,
        launch_discount_active,
    }
}

pub fn detect_claude_profile(state: &AppState) -> ClaudeAccountProfile {
    state.cached_claude_profile()
}

pub fn detect_claude_profile_uncached(state: &AppState) -> ClaudeAccountProfile {
    let Some(token) = state.current_bearer_token() else {
        // No token yet — proxy hasn't seen a request through. Return a minimal
        // profile so the app can show "send a message first" messaging.
        return ClaudeAccountProfile {
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
            weekly_utilization_pct: None,
            five_hour_utilization_pct: None,
            extra_usage_monthly_limit: None,
            profile_fetch_error: None,
        };
    };

    let (profile, profile_fetch_error) = match fetch_oauth_profile(&token) {
        Ok(p) => (Some(p), None),
        Err(msg) => (None, Some(msg)),
    };
    let usage = fetch_claude_usage(state).ok();

    let (plan_tier, plan_detection_source) = if let Some(ref p) = profile {
        detect_plan_tier_from_profile(p)
    } else {
        (ClaudePlanTier::Unknown, None)
    };

    ClaudeAccountProfile {
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
        weekly_utilization_pct: usage
            .as_ref()
            .and_then(|u| u.seven_day.as_ref().map(|w| w.utilization)),
        five_hour_utilization_pct: usage
            .as_ref()
            .and_then(|u| u.five_hour.as_ref().map(|w| w.utilization)),
        extra_usage_monthly_limit: usage
            .as_ref()
            .and_then(|u| u.extra_usage.as_ref().and_then(|e| e.monthly_limit)),
        profile_fetch_error,
    }
}

fn fetch_oauth_profile(token: &str) -> Result<ClaudeOauthProfile, String> {
    let response = http_client()?
        .get("https://api.anthropic.com/api/oauth/profile")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .send()
        .map_err(|_| {
            "Couldn't reach Anthropic to refresh your Claude plan. Check your internet connection \
             and we'll try again shortly."
                .to_string()
        })?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let user_msg = if status >= 500 {
            format!(
                "Anthropic is having trouble serving your Claude plan right now (HTTP {status}). \
                 We'll keep trying."
            )
        } else if status == 401 || status == 403 {
            "Anthropic rejected our request for your Claude plan. Try signing out of Claude Code \
             and back in."
                .to_string()
        } else {
            format!(
                "Anthropic returned an unexpected response for your Claude plan (HTTP {status}). \
                 We'll try again shortly."
            )
        };
        if status >= 500 {
            sentry::capture_message(
                &format!("Could not fetch Claude OAuth profile (status {status})."),
                sentry::Level::Error,
            );
        }
        return Err(user_msg);
    }

    let body: serde_json::Value = response.json().map_err(|err| {
        sentry::capture_message(
            &format!("Could not parse Claude OAuth profile: {err}"),
            sentry::Level::Error,
        );
        "We couldn't read the response from Anthropic for your Claude plan. Please report this if \
         it keeps happening."
            .to_string()
    })?;

    parse_oauth_profile_value(&body).ok_or_else(|| {
        "Anthropic's response didn't include your Claude account details. Please report this if \
         it keeps happening."
            .to_string()
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
    })
}

fn detect_plan_tier_from_profile(profile: &ClaudeOauthProfile) -> (ClaudePlanTier, Option<String>) {
    let Some(org) = profile.organization.as_ref() else {
        return (ClaudePlanTier::Free, Some("oauth_profile.account".into()));
    };

    if let Some(rate_limit_tier) = org.rate_limit_tier.as_deref() {
        let normalized = rate_limit_tier.trim().to_ascii_lowercase();
        if normalized.contains("20x") {
            return (
                ClaudePlanTier::Max20x,
                Some("oauth_profile.organization.rateLimitTier".into()),
            );
        }
        if normalized.contains("5x") {
            return (
                ClaudePlanTier::Max5x,
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
            if organization_type.eq_ignore_ascii_case("claude_pro")
                || organization_type.eq_ignore_ascii_case("claude_enterprise")
            {
                return (
                    ClaudePlanTier::Pro,
                    Some("oauth_profile.organization.organizationType".into()),
                );
            }
        }
    }

    if let Some(organization_type) = org.organization_type.as_deref() {
        let normalized = organization_type.trim().to_ascii_lowercase();
        if normalized == "claude_max" {
            return (
                ClaudePlanTier::Max5x,
                Some("oauth_profile.organization.organizationType".into()),
            );
        }
        if normalized == "claude_pro" || normalized == "claude_enterprise" {
            return (
                ClaudePlanTier::Pro,
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

    (
        ClaudePlanTier::Unknown,
        Some("oauth_profile.organization".into()),
    )
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
        trial_active: value.trial_active,
        subscription_active: value.subscription_active,
        subscription_tier: value.subscription_tier,
        subscription_started_at: value.subscription_started_at,
        subscription_renews_at: value.subscription_renews_at,
        subscription_amount_cents: value.subscription_amount_cents,
        subscription_billing_period: value.subscription_billing_period,
        subscription_discount_duration: value.subscription_discount_duration,
        subscription_discount_duration_in_months: value.subscription_discount_duration_in_months,
        invite_code: value.invite_code,
        accepted_invites_count: value.accepted_invites_count,
        invite_bonus_percent: value.invite_bonus_percent.min(50.0).max(0.0),
    }
}

fn merge_background_account_sync(
    session_token: Option<&str>,
    sync_result: Result<RemoteAccountResponse, RemoteAccountSyncError>,
) -> (bool, Option<HeadroomAccountProfile>, Option<String>) {
    if session_token.is_none() {
        return (false, None, None);
    }

    match sync_result {
        // Background polling should not silently drop the locally stored session.
        // Explicit auth-required actions still clear the token if the server says it
        // is expired, but passive refreshes keep the user signed in locally.
        Ok(account) => (true, Some(remote_account_to_profile(account)), None),
        Err(RemoteAccountSyncError::Unauthorized) => (
            true,
            None,
            Some("Headroom account connected, but your plan details could not be refreshed. Sign in again if this keeps happening.".into()),
        ),
        Err(RemoteAccountSyncError::Other) => (
            true,
            None,
            Some("Headroom account connected, but your plan details are unavailable right now.".into()),
        ),
    }
}

fn load_or_initialize_local_state() -> Result<LocalPricingState, String> {
    let path = local_state_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(state) = serde_json::from_slice::<LocalPricingState>(&bytes) {
            return Ok(state);
        }
    }

    let state = LocalPricingState {
        first_seen_at: Utc::now(),
        reconcile_with_server: true,
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
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(state)
            .map_err(|err| format!("Failed to serialize pricing state: {err}"))?,
    )
    .map_err(|err| format!("Failed to write pricing state {}: {err}", path.display()))
}

fn reconcile_local_state_with_server(state: &AppState) -> Result<LocalPricingState, String> {
    let mut local = load_or_initialize_local_state()?;
    let identity = IdentityPayload::for_state(state);
    match fetch_grace_start(&identity) {
        Ok(response) => {
            let server_first_seen = response.first_seen_at;
            let new_first_seen = if local.reconcile_with_server {
                server_first_seen.min(local.first_seen_at)
            } else {
                server_first_seen
            };
            if new_first_seen != local.first_seen_at || local.reconcile_with_server {
                local.first_seen_at = new_first_seen;
                local.reconcile_with_server = false;
                if let Err(err) = write_local_state(&local) {
                    sentry::capture_message(
                        &format!("Could not persist reconciled grace state: {err}"),
                        sentry::Level::Warning,
                    );
                }
            }
        }
        Err(_) => {
            // Server unreachable; keep whatever we have locally. reconcile_with_server
            // stays set if this is a fresh install so the next successful call wins.
        }
    }
    Ok(local)
}

fn fetch_grace_start(identity: &IdentityPayload) -> Result<GraceResponse, String> {
    let response = http_client()?
        .post(api_url("desktop/grace/start"))
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
    launch_discount_active: bool,
}

fn fetch_public_config() -> Option<PublicConfig> {
    let response = http_client().ok()?.get(api_url("desktop/config")).send().ok()?;
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
        .map_err(|_| RemoteAccountSyncError::Other)?
        .get(api_url("desktop/account"))
        .header("Authorization", format!("Bearer {token}"));
    let response = identity
        .apply_headers(builder)
        .send()
        .map_err(|_| RemoteAccountSyncError::Other)?;

    if response.status().as_u16() == 401 {
        return Err(RemoteAccountSyncError::Unauthorized);
    }

    if !response.status().is_success() {
        return Err(RemoteAccountSyncError::Other);
    }

    response
        .json::<RemoteAccountEnvelope>()
        .map_err(|_| RemoteAccountSyncError::Other)
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|err| format!("Could not build HTTP client: {err}"))
}

fn api_url(path: &str) -> String {
    // Runtime override is only honored in debug builds. In release builds an
    // attacker with persistence on the user's machine (e.g. a launchd plist)
    // could otherwise redirect every billing/auth call to a rogue host.
    #[cfg(debug_assertions)]
    let runtime_env = std::env::var("HEADROOM_ACCOUNT_API_BASE_URL").ok();
    #[cfg(not(debug_assertions))]
    let runtime_env: Option<String> = None;

    let base = resolve_account_api_base_url(
        runtime_env,
        option_env!("HEADROOM_ACCOUNT_API_BASE_URL"),
    );
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

#[derive(Debug, Clone)]
struct PricingPolicy {
    nudge_threshold_percent: f64,
    disable_threshold_percent: f64,
    recommended_tier: HeadroomSubscriptionTier,
    monthly_price_usd: f64,
}

fn pricing_policy_for_plan(plan: &ClaudePlanTier) -> Option<PricingPolicy> {
    match plan {
        ClaudePlanTier::Free => None,
        ClaudePlanTier::Pro => Some(PricingPolicy {
            nudge_threshold_percent: 10.0,
            disable_threshold_percent: 25.0,
            recommended_tier: HeadroomSubscriptionTier::Pro,
            monthly_price_usd: 2.5,
        }),
        ClaudePlanTier::Max5x => Some(PricingPolicy {
            nudge_threshold_percent: 10.0,
            disable_threshold_percent: 25.0,
            recommended_tier: HeadroomSubscriptionTier::Max5x,
            monthly_price_usd: 12.5,
        }),
        ClaudePlanTier::Max20x => Some(PricingPolicy {
            nudge_threshold_percent: 10.0,
            disable_threshold_percent: 25.0,
            recommended_tier: HeadroomSubscriptionTier::Max20x,
            monthly_price_usd: 25.0,
        }),
        ClaudePlanTier::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{
        detect_plan_tier_from_profile, evaluate_pricing_status, merge_background_account_sync,
        pricing_policy_for_plan, remote_account_to_profile, resolve_account_api_base_url,
        ClaudeOauthProfile, ClaudeOauthProfileAccount, ClaudeOauthProfileOrganization,
        HeadroomSubscriptionTier, IdentityPayload, LocalPricingState, RemoteAccountResponse,
        RemoteAccountSyncError, DEFAULT_ACCOUNT_API_BASE_URL,
    };
    use crate::models::{
        ClaudeAccountProfile, ClaudeAuthMethod, ClaudePlanTier, HeadroomAccountProfile,
        PricingGateReason,
    };

    fn sample_remote_account() -> RemoteAccountResponse {
        RemoteAccountResponse {
            email: "user@example.com".into(),
            trial_started_at: Some(Utc::now()),
            trial_ends_at: Some(Utc::now()),
            trial_active: true,
            subscription_active: true,
            subscription_tier: Some(HeadroomSubscriptionTier::Pro),
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            invite_code: Some("invite-code".into()),
            accepted_invites_count: 2,
            invite_bonus_percent: 10.0,
        }
    }

    #[test]
    fn identity_payload_serializes_with_camelcase_keys_and_skips_nulls() {
        let identity = IdentityPayload {
            device_id: "abc123".into(),
            chopratejas_instance_id: None,
            claude_account_uuid: Some("claude-uuid".into()),
            claude_email: None,
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["deviceId"], "abc123");
        assert_eq!(json["claudeAccountUuid"], "claude-uuid");
        assert!(json.get("chopratejasInstanceId").is_none());
        assert!(json.get("claudeEmail").is_none());
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
    fn unauthorized_background_sync_keeps_local_session_authenticated() {
        let (authenticated, account, error) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Unauthorized),
        );

        assert!(authenticated);
        assert!(account.is_none());
        assert!(error.is_some());
    }

    #[test]
    fn transient_background_sync_error_keeps_local_session_authenticated() {
        let (authenticated, account, error) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Other),
        );

        assert!(authenticated);
        assert!(account.is_none());
        assert!(error.is_some());
    }

    #[test]
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
    fn pricing_policy_uses_the_reduced_monthly_prices() {
        assert_eq!(
            pricing_policy_for_plan(&ClaudePlanTier::Pro).map(|policy| policy.monthly_price_usd),
            Some(2.5)
        );
        assert_eq!(
            pricing_policy_for_plan(&ClaudePlanTier::Max5x).map(|policy| policy.monthly_price_usd),
            Some(12.5)
        );
        assert_eq!(
            pricing_policy_for_plan(&ClaudePlanTier::Max20x).map(|policy| policy.monthly_price_usd),
            Some(25.0)
        );
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
            weekly_utilization_pct: None,
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

    fn trial_account() -> HeadroomAccountProfile {
        HeadroomAccountProfile {
            email: "user@example.com".into(),
            trial_started_at: Some(Utc::now()),
            trial_ends_at: Some(Utc::now()),
            trial_active: true,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: 0.0,
        }
    }

    fn expired_account(invite_bonus: f64) -> HeadroomAccountProfile {
        HeadroomAccountProfile {
            email: "user@example.com".into(),
            trial_started_at: None,
            trial_ends_at: None,
            trial_active: false,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: invite_bonus,
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
        );
        assert!(status.optimization_allowed);
        assert!(status.gate_reason.is_none());
    }

    #[test]
    fn free_tier_is_never_gated_by_weekly_usage() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            empty_claude_profile(ClaudePlanTier::Free),
            false,
        );
        assert!(status.optimization_allowed);
        assert!(!status.should_nudge);
        assert!(status.nudge_threshold_percent.is_none());
    }

    #[test]
    fn unknown_tier_surfaces_detection_prompt() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            empty_claude_profile(ClaudePlanTier::Unknown),
            false,
        );
        assert!(status.optimization_allowed);
        assert!(!status.should_nudge);
        assert!(status.gate_message.contains("detect"));
    }

    #[test]
    fn pro_below_nudge_threshold_stays_silent() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            pro_profile_with_weekly(5.0),
            false,
        );
        assert!(status.optimization_allowed);
        assert!(!status.should_nudge);
    }

    #[test]
    fn pro_between_nudge_and_disable_nudges() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            pro_profile_with_weekly(15.0),
            false,
        );
        assert!(status.optimization_allowed);
        assert!(status.should_nudge);
    }

    #[test]
    fn pro_at_disable_threshold_gates_optimization() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            pro_profile_with_weekly(25.0),
            false,
        );
        assert!(!status.optimization_allowed);
        assert!(matches!(
            status.gate_reason,
            Some(PricingGateReason::WeeklyUsageLimitReached)
        ));
    }

    #[test]
    fn invite_bonus_raises_disable_threshold() {
        let (start, end) = grace();
        // Pro disable=25; with +10 bonus -> 35. Usage=30 should not gate.
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(10.0)),
            pro_profile_with_weekly(30.0),
            false,
        );
        assert!(status.optimization_allowed);
        assert!(status.should_nudge);
        assert_eq!(status.effective_disable_threshold_percent, Some(35.0));
    }

    #[test]
    fn invite_bonus_is_capped_at_50_percentage_points() {
        let (start, end) = grace();
        // Even if the backend sent 200, the effective cap is +50.
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(200.0)),
            pro_profile_with_weekly(0.0),
            false,
        );
        assert_eq!(status.effective_disable_threshold_percent, Some(75.0));
    }

    #[test]
    fn missing_weekly_usage_keeps_optimization_on_for_paid_tier() {
        let (start, end) = grace();
        let status = evaluate_pricing_status(
            true,
            start,
            end,
            false,
            None,
            Some(expired_account(0.0)),
            empty_claude_profile(ClaudePlanTier::Pro),
            false,
        );
        assert!(status.optimization_allowed);
        assert!(!status.should_nudge);
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
            }),
        }
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
    fn detect_plan_tier_default_rate_limit_with_claude_max_is_max5x() {
        let p = oauth_profile(Some("default_claude_ai"), Some("claude_max"), Some(Utc::now()));
        assert!(matches!(
            detect_plan_tier_from_profile(&p).0,
            ClaudePlanTier::Max5x
        ));
    }

    #[test]
    fn detect_plan_tier_default_rate_limit_with_claude_pro_is_pro() {
        let p = oauth_profile(Some("default_claude_ai"), Some("claude_pro"), Some(Utc::now()));
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

    #[test]
    fn remote_account_clamps_invite_bonus_to_50() {
        let raw = RemoteAccountResponse {
            email: "a@b".into(),
            trial_started_at: None,
            trial_ends_at: None,
            trial_active: false,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: 999.0,
        };
        assert_eq!(remote_account_to_profile(raw).invite_bonus_percent, 50.0);
    }

    #[test]
    fn remote_account_clamps_negative_invite_bonus_to_zero() {
        let raw = RemoteAccountResponse {
            email: "a@b".into(),
            trial_started_at: None,
            trial_ends_at: None,
            trial_active: false,
            subscription_active: false,
            subscription_tier: None,
            subscription_started_at: None,
            subscription_renews_at: None,
            subscription_amount_cents: None,
            subscription_billing_period: None,
            subscription_discount_duration: None,
            subscription_discount_duration_in_months: None,
            invite_code: None,
            accepted_invites_count: 0,
            invite_bonus_percent: -10.0,
        };
        assert_eq!(remote_account_to_profile(raw).invite_bonus_percent, 0.0);
    }
}
