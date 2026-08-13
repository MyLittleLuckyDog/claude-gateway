use crate::messages::cli_control::HookCallbackInput;
use crate::options::{ClaudeAgentOptions, HookAction};

/// Decision produced by evaluating server-side rules against a hook callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedDecision {
    Approve,
    Block {
        reason: Option<String>,
    },
    /// No rule fully resolved this callback — let the client decide.
    Defer,
}

/// Evaluate server-side hook rules against a `hook_callback` payload.
///
/// Priority (same as before): `block` > `approve` > `defer`.
/// Returns `None` when no rule matches at all *or* the resolved outcome is
/// defer — both of those hand control to the streaming client.
pub fn evaluate_hook_rules(
    options: &ClaudeAgentOptions,
    input: &HookCallbackInput,
) -> Option<ResolvedDecision> {
    let rules = options.hook_rules.as_ref()?;
    let mut result: Option<ResolvedDecision> = None;

    for rule in rules {
        if rule.event != input.hook_event_name {
            continue;
        }
        if let Some(pattern) = &rule.tool_pattern {
            let tool_name = input.tool_name.as_deref().unwrap_or("");
            if !matches_tool_pattern(pattern, tool_name) {
                continue;
            }
        }

        match &rule.action {
            HookAction::Block { reason } => {
                return Some(ResolvedDecision::Block {
                    reason: reason.clone(),
                });
            }
            HookAction::Approve => {
                if !matches!(result, Some(ResolvedDecision::Block { .. })) {
                    result = Some(ResolvedDecision::Approve);
                }
            }
            HookAction::Defer => {
                if result.is_none() {
                    result = Some(ResolvedDecision::Defer);
                }
            }
        }
    }

    match result {
        Some(ResolvedDecision::Defer) => None,
        other => other,
    }
}

fn matches_tool_pattern(pattern: &str, tool_name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    pattern.split('|').any(|p| p.trim() == tool_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{ClaudeAgentOptions, HookAction, HookRule};

    fn input(event: &str, tool: &str) -> HookCallbackInput {
        HookCallbackInput {
            hook_event_name: event.to_string(),
            tool_name: Some(tool.to_string()),
            tool_input: None,
        }
    }

    #[test]
    fn block_wins_over_approve_regardless_of_order() {
        let opts = ClaudeAgentOptions {
            hook_rules: Some(vec![
                HookRule {
                    event: "PreToolUse".into(),
                    tool_pattern: Some("Bash".into()),
                    action: HookAction::Approve,
                },
                HookRule {
                    event: "PreToolUse".into(),
                    tool_pattern: Some("Bash".into()),
                    action: HookAction::Block {
                        reason: Some("nope".into()),
                    },
                },
            ]),
            ..Default::default()
        };
        let r = evaluate_hook_rules(&opts, &input("PreToolUse", "Bash")).unwrap();
        assert!(matches!(r, ResolvedDecision::Block { .. }));
    }

    #[test]
    fn wildcard_pattern_matches_anything() {
        let opts = ClaudeAgentOptions {
            hook_rules: Some(vec![HookRule {
                event: "PreToolUse".into(),
                tool_pattern: Some("*".into()),
                action: HookAction::Approve,
            }]),
            ..Default::default()
        };
        let r = evaluate_hook_rules(&opts, &input("PreToolUse", "Read")).unwrap();
        assert_eq!(r, ResolvedDecision::Approve);
    }

    #[test]
    fn defer_returns_none_so_client_is_asked() {
        let opts = ClaudeAgentOptions {
            hook_rules: Some(vec![HookRule {
                event: "PreToolUse".into(),
                tool_pattern: Some("Bash".into()),
                action: HookAction::Defer,
            }]),
            ..Default::default()
        };
        assert!(evaluate_hook_rules(&opts, &input("PreToolUse", "Bash")).is_none());
    }
}
