use crate::options::PermissionMode;

/// Check if a tool action should be auto-approved based on permission mode
pub fn should_auto_approve(mode: &PermissionMode, tool_name: &str) -> bool {
    match mode {
        PermissionMode::BypassPermissions => true,
        PermissionMode::AcceptEdits => {
            matches!(tool_name, "Edit" | "Write" | "NotebookEdit")
        }
        PermissionMode::DontAsk | PermissionMode::Plan => false,
        PermissionMode::Default => false,
    }
}

#[cfg(test)]
mod tests {
    use super::should_auto_approve;
    use crate::options::PermissionMode;

    #[test]
    fn dont_ask_does_not_auto_approve() {
        assert!(!should_auto_approve(&PermissionMode::DontAsk, "Bash"));
    }

    #[test]
    fn accept_edits_only_auto_approves_editing_tools() {
        assert!(should_auto_approve(&PermissionMode::AcceptEdits, "Edit"));
        assert!(should_auto_approve(&PermissionMode::AcceptEdits, "Write"));
        assert!(!should_auto_approve(&PermissionMode::AcceptEdits, "Bash"));
    }
}
