//! Provider-agnostic building blocks shared by the Claude, Codex and
//! Codex app-server axes.
//!
//! Scope is deliberately narrow — see `docs/PHASE2_COMMON_LAYER.md`. Only
//! things that are *already* duplicated across two or more providers belong
//! here. Provider-specific concepts (Claude's `hook_*` / `control_request`,
//! Codex approval workflow, per-provider session state machines) stay with
//! their provider.

pub mod events;
