use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::keychain;
use crate::models::{
    ClaudeAccountProfile, ClaudeAuthMethod, ClaudePlanTier, ClaudeUsage, ClaudeUsageWindow,
    HeadroomAccountProfile, HeadroomAuthCodeRequest, HeadroomPricingStatus,
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
const LOCAL_GRACE_PERIOD_HOURS: i64 = 24;
const AUTH_CODE_EXPIRY_SECONDS: u64 = 900;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalPricingState {
    first_seen_at: DateTime<Utc>,
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
    invite_code: Option<String>,
    accepted_invites_count: usize,
    invite_bonus_percent: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyCodeResponse {
    session_token: String,
    account: RemoteAccountResponse,
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifyCodePayload<'a> {
    email: &'a str,
    code: &'a str,
    invite_code: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutSessionPayload {
    subscription_tier: HeadroomSubscriptionTier,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutSessionResponse {
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
    let local_state = load_or_initialize_local_state()?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let local_grace_active = Utc::now() < local_grace_ends_at;
    let session_token = read_session_token()?;
    let (authenticated, account) = if let Some(token) = session_token.as_deref() {
        merge_background_account_sync(Some(token), fetch_remote_account(token))
    } else {
        (false, None)
    };

    let claude = detect_claude_profile(state);

    Ok(evaluate_pricing_status(
        authenticated,
        local_state.first_seen_at,
        local_grace_ends_at,
        local_grace_active,
        account,
        claude,
    ))
}

pub fn request_auth_code(email: &str) -> Result<HeadroomAuthCodeRequest, String> {
    let trimmed = email.trim().to_ascii_lowercase();
    if trimmed.is_empty() || !trimmed.contains('@') {
        return Err("Enter a valid email address.".into());
    }

    let response = http_client()?
        .post(api_url("desktop/auth/request_code"))
        .json(&RequestCodePayload { email: &trimmed })
        .send()
        .map_err(|err| format!("Could not request sign-in code: {err}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Could not request sign-in code (status {}).",
            response.status().as_u16()
        ));
    }

    let body: RequestCodeResponse = response
        .json()
        .map_err(|err| format!("Could not parse sign-in response: {err}"))?;

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

    let local_state = load_or_initialize_local_state()?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let claude = detect_claude_profile(state);

    Ok(evaluate_pricing_status(
        true,
        local_state.first_seen_at,
        local_grace_ends_at,
        Utc::now() < local_grace_ends_at,
        Some(remote_account_to_profile(body.account)),
        claude,
    ))
}

pub fn sign_out() -> Result<(), String> {
    clear_session_token()
}

pub fn activate_account(state: &AppState) -> Result<HeadroomPricingStatus, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before activating desktop access.".to_string())?;
    let response = http_client()?
        .post(api_url("desktop/account/activate"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|err| format!("Could not activate Headroom desktop access: {err}"))?;

    if response.status().as_u16() == 401 {
        clear_session_token()?;
        return Err("Your Headroom session expired. Sign in again.".into());
    }

    if !response.status().is_success() {
        return Err(format!(
            "Could not activate Headroom desktop access (status {}).",
            response.status().as_u16()
        ));
    }

    let body: RemoteAccountEnvelope = response
        .json()
        .map_err(|err| format!("Could not parse Headroom activation response: {err}"))?;
    let local_state = load_or_initialize_local_state()?;
    let local_grace_ends_at = local_state.first_seen_at + Duration::hours(LOCAL_GRACE_PERIOD_HOURS);
    let claude = detect_claude_profile(state);

    Ok(evaluate_pricing_status(
        true,
        local_state.first_seen_at,
        local_grace_ends_at,
        Utc::now() < local_grace_ends_at,
        Some(remote_account_to_profile(body.account)),
        claude,
    ))
}

pub fn create_checkout_session(
    subscription_tier: HeadroomSubscriptionTier,
) -> Result<String, String> {
    let token = read_session_token()?
        .ok_or_else(|| "Sign in to Headroom before starting checkout.".to_string())?;
    let response = http_client()?
        .post(api_url("desktop/checkout"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&CheckoutSessionPayload { subscription_tier })
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

pub fn fetch_claude_usage(state: &AppState) -> Result<ClaudeUsage, String> {
    use chrono::DateTime;

    let access_token = state
        .claude_bearer_token
        .lock()
        .map_err(|_| "Token lock poisoned".to_string())?
        .clone()
        .ok_or_else(|| {
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
    account: Option<HeadroomAccountProfile>,
    claude: ClaudeAccountProfile,
) -> HeadroomPricingStatus {
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
                                "Headroom is paused because you've reached {:.1}% of weekly Claude usage. Upgrade or earn invite bonuses to raise your limit.",
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
            "Headroom is active during your first local day. Create an account to unlock the 14-day trial before this grace period ends."
                .into();
    }

    HeadroomPricingStatus {
        authenticated,
        local_grace_started_at,
        local_grace_ends_at,
        local_grace_active,
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
    }
}

pub fn detect_claude_profile(state: &AppState) -> ClaudeAccountProfile {
    let token = state
        .claude_bearer_token
        .lock()
        .ok()
        .and_then(|t| t.clone());

    let Some(token) = token else {
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
        };
    };

    let profile = fetch_oauth_profile(&token).ok();
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
    }
}

fn fetch_oauth_profile(token: &str) -> Result<ClaudeOauthProfile, String> {
    let response = http_client()?
        .get("https://api.anthropic.com/api/oauth/profile")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .send()
        .map_err(|err| format!("Could not fetch Claude OAuth profile: {err}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Could not fetch Claude OAuth profile (status {}).",
            response.status().as_u16()
        ));
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|err| format!("Could not parse Claude OAuth profile: {err}"))?;

    parse_oauth_profile_value(&body)
        .ok_or_else(|| "Claude OAuth profile response was missing account details.".to_string())
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
        invite_code: value.invite_code,
        accepted_invites_count: value.accepted_invites_count,
        invite_bonus_percent: value.invite_bonus_percent.min(50.0).max(0.0),
    }
}

fn merge_background_account_sync(
    session_token: Option<&str>,
    sync_result: Result<RemoteAccountResponse, RemoteAccountSyncError>,
) -> (bool, Option<HeadroomAccountProfile>) {
    if session_token.is_none() {
        return (false, None);
    }

    match sync_result {
        // Background polling should not silently drop the locally stored session.
        // Explicit auth-required actions still clear the token if the server says it
        // is expired, but passive refreshes keep the user signed in locally.
        Ok(account) => (true, Some(remote_account_to_profile(account))),
        Err(RemoteAccountSyncError::Unauthorized | RemoteAccountSyncError::Other) => (true, None),
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
    };
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
        serde_json::to_vec_pretty(&state)
            .map_err(|err| format!("Failed to serialize pricing state: {err}"))?,
    )
    .map_err(|err| format!("Failed to write pricing state {}: {err}", path.display()))?;
    Ok(state)
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

fn fetch_remote_account(token: &str) -> Result<RemoteAccountResponse, RemoteAccountSyncError> {
    let response = http_client()
        .map_err(|_| RemoteAccountSyncError::Other)?
        .get(api_url("desktop/account"))
        .header("Authorization", format!("Bearer {token}"))
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
        .map(|body| body.account)
        .map_err(|_| RemoteAccountSyncError::Other)
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|err| format!("Could not build HTTP client: {err}"))
}

fn api_url(path: &str) -> String {
    let base = resolve_account_api_base_url(
        std::env::var("HEADROOM_ACCOUNT_API_BASE_URL").ok(),
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
            monthly_price_usd: 5.0,
        }),
        ClaudePlanTier::Max5x => Some(PricingPolicy {
            nudge_threshold_percent: 5.0,
            disable_threshold_percent: 10.0,
            recommended_tier: HeadroomSubscriptionTier::Max5x,
            monthly_price_usd: 25.0,
        }),
        ClaudePlanTier::Max20x => Some(PricingPolicy {
            nudge_threshold_percent: 2.5,
            disable_threshold_percent: 5.0,
            recommended_tier: HeadroomSubscriptionTier::Max20x,
            monthly_price_usd: 50.0,
        }),
        ClaudePlanTier::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{
        merge_background_account_sync, resolve_account_api_base_url, HeadroomSubscriptionTier,
        RemoteAccountResponse, RemoteAccountSyncError, DEFAULT_ACCOUNT_API_BASE_URL,
    };

    fn sample_remote_account() -> RemoteAccountResponse {
        RemoteAccountResponse {
            email: "user@example.com".into(),
            trial_started_at: Some(Utc::now()),
            trial_ends_at: Some(Utc::now()),
            trial_active: true,
            subscription_active: true,
            subscription_tier: Some(HeadroomSubscriptionTier::Pro),
            invite_code: Some("invite-code".into()),
            accepted_invites_count: 2,
            invite_bonus_percent: 10.0,
        }
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
        let (authenticated, account) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Unauthorized),
        );

        assert!(authenticated);
        assert!(account.is_none());
    }

    #[test]
    fn transient_background_sync_error_keeps_local_session_authenticated() {
        let (authenticated, account) = merge_background_account_sync(
            Some("session-token"),
            Err(RemoteAccountSyncError::Other),
        );

        assert!(authenticated);
        assert!(account.is_none());
    }

    #[test]
    fn successful_background_sync_returns_remote_account_profile() {
        let (authenticated, account) =
            merge_background_account_sync(Some("session-token"), Ok(sample_remote_account()));

        assert!(authenticated);
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
}
