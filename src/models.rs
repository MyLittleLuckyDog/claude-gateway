//! Model definitions, aliases, and API constraints.
//! Based on Claude Code CLI source (configs.ts, context.ts).

use std::collections::HashMap;
use std::sync::LazyLock;

/// Model definition with API constraints
#[derive(Debug, Clone)]
pub struct ModelDef {
    /// Canonical API model ID
    pub id: &'static str,
    /// Short alias for convenience
    pub alias: &'static str,
    /// Default max_tokens if not specified by client
    pub default_max_tokens: u32,
    /// Upper limit for max_tokens
    pub max_output_tokens: u32,
    /// Context window size (input + output)
    pub context_window: u32,
    /// Model family for rate limit grouping
    pub family: ModelFamily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Haiku,
    Sonnet,
    Opus,
}

// ── Model Catalog ──────────────────────────────────────────────────

pub const HAIKU_4_5: ModelDef = ModelDef {
    id: "claude-haiku-4-5-20251001",
    alias: "haiku",
    default_max_tokens: 8_000,
    max_output_tokens: 8_192,
    context_window: 200_000,
    family: ModelFamily::Haiku,
};

pub const SONNET_4: ModelDef = ModelDef {
    id: "claude-sonnet-4-20250514",
    alias: "sonnet4",
    default_max_tokens: 8_000,
    max_output_tokens: 64_000,
    context_window: 200_000,
    family: ModelFamily::Sonnet,
};

pub const SONNET_4_5: ModelDef = ModelDef {
    id: "claude-sonnet-4-5-20250929",
    alias: "sonnet4.5",
    default_max_tokens: 8_000,
    max_output_tokens: 64_000,
    context_window: 200_000,
    family: ModelFamily::Sonnet,
};

pub const SONNET_4_6: ModelDef = ModelDef {
    id: "claude-sonnet-4-6",
    alias: "sonnet",
    default_max_tokens: 8_000,
    max_output_tokens: 64_000,
    context_window: 200_000,
    family: ModelFamily::Sonnet,
};

pub const OPUS_4: ModelDef = ModelDef {
    id: "claude-opus-4-20250514",
    alias: "opus4",
    default_max_tokens: 8_000,
    max_output_tokens: 32_000,
    context_window: 200_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4_5: ModelDef = ModelDef {
    id: "claude-opus-4-5-20251101",
    alias: "opus4.5",
    default_max_tokens: 8_000,
    max_output_tokens: 32_000,
    context_window: 200_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4_6: ModelDef = ModelDef {
    id: "claude-opus-4-6",
    alias: "opus",
    default_max_tokens: 8_000,
    max_output_tokens: 32_000,
    context_window: 200_000,
    family: ModelFamily::Opus,
};

/// All known models
pub const ALL_MODELS: &[&ModelDef] = &[
    &HAIKU_4_5,
    &SONNET_4,
    &SONNET_4_5,
    &SONNET_4_6,
    &OPUS_4,
    &OPUS_4_5,
    &OPUS_4_6,
];

/// Default model when not specified
pub const DEFAULT_MODEL: &ModelDef = &SONNET_4_6;

// ── Context / Token Constants ──────────────────────────────────────

/// Default max_tokens when client doesn't specify (matches CLI)
pub const CAPPED_DEFAULT_MAX_TOKENS: u32 = 8_000;

/// Escalated max_tokens for extended generation
pub const ESCALATED_MAX_TOKENS: u32 = 64_000;

/// Default context window for all current models
pub const MODEL_CONTEXT_WINDOW: u32 = 200_000;

/// Safety buffer: stop accepting messages before hitting context limit
pub const CONTEXT_SAFETY_BUFFER: u32 = 10_000;

/// Threshold for session auto-cleanup (context window - safety buffer)
pub const CONTEXT_CLEANUP_THRESHOLD: u32 = MODEL_CONTEXT_WINDOW - CONTEXT_SAFETY_BUFFER;

// ── API Constants ──────────────────────────────────────────────────

/// Required beta header for OAuth authentication
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// Anthropic API version
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic API base URL
pub const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";

// ── Alias Lookup ───────────────────────────────────────────────────

/// Maps aliases and canonical IDs to ModelDef references.
/// Accepts: "haiku", "sonnet", "opus", full model IDs, partial matches.
static MODEL_LOOKUP: LazyLock<HashMap<String, &'static ModelDef>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for model in ALL_MODELS {
        // Canonical ID
        map.insert(model.id.to_string(), *model);
        // Short alias
        map.insert(model.alias.to_string(), *model);
    }
    // Extra convenience aliases
    map.insert("haiku4.5".to_string(), &HAIKU_4_5);
    map.insert("claude-haiku".to_string(), &HAIKU_4_5);
    map.insert("claude-sonnet".to_string(), &SONNET_4_6);
    map.insert("claude-opus".to_string(), &OPUS_4_6);
    map
});

/// Resolve a model string to a known ModelDef.
/// Returns None for unknown models (allows passthrough for custom/new models).
pub fn resolve_model(input: &str) -> Option<&'static ModelDef> {
    let lower = input.to_lowercase();

    // Exact match first
    if let Some(m) = MODEL_LOOKUP.get(&lower) {
        return Some(m);
    }

    // Prefix match: "claude-sonnet-4-6-20250101" matches SONNET_4_6.
    // Only allow user input that is longer/equal to the model ID — the
    // reverse (short input like "claude" matching the first model) is
    // ambiguous and was removed intentionally.
    ALL_MODELS
        .iter()
        .find(|model| lower.starts_with(model.id))
        .copied()
}

/// Get the canonical model ID for a given input string.
/// If unknown, returns the input as-is (passthrough).
pub fn canonical_model_id(input: &str) -> &str {
    resolve_model(input).map(|m| m.id).unwrap_or(input)
}

/// Get max_tokens default for a model. Falls back to CAPPED_DEFAULT_MAX_TOKENS.
pub fn default_max_tokens(model_id: &str) -> u32 {
    resolve_model(model_id)
        .map(|m| m.default_max_tokens)
        .unwrap_or(CAPPED_DEFAULT_MAX_TOKENS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_aliases() {
        assert_eq!(resolve_model("haiku").unwrap().id, "claude-haiku-4-5-20251001");
        assert_eq!(resolve_model("sonnet").unwrap().id, "claude-sonnet-4-6");
        assert_eq!(resolve_model("opus").unwrap().id, "claude-opus-4-6");
        assert_eq!(resolve_model("claude-opus").unwrap().id, "claude-opus-4-6");
    }

    #[test]
    fn test_resolve_full_id() {
        assert_eq!(resolve_model("claude-sonnet-4-20250514").unwrap().id, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_unknown_model() {
        assert!(resolve_model("gpt-4o").is_none());
    }

    #[test]
    fn test_canonical_passthrough() {
        assert_eq!(canonical_model_id("custom-model-123"), "custom-model-123");
        assert_eq!(canonical_model_id("haiku"), "claude-haiku-4-5-20251001");
    }
}
