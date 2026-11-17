use std::sync::atomic::{AtomicBool, Ordering};

/// The user's "Usage analytics and crash reports" setting (Settings), mirrored
/// from the setup state so Sentry's before_send and every sender read it
/// without touching disk. Set at launch before Sentry starts, and on toggle.
static SHARING_DISABLED: AtomicBool = AtomicBool::new(false);

/// False once the user turned usage data off: no Sentry events, no funnel
/// steps, and no savings snapshot or analytics-only fields on account calls.
/// Licensing calls are unaffected.
pub fn sharing_enabled() -> bool {
    !SHARING_DISABLED.load(Ordering::Relaxed)
}

pub fn set_sharing_enabled(enabled: bool) {
    SHARING_DISABLED.store(!enabled, Ordering::Relaxed);
}
