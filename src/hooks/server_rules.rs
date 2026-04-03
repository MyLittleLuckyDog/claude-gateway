use crate::messages::cli_input::HookDecision;
use crate::messages::cli_output::CliHookRequestEvent;
use crate::options::{ClaudeAgentOptions, HookAction};

/// Evaluate server-side hook rules. Returns Some(decision) if matched, None to defer to client.
/// Priority: block > approve > defer
pub fn evaluate_hook_rules(
    options: &ClaudeAgentOptions,
    hook: &CliHookRequestEvent,
) -> Option<HookDecision> {
    let rules = match &options.hook_rules {
        Some(rules) => rules,
        None => return None,
    };

    let mut result: Option<HookDecision> = None;

    for rule in rules {
        // Check event name match
        if rule.event != hook.hook_event_name {
            continue;
        }

        // Check tool pattern match
        if let Some(pattern) = &rule.tool_pattern {
            let tool_name = hook.tool_name.as_deref().unwrap_or("");
            if !matches_tool_pattern(pattern, tool_name) {
                continue;
            }
        }

        // Apply priority: block > approve > defer
        match &rule.action {
            HookAction::Block { .. } => {
                // Block always wins
                return Some(HookDecision::Block);
            }
            HookAction::Approve => {
                if !matches!(result, Some(HookDecision::Block)) {
                    result = Some(HookDecision::Approve);
                }
            }
            HookAction::Defer => {
                if result.is_none() {
                    result = Some(HookDecision::Defer);
                }
            }
        }
    }

    // If defer, return None to let client handle
    match result {
        Some(HookDecision::Defer) => None,
        other => other,
    }
}

fn matches_tool_pattern(pattern: &str, tool_name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Support pipe-separated patterns: "Edit|Write"
    pattern.split('|').any(|p| p.trim() == tool_name)
}
