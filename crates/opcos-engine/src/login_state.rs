use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use opcos_hosts::{ExecRequest, Host};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginStateBackupEvidence {
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum LoginValidationStatus {
    Valid,
    Invalid,
    Undetermined,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginValidationExpectation {
    pub url: String,
    pub expected_signal: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginValidationObservation {
    pub status: LoginValidationStatus,
    pub signal: Option<String>,
}

#[derive(Debug, Error)]
pub enum LoginStateError {
    #[error("login-state operation requires a remote Windows host")]
    UnsupportedHost,
    #[error("browser process is running; stop the browser before backup or restore")]
    BrowserRunning,
    #[error("login-state path is unavailable on the remote host")]
    PathUnavailable,
    #[error("login-state backup integrity check failed")]
    IntegrityMismatch,
    #[error("login-state validation is undetermined; manual login is required")]
    ValidationUndetermined,
    #[error("login-state host is unavailable")]
    HostUnavailable,
}

pub async fn backup_login_state(
    host: &dyn Host,
    profile_path: &str,
    backup_path: &str,
) -> Result<LoginStateBackupEvidence, LoginStateError> {
    ensure_windows_host(host).await?;
    ensure_browser_stopped(host).await?;
    let profile = host
        .storage_stat(profile_path)
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if !profile.is_dir || profile.is_symlink {
        return Err(LoginStateError::PathUnavailable);
    }
    let command = powershell_archive_command(profile_path, backup_path);
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: 300,
            session: None,
            env: None,
        })
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if result.result.exit_code != 0 {
        return Err(LoginStateError::PathUnavailable);
    }
    let stat = host
        .storage_stat(backup_path)
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    let hash = host
        .storage_hash(backup_path)
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    Ok(LoginStateBackupEvidence {
        hash: hash.hash,
        size: stat.size,
    })
}

pub async fn restore_login_state(
    host: &dyn Host,
    backup_path: &str,
    expected_hash: &str,
    profile_path: &str,
) -> Result<(), LoginStateError> {
    ensure_windows_host(host).await?;
    ensure_browser_stopped(host).await?;
    if !host
        .storage_exists(backup_path)
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?
    {
        return Err(LoginStateError::PathUnavailable);
    }
    let actual_hash = host
        .storage_hash(backup_path)
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if actual_hash.hash != expected_hash {
        return Err(LoginStateError::IntegrityMismatch);
    }
    let command = powershell_extract_archive_command(backup_path, profile_path);
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: 300,
            session: None,
            env: None,
        })
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if result.result.exit_code != 0 {
        return Err(LoginStateError::PathUnavailable);
    }
    Ok(())
}

pub fn classify_login_validation(
    expectation: &LoginValidationExpectation,
    observed_signal: Option<&str>,
) -> LoginValidationObservation {
    let Some(signal) = observed_signal else {
        return LoginValidationObservation {
            status: LoginValidationStatus::Undetermined,
            signal: None,
        };
    };
    LoginValidationObservation {
        status: if signal == expectation.expected_signal {
            LoginValidationStatus::Valid
        } else {
            LoginValidationStatus::Invalid
        },
        signal: Some(signal.to_owned()),
    }
}

async fn ensure_windows_host(host: &dyn Host) -> Result<(), LoginStateError> {
    let health = host
        .health()
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if health.platform.as_deref() != Some("win32") && health.platform.as_deref() != Some("windows")
    {
        return Err(LoginStateError::UnsupportedHost);
    }
    Ok(())
}

async fn ensure_browser_stopped(host: &dyn Host) -> Result<(), LoginStateError> {
    let result = host
        .exec(ExecRequest {
            command: "powershell -NoProfile -NonInteractive -Command \"$p=Get-Process -Name chrome,msedge,firefox -ErrorAction SilentlyContinue; if ($p) { 'running' }\"".into(),
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if result.result.exit_code != 0 || !result.result.stdout.trim().is_empty() {
        return Err(LoginStateError::BrowserRunning);
    }
    Ok(())
}

fn powershell_archive_command(profile_path: &str, backup_path: &str) -> String {
    powershell_path_command(
        "Compress-Archive -Path $profile -DestinationPath $backup -Force",
        profile_path,
        backup_path,
    )
}

fn powershell_extract_archive_command(backup_path: &str, profile_path: &str) -> String {
    powershell_path_command(
        "Expand-Archive -Path $backup -DestinationPath $profile -Force",
        backup_path,
        profile_path,
    )
}

fn powershell_path_command(body: &str, first: &str, second: &str) -> String {
    let first = BASE64.encode(first.as_bytes());
    let second = BASE64.encode(second.as_bytes());
    format!(
        "powershell -NoProfile -NonInteractive -Command \"$profile=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{first}')); $backup=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{second}')); {body}\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_signal_is_undetermined_not_valid() {
        let expectation = LoginValidationExpectation {
            url: "https://example.test".into(),
            expected_signal: "dashboard".into(),
        };
        assert_eq!(
            classify_login_validation(&expectation, None).status,
            LoginValidationStatus::Undetermined
        );
    }

    #[test]
    fn matching_and_nonmatching_signals_are_distinct() {
        let expectation = LoginValidationExpectation {
            url: "https://example.test".into(),
            expected_signal: "dashboard".into(),
        };
        assert_eq!(
            classify_login_validation(&expectation, Some("dashboard")).status,
            LoginValidationStatus::Valid
        );
        assert_eq!(
            classify_login_validation(&expectation, Some("login")).status,
            LoginValidationStatus::Invalid
        );
    }

    #[test]
    fn archive_commands_encode_remote_paths() {
        let profile = r"C:\Users\Agent\Profile With Spaces";
        let backup = r"C:\Users\Agent\OPCOS\backup.zip";
        let command = powershell_archive_command(profile, backup);
        assert!(!command.contains(profile));
        assert!(!command.contains(backup));
    }
}
