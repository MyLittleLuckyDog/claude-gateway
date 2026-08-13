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
    Fable,
}

// ── Model Catalog ──────────────────────────────────────────────────
//
// Current-generation models (Claude 5 family, Opus 4.6+, Sonnet 4.6) carry a
// 1M context window and a 128K output ceiling. Reaching 128K output requires
// streaming — a non-streaming request that large will hit the HTTP timeout.
//
// Legacy entries below keep the context/output figures they were introduced
// with. Verify against `GET /v1/models/{id}` (`max_input_tokens`, `max_tokens`)
// before relying on them for anything load-bearing.

pub const FABLE_5: ModelDef = ModelDef {
    id: "claude-fable-5",
    alias: "fable",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Fable,
};

pub const OPUS_5: ModelDef = ModelDef {
    id: "claude-opus-5",
    alias: "opus",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4_8: ModelDef = ModelDef {
    id: "claude-opus-4-8",
    alias: "opus4.8",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4_7: ModelDef = ModelDef {
    id: "claude-opus-4-7",
    alias: "opus4.7",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4_6: ModelDef = ModelDef {
    id: "claude-opus-4-6",
    alias: "opus4.6",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4_5: ModelDef = ModelDef {
    id: "claude-opus-4-5",
    alias: "opus4.5",
    default_max_tokens: 8_000,
    max_output_tokens: 32_000,
    context_window: 200_000,
    family: ModelFamily::Opus,
};

pub const OPUS_4: ModelDef = ModelDef {
    id: "claude-opus-4-0",
    alias: "opus4",
    default_max_tokens: 8_000,
    max_output_tokens: 32_000,
    context_window: 200_000,
    family: ModelFamily::Opus,
};

pub const SONNET_5: ModelDef = ModelDef {
    id: "claude-sonnet-5",
    alias: "sonnet",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Sonnet,
};

pub const SONNET_4_6: ModelDef = ModelDef {
    id: "claude-sonnet-4-6",
    alias: "sonnet4.6",
    default_max_tokens: 8_000,
    max_output_tokens: 128_000,
    context_window: 1_000_000,
    family: ModelFamily::Sonnet,
};

pub const SONNET_4_5: ModelDef = ModelDef {
    id: "claude-sonnet-4-5",
    alias: "sonnet4.5",
    default_max_tokens: 8_000,
    max_output_tokens: 64_000,
    context_window: 200_000,
    family: ModelFamily::Sonnet,
};

pub const SONNET_4: ModelDef = ModelDef {
    id: "claude-sonnet-4-0",
    alias: "sonnet4",
    default_max_tokens: 8_000,
    max_output_tokens: 64_000,
    context_window: 200_000,
    family: ModelFamily::Sonnet,
};

pub const HAIKU_4_5: ModelDef = ModelDef {
    id: "claude-haiku-4-5",
    alias: "haiku",
    default_max_tokens: 8_000,
    max_output_tokens: 64_000,
    context_window: 200_000,
    family: ModelFamily::Haiku,
};

/// All known models.
///
/// Newest first: `resolve_model` falls back to a prefix scan in this order, so
/// a dated ID resolves to the newest entry it actually extends.
pub const ALL_MODELS: &[&ModelDef] = &[
    &FABLE_5,
    &OPUS_5,
    &OPUS_4_8,
    &OPUS_4_7,
    &OPUS_4_6,
    &OPUS_4_5,
    &OPUS_4,
    &SONNET_5,
    &SONNET_4_6,
    &SONNET_4_5,
    &SONNET_4,
    &HAIKU_4_5,
];

/// Default model when not specified
pub const DEFAULT_MODEL: &ModelDef = &SONNET_5;

// ── Context / Token Constants ──────────────────────────────────────

/// Default max_tokens when client doesn't specify (matches CLI)
pub const CAPPED_DEFAULT_MAX_TOKENS: u32 = 8_000;

/// Escalated max_tokens for extended generation
pub const ESCALATED_MAX_TOKENS: u32 = 64_000;

/// Context window assumed for a model that is not in the catalog.
/// Known models carry their own `context_window` — prefer that.
pub const MODEL_CONTEXT_WINDOW: u32 = 200_000;

/// Safety buffer: stop accepting messages before hitting context limit
pub const CONTEXT_SAFETY_BUFFER: u32 = 10_000;

/// Cleanup threshold for a model that is not in the catalog.
/// For known models use [`context_cleanup_threshold`], which scales with the
/// model's own window — a fixed 190K threshold would fire at 19% usage on the
/// 1M-context models.
pub const CONTEXT_CLEANUP_THRESHOLD: u32 = MODEL_CONTEXT_WINDOW - CONTEXT_SAFETY_BUFFER;

/// Context window for `model`, falling back to [`MODEL_CONTEXT_WINDOW`] when
/// the model is unknown (custom or newer than this catalog).
pub fn context_window(model_id: &str) -> u32 {
    resolve_model(model_id)
        .map(|m| m.context_window)
        .unwrap_or(MODEL_CONTEXT_WINDOW)
}

/// Token count at which a session for `model` should be recycled.
pub fn context_cleanup_threshold(model_id: &str) -> u32 {
    context_window(model_id).saturating_sub(CONTEXT_SAFETY_BUFFER)
}

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
    map.insert("fable5".to_string(), &FABLE_5);
    map.insert("claude-fable".to_string(), &FABLE_5);
    map.insert("opus5".to_string(), &OPUS_5);
    map.insert("claude-opus".to_string(), &OPUS_5);
    map.insert("sonnet5".to_string(), &SONNET_5);
    map.insert("claude-sonnet".to_string(), &SONNET_5);
    map.insert("haiku4.5".to_string(), &HAIKU_4_5);
    map.insert("claude-haiku".to_string(), &HAIKU_4_5);
    // Dated IDs that predate the un-suffixed aliases.
    // claude-opus-4-1 is deliberately absent: it is a distinct model, so it
    // passes through untouched rather than being rewritten to Opus 4.0.
    map.insert("claude-haiku-4-5-20251001".to_string(), &HAIKU_4_5);
    map.insert("claude-opus-4-20250514".to_string(), &OPUS_4);
    map.insert("claude-opus-4-5-20251101".to_string(), &OPUS_4_5);
    map.insert("claude-sonnet-4-20250514".to_string(), &SONNET_4);
    map.insert("claude-sonnet-4-5-20250929".to_string(), &SONNET_4_5);
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

    // Prefix match: "claude-sonnet-5-20260101" matches SONNET_5.
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
        assert_eq!(resolve_model("haiku").unwrap().id, "claude-haiku-4-5");
        assert_eq!(resolve_model("sonnet").unwrap().id, "claude-sonnet-5");
        assert_eq!(resolve_model("opus").unwrap().id, "claude-opus-5");
        assert_eq!(resolve_model("fable").unwrap().id, "claude-fable-5");
        assert_eq!(resolve_model("claude-opus").unwrap().id, "claude-opus-5");
    }

    #[test]
    fn test_resolve_versioned_aliases() {
        assert_eq!(resolve_model("opus4.8").unwrap().id, "claude-opus-4-8");
        assert_eq!(resolve_model("opus4.6").unwrap().id, "claude-opus-4-6");
        assert_eq!(resolve_model("sonnet4.6").unwrap().id, "claude-sonnet-4-6");
    }

    #[test]
    fn test_resolve_full_id() {
        assert_eq!(
            resolve_model("claude-sonnet-4-20250514").unwrap().id,
            "claude-sonnet-4-0"
        );
        assert_eq!(
            resolve_model("claude-haiku-4-5-20251001").unwrap().id,
            "claude-haiku-4-5"
        );
    }

    /// A dated suffix must resolve to the entry it extends, not to an older
    /// model whose ID happens to be a prefix of it.
    #[test]
    fn test_dated_suffix_prefers_exact_generation() {
        assert_eq!(
            resolve_model("claude-sonnet-5-20260401").unwrap().id,
            "claude-sonnet-5"
        );
        assert_eq!(
            resolve_model("claude-opus-4-6-20260101").unwrap().id,
            "claude-opus-4-6"
        );
    }

    /// Opus 4.1 is its own model — it must not be canonicalised to Opus 4.0.
    #[test]
    fn test_opus_4_1_passes_through() {
        assert!(resolve_model("claude-opus-4-1").is_none());
        assert_eq!(canonical_model_id("claude-opus-4-1"), "claude-opus-4-1");
    }

    #[test]
    fn test_unknown_model() {
        assert!(resolve_model("gpt-4o").is_none());
    }

    #[test]
    fn test_canonical_passthrough() {
        assert_eq!(canonical_model_id("custom-model-123"), "custom-model-123");
        assert_eq!(canonical_model_id("haiku"), "claude-haiku-4-5");
    }

    #[test]
    fn test_current_models_have_long_context() {
        for m in [&FABLE_5, &OPUS_5, &SONNET_5, &OPUS_4_6, &SONNET_4_6] {
            assert_eq!(m.context_window, 1_000_000, "{}", m.id);
            assert_eq!(m.max_output_tokens, 128_000, "{}", m.id);
        }
    }

    #[test]
    fn test_cleanup_threshold_scales_with_model() {
        assert_eq!(context_cleanup_threshold("claude-opus-5"), 990_000);
        assert_eq!(context_cleanup_threshold("claude-haiku-4-5"), 190_000);
        // Unknown model falls back to the conservative fixed threshold.
        assert_eq!(
            context_cleanup_threshold("gpt-4o"),
            CONTEXT_CLEANUP_THRESHOLD
        );
    }

    /// Every alias and ID must be unique, or the lookup silently shadows one.
    #[test]
    fn test_no_duplicate_ids_or_aliases() {
        let mut seen = std::collections::HashSet::new();
        for m in ALL_MODELS {
            assert!(seen.insert(m.id), "duplicate id: {}", m.id);
            assert!(seen.insert(m.alias), "duplicate alias: {}", m.alias);
        }
    }
}
