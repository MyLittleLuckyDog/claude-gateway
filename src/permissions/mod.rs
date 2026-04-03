use crate::options::PermissionMode;

/// Check if a tool action should be auto-approved based on permission mode
pub fn should_auto_approve(mode: &PermissionMode, tool_name: &str) -> bool {
    match mode {
        PermissionMode::BypassPermissions | PermissionMode::DontAsk => true,
        PermissionMode::AcceptEdits => {
            matches!(tool_name, "Edit" | "Write" | "NotebookEdit")
        }
        PermissionMode::Plan => false, // plan mode doesn't execute tools
        PermissionMode::Default => false,
    }
}
