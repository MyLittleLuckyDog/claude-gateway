//! What a request may decide about how the CLI runs.
//!
//! Session options travel from the request body to the subprocess almost
//! untouched. On a loopback gateway that is a convenience: the caller already
//! has the privileges it is asking for. The moment the gateway is reachable
//! from a browser or another host it stops being one — `cli_path` chooses the
//! binary, `env` its environment, `mcp_servers` spawns a second one, and
//! `permission_mode` decides whether anyone is asked first.
//!
//! [`RequestPolicy::Restricted`] keeps those decisions on the server. It
//! rejects rather than silently strips: a caller that asked for
//! `bypassPermissions` and got `plan` would otherwise believe its tools were
//! approved.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::codex::options::{CodexOptions, CodexSandboxMode};
use crate::options::{ClaudeAgentOptions, PermissionMode};

/// How much of the option surface a request may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Every option, as sent. Correct for a loopback gateway, where the caller
    /// is the local user and gains nothing it did not already have.
    #[default]
    Trusted,
    /// The server owns process, filesystem and approval settings.
    Restricted,
}

/// Ceiling on how much the CLI may do without asking.
///
/// Ordered least to most privileged, so a configured ceiling admits everything
/// below it.
fn permission_rank(mode: &PermissionMode) -> u8 {
    match mode {
        PermissionMode::Plan => 0,
        PermissionMode::Default => 1,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::DontAsk => 3,
        PermissionMode::BypassPermissions => 4,
    }
}

fn sandbox_rank(mode: &CodexSandboxMode) -> u8 {
    match mode {
        CodexSandboxMode::ReadOnly => 0,
        CodexSandboxMode::WorkspaceWrite => 1,
        CodexSandboxMode::DangerFullAccess => 2,
    }
}

#[derive(Debug, Clone)]
pub struct RequestPolicy {
    pub mode: PolicyMode,
    /// Directories a request may point `cwd` or `add_dirs` at. Empty means the
    /// request may not choose a directory at all.
    pub allowed_roots: Vec<PathBuf>,
    pub max_permission_mode: PermissionMode,
    pub max_codex_sandbox: CodexSandboxMode,
}

/// Every way a request overstepped, so the caller can fix them in one round
/// trip instead of discovering them one at a time.
#[derive(Debug)]
pub struct PolicyRejection(pub Vec<String>);

impl std::fmt::Display for PolicyRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.join("; "))
    }
}

impl RequestPolicy {
    fn server_owned(&self, out: &mut Vec<String>, field: &str, set: bool) {
        if set {
            out.push(format!(
                "`options.{field}` is set by the server and cannot be requested"
            ));
        }
    }

    /// Resolve `path` and confirm it sits inside a configured root.
    ///
    /// Both sides are canonicalised, so a symlink out of an allowed root is
    /// caught. A path that does not exist is refused rather than assumed
    /// harmless — it cannot be checked, and the CLI could not use it anyway.
    fn check_dir(&self, out: &mut Vec<String>, field: &str, path: &Path) {
        if self.allowed_roots.is_empty() {
            out.push(format!(
                "`options.{field}` is not allowed: no directories are configured for requests"
            ));
            return;
        }
        let Ok(resolved) = std::fs::canonicalize(path) else {
            out.push(format!(
                "`options.{field}` ({}) does not resolve to an existing directory",
                path.display()
            ));
            return;
        };
        let inside = self
            .allowed_roots
            .iter()
            .any(|root| std::fs::canonicalize(root).is_ok_and(|root| resolved.starts_with(&root)));
        if !inside {
            out.push(format!(
                "`options.{field}` ({}) is outside every directory configured for requests",
                path.display()
            ));
        }
    }

    pub fn check_claude(&self, o: &ClaudeAgentOptions) -> Result<(), PolicyRejection> {
        if self.mode == PolicyMode::Trusted {
            return Ok(());
        }
        let mut bad = Vec::new();

        // Each of these is on its own enough to run an arbitrary program.
        self.server_owned(&mut bad, "cli_path", o.cli_path.is_some());
        self.server_owned(&mut bad, "env", o.env.is_some());
        self.server_owned(&mut bad, "mcp_servers", o.mcp_servers.is_some());
        // Loads the host's own settings and hooks into the session.
        self.server_owned(&mut bad, "setting_sources", o.setting_sources.is_some());
        // Pre-approves tools, which is a permission bypass by another name.
        self.server_owned(&mut bad, "allowed_tools", o.allowed_tools.is_some());
        // Attach to a CLI conversation this caller did not start.
        self.server_owned(&mut bad, "resume", o.resume.is_some());
        self.server_owned(&mut bad, "fork_session", o.fork_session.is_some());
        self.server_owned(&mut bad, "continue_conversation", o.continue_conversation);

        if let Some(cwd) = &o.cwd {
            self.check_dir(&mut bad, "cwd", cwd);
        }
        for dir in o.add_dirs.iter().flatten() {
            self.check_dir(&mut bad, "add_dirs", dir);
        }

        if let Some(mode) = &o.permission_mode {
            if permission_rank(mode) > permission_rank(&self.max_permission_mode) {
                bad.push(format!(
                    "`options.permission_mode` ({}) exceeds the configured maximum ({})",
                    mode.as_str(),
                    self.max_permission_mode.as_str()
                ));
            }
        }

        if bad.is_empty() {
            Ok(())
        } else {
            Err(PolicyRejection(bad))
        }
    }

    pub fn check_codex(&self, o: &CodexOptions) -> Result<(), PolicyRejection> {
        if self.mode == PolicyMode::Trusted {
            return Ok(());
        }
        let mut bad = Vec::new();

        self.server_owned(&mut bad, "cli_path", o.cli_path.is_some());
        self.server_owned(&mut bad, "env", o.env.is_some());
        self.server_owned(
            &mut bad,
            "dangerously_bypass_approvals_and_sandbox",
            o.dangerously_bypass_approvals_and_sandbox,
        );
        self.server_owned(&mut bad, "full_auto", o.full_auto);

        if let Some(cwd) = &o.cwd {
            self.check_dir(&mut bad, "cwd", cwd);
        }
        for dir in o.add_dirs.iter().flatten() {
            self.check_dir(&mut bad, "add_dirs", dir);
        }

        if let Some(sandbox) = &o.sandbox {
            if sandbox_rank(sandbox) > sandbox_rank(&self.max_codex_sandbox) {
                bad.push(format!(
                    "`options.sandbox` ({}) exceeds the configured maximum ({})",
                    sandbox.as_str(),
                    self.max_codex_sandbox.as_str()
                ));
            }
        }

        // `approval_policy` is deliberately not capped: the exec transport
        // requires `never` (see codex::validate_automation_policy) and the
        // app-server transport needs `on-request` to surface approvals at all.
        // The sandbox ceiling is what bounds the damage.

        if bad.is_empty() {
            Ok(())
        } else {
            Err(PolicyRejection(bad))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::options::CodexApprovalPolicy;

    fn restricted(roots: Vec<PathBuf>) -> RequestPolicy {
        RequestPolicy {
            mode: PolicyMode::Restricted,
            allowed_roots: roots,
            max_permission_mode: PermissionMode::Plan,
            max_codex_sandbox: CodexSandboxMode::ReadOnly,
        }
    }

    fn trusted() -> RequestPolicy {
        RequestPolicy {
            mode: PolicyMode::Trusted,
            allowed_roots: Vec::new(),
            max_permission_mode: PermissionMode::Plan,
            max_codex_sandbox: CodexSandboxMode::ReadOnly,
        }
    }

    #[test]
    fn trusted_mode_accepts_everything() {
        let o = ClaudeAgentOptions {
            cli_path: Some("/bin/sh".into()),
            env: Some(Default::default()),
            permission_mode: Some(PermissionMode::BypassPermissions),
            cwd: Some("/".into()),
            ..Default::default()
        };
        assert!(trusted().check_claude(&o).is_ok());
    }

    #[test]
    fn a_plain_prompt_request_passes_under_restriction() {
        let o = ClaudeAgentOptions {
            system_prompt: Some("be brief".to_string()),
            model: Some("sonnet".to_string()),
            max_turns: Some(2),
            ..Default::default()
        };
        assert!(restricted(vec![]).check_claude(&o).is_ok());
    }

    /// The three fields that each amount to running a program of the caller's
    /// choosing.
    #[test]
    fn process_control_fields_are_refused() {
        for (label, o) in [
            (
                "cli_path",
                ClaudeAgentOptions {
                    cli_path: Some("/bin/sh".into()),
                    ..Default::default()
                },
            ),
            (
                "env",
                ClaudeAgentOptions {
                    env: Some(Default::default()),
                    ..Default::default()
                },
            ),
            (
                "mcp_servers",
                ClaudeAgentOptions {
                    mcp_servers: Some(Default::default()),
                    ..Default::default()
                },
            ),
        ] {
            let err = restricted(vec![]).check_claude(&o).unwrap_err();
            assert!(err.to_string().contains(label), "{label}: {err}");
        }
    }

    #[test]
    fn permission_mode_is_capped() {
        let o = ClaudeAgentOptions {
            permission_mode: Some(PermissionMode::BypassPermissions),
            ..Default::default()
        };
        let err = restricted(vec![]).check_claude(&o).unwrap_err();
        assert!(err.to_string().contains("permission_mode"), "{err}");
    }

    #[test]
    fn a_mode_at_or_below_the_ceiling_passes() {
        let mut policy = restricted(vec![]);
        policy.max_permission_mode = PermissionMode::AcceptEdits;

        for mode in [
            PermissionMode::Plan,
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
        ] {
            let o = ClaudeAgentOptions {
                permission_mode: Some(mode.clone()),
                ..Default::default()
            };
            assert!(policy.check_claude(&o).is_ok(), "{}", mode.as_str());
        }
    }

    #[test]
    fn every_violation_is_reported_at_once() {
        let o = ClaudeAgentOptions {
            cli_path: Some("/bin/sh".into()),
            env: Some(Default::default()),
            permission_mode: Some(PermissionMode::BypassPermissions),
            ..Default::default()
        };
        let err = restricted(vec![]).check_claude(&o).unwrap_err();
        assert_eq!(err.0.len(), 3, "{err}");
    }

    #[test]
    fn a_directory_inside_an_allowed_root_passes() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("work");
        std::fs::create_dir(&inner).unwrap();

        let o = ClaudeAgentOptions {
            cwd: Some(inner),
            ..Default::default()
        };
        assert!(restricted(vec![root.path().to_path_buf()])
            .check_claude(&o)
            .is_ok());
    }

    #[test]
    fn a_directory_outside_every_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();

        let o = ClaudeAgentOptions {
            cwd: Some(other.path().to_path_buf()),
            ..Default::default()
        };
        let err = restricted(vec![root.path().to_path_buf()])
            .check_claude(&o)
            .unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
    }

    /// `..` must not walk out of an allowed root.
    #[test]
    fn a_traversal_out_of_a_root_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("allowed");
        let sibling = base.path().join("secret");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&sibling).unwrap();

        let o = ClaudeAgentOptions {
            cwd: Some(root.join("..").join("secret")),
            ..Default::default()
        };
        let err = restricted(vec![root]).check_claude(&o).unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
    }

    /// A symlink is resolved before the check, so it cannot bridge out.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_root_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("allowed");
        let secret = base.path().join("secret");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&secret).unwrap();
        std::os::unix::fs::symlink(&secret, root.join("escape")).unwrap();

        let o = ClaudeAgentOptions {
            cwd: Some(root.join("escape")),
            ..Default::default()
        };
        let err = restricted(vec![root]).check_claude(&o).unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[test]
    fn no_configured_roots_means_no_directory_may_be_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let o = ClaudeAgentOptions {
            cwd: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let err = restricted(vec![]).check_claude(&o).unwrap_err();
        assert!(err.to_string().contains("no directories"), "{err}");
    }

    #[test]
    fn codex_process_and_bypass_fields_are_refused() {
        let o = CodexOptions {
            cli_path: Some("/bin/sh".into()),
            dangerously_bypass_approvals_and_sandbox: true,
            full_auto: true,
            ..Default::default()
        };
        let err = restricted(vec![]).check_codex(&o).unwrap_err();
        assert_eq!(err.0.len(), 3, "{err}");
    }

    #[test]
    fn codex_sandbox_is_capped() {
        let o = CodexOptions {
            sandbox: Some(CodexSandboxMode::DangerFullAccess),
            ..Default::default()
        };
        let err = restricted(vec![]).check_codex(&o).unwrap_err();
        assert!(err.to_string().contains("sandbox"), "{err}");
    }

    /// The exec transport refuses anything but `never`, so capping this would
    /// break the endpoint rather than protect it.
    #[test]
    fn codex_approval_policy_is_left_alone() {
        let o = CodexOptions {
            approval_policy: Some(CodexApprovalPolicy::Never),
            ..Default::default()
        };
        assert!(restricted(vec![]).check_codex(&o).is_ok());
    }
}
