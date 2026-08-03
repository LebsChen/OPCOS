use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GitCommandResult {
    pub status: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub cwd: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PushFailureKind {
    NonFastForward,
    PermissionDenied,
    BranchProtected,
    Authentication,
    RemoteUnavailable,
    Other,
}

pub fn classify_push_failure(stderr: &str, _exit_code: i32) -> PushFailureKind {
    let text = stderr.to_ascii_lowercase();
    if text.contains("non-fast-forward")
        || text.contains("non fast forward")
        || text.contains("fetch first")
    {
        PushFailureKind::NonFastForward
    } else if text.contains("protected branch")
        || text.contains("branch protection")
        || text.contains("required status check")
    {
        PushFailureKind::BranchProtected
    } else if text.contains("permission denied")
        || text.contains("write access")
        || text.contains("denied to ")
    {
        PushFailureKind::PermissionDenied
    } else if text.contains("authentication failed")
        || text.contains("could not read username")
        || text.contains("invalid username or token")
    {
        PushFailureKind::Authentication
    } else if text.contains("could not resolve host")
        || text.contains("failed to connect")
        || text.contains("connection timed out")
        || text.contains("network is unreachable")
    {
        PushFailureKind::RemoteUnavailable
    } else {
        PushFailureKind::Other
    }
}

pub fn commit_result(exit_code: i32, stdout: &str, stderr: &str, cwd: &str) -> GitCommandResult {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    let status =
        if combined.contains("nothing to commit") || combined.contains("nothing added to commit") {
            "no_changes"
        } else if exit_code == 0 {
            "committed"
        } else {
            "failed"
        };
    GitCommandResult {
        status: status.into(),
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
        cwd: cwd.into(),
    }
}

pub fn branch_result(exit_code: i32, stdout: &str, stderr: &str, cwd: &str) -> GitCommandResult {
    let status = if exit_code == 0 {
        "created"
    } else if stderr.to_ascii_lowercase().contains("already exists") {
        "already_exists"
    } else {
        "failed"
    };
    GitCommandResult {
        status: status.into(),
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
        cwd: cwd.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_push_failures_for_model_recovery() {
        assert_eq!(
            classify_push_failure("! [rejected] main -> main (non-fast-forward)", 1),
            PushFailureKind::NonFastForward
        );
        assert_eq!(
            classify_push_failure("remote: Permission denied to repository", 1),
            PushFailureKind::PermissionDenied
        );
        assert_eq!(
            classify_push_failure("remote: Protected branch update failed", 1),
            PushFailureKind::BranchProtected
        );
        assert_eq!(
            classify_push_failure("fatal: Authentication failed", 128),
            PushFailureKind::Authentication
        );
        assert_eq!(
            classify_push_failure("Could not resolve host: github.com", 128),
            PushFailureKind::RemoteUnavailable
        );
    }

    #[test]
    fn commit_reports_no_changes_without_claiming_success() {
        let result = commit_result(
            1,
            "On branch feature\nnothing to commit, working tree clean",
            "",
            "/repo",
        );
        assert_eq!(result.status, "no_changes");

        let result = commit_result(0, "", "nothing to commit, working tree clean", "/repo");
        assert_eq!(result.status, "no_changes");
    }

    #[test]
    fn branch_reports_existing_branch_semantics() {
        let result = branch_result(128, "", "fatal: a branch named 'x' already exists", "/repo");
        assert_eq!(result.status, "already_exists");
    }
}
