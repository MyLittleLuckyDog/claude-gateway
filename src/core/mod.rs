//! Provider-agnostic building blocks shared by the Claude, Codex and
//! Codex app-server axes.
//!
//! Scope is deliberately narrow — see `docs/PHASE2_COMMON_LAYER.md`. Only
//! things that are *already* duplicated across two or more providers belong
//! here. Provider-specific concepts (Claude's `hook_*` / `control_request`,
//! Codex approval workflow, per-provider session state machines) stay with
//! their provider.

pub mod events;
pub mod session;
pub mod stats;

/// Epoch millis, saturating to 0 if the clock is before the epoch.
///
/// Session activity stamps are compared as plain integers, so every axis must
/// produce them the same way.
pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
