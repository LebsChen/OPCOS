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

const ARCHIVE_TIMEOUT_SECONDS: u64 = 1_800;
const ARCHIVE_TOO_LARGE_EXIT_CODE: i32 = 42;

#[derive(Debug, Error)]
pub enum LoginStateError {
    #[error("login-state operation requires a remote Windows host")]
    UnsupportedHost,
    #[error("browser process is running; stop the browser before backup or restore")]
    BrowserRunning,
    #[error("could not determine whether a browser process is running")]
    BrowserCheckFailed,
    #[error("login-state path is unavailable on the remote host")]
    PathUnavailable,
    #[error("login-state profile is too large for the remote archive format")]
    ProfileTooLarge,
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
            timeout_seconds: ARCHIVE_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .map_err(|_| LoginStateError::HostUnavailable)?;
    if result.result.exit_code == ARCHIVE_TOO_LARGE_EXIT_CODE {
        return Err(LoginStateError::ProfileTooLarge);
    }
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
            timeout_seconds: ARCHIVE_TIMEOUT_SECONDS,
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
    if result.result.exit_code != 0 {
        return Err(LoginStateError::BrowserCheckFailed);
    }
    if !result.result.stdout.trim().is_empty() {
        return Err(LoginStateError::BrowserRunning);
    }
    Ok(())
}

fn powershell_archive_command(profile_path: &str, backup_path: &str) -> String {
    powershell_path_command(
        "$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.IO.Compression; Add-Type -AssemblyName System.IO.Compression.FileSystem; $bytes=(Get-ChildItem -LiteralPath $profilePath -Force -Recurse -File | Measure-Object -Property Length -Sum).Sum; if ($bytes -gt 1900000000) { exit 42 }; New-Item -ItemType Directory -Force -Path (Split-Path -Parent $backupPath) | Out-Null; Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue; $archive=[IO.Compression.ZipFile]::Open($backupPath,[IO.Compression.ZipArchiveMode]::Create); try { $profile=Get-Item -LiteralPath $profilePath -Force; $root=$profile.FullName.TrimEnd('\\')+'\\'; $archive.CreateEntry($profile.Name+'/') | Out-Null; Get-ChildItem -LiteralPath $profilePath -Force -Recurse | ForEach-Object { $relative=$_.FullName.Substring($root.Length).Replace('\\','/'); if ($_.PSIsContainer) { $archive.CreateEntry($profile.Name+'/'+$relative+'/') | Out-Null } else { $entry=$archive.CreateEntry($profile.Name+'/'+$relative); $inputStream=[IO.File]::OpenRead($_.FullName); $outputStream=$entry.Open(); try { $inputStream.CopyTo($outputStream) } finally { $outputStream.Dispose(); $inputStream.Dispose() } } } } finally { $archive.Dispose() }",
        profile_path,
        backup_path,
    )
}

fn powershell_extract_archive_command(backup_path: &str, profile_path: &str) -> String {
    let backup_path = BASE64.encode(backup_path.as_bytes());
    let profile_path = BASE64.encode(profile_path.as_bytes());
    powershell_encoded_command(&format!(
        "$ErrorActionPreference='Stop'; $profilePath=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{profile_path}')); $backupPath=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{backup_path}')); $restorePath=\"$($profilePath).__opcos_restore_$([Guid]::NewGuid().ToString('N'))\"; $previousPath=\"$($profilePath).__opcos_previous_$([Guid]::NewGuid().ToString('N'))\"; try {{ if (Test-Path -LiteralPath $profilePath) {{ Move-Item -LiteralPath $profilePath -Destination $previousPath -ErrorAction Stop }}; Expand-Archive -LiteralPath $backupPath -DestinationPath $restorePath -Force -ErrorAction Stop; $archiveRoot=@(Get-ChildItem -LiteralPath $restorePath -Force); if (($archiveRoot.Count -ne 1) -or -not $archiveRoot[0].PSIsContainer) {{ throw 'archive root is invalid' }}; New-Item -ItemType Directory -Force -Path $profilePath | Out-Null; Get-ChildItem -LiteralPath $archiveRoot[0].FullName -Force | Move-Item -Destination $profilePath -Force -ErrorAction Stop; Remove-Item -LiteralPath $restorePath -Recurse -Force -ErrorAction Stop; if (Test-Path -LiteralPath $previousPath) {{ Remove-Item -LiteralPath $previousPath -Recurse -Force -ErrorAction SilentlyContinue }} }} catch {{ if (Test-Path -LiteralPath $restorePath) {{ Remove-Item -LiteralPath $restorePath -Recurse -Force -ErrorAction SilentlyContinue }}; if (Test-Path -LiteralPath $previousPath) {{ if (Test-Path -LiteralPath $profilePath) {{ Remove-Item -LiteralPath $profilePath -Recurse -Force -ErrorAction SilentlyContinue }}; Move-Item -LiteralPath $previousPath -Destination $profilePath -ErrorAction SilentlyContinue }}; exit 1 }}",
    ))
}

fn powershell_path_command(body: &str, first: &str, second: &str) -> String {
    let first = BASE64.encode(first.as_bytes());
    let second = BASE64.encode(second.as_bytes());
    powershell_encoded_command(&format!(
        "$profilePath=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{first}')); $backupPath=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{second}')); {body}"
    ))
}

fn powershell_encoded_command(script: &str) -> String {
    let encoded = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        BASE64.encode(encoded)
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
        let script = decode_powershell_command(&command);
        assert!(!script.contains(profile));
        assert!(!script.contains(backup));
        assert!(script.contains("ZipFile]::Open"));
        assert!(script.contains("-Force -Recurse -File"));
        assert!(script.contains("Get-ChildItem -LiteralPath $profilePath -Force -Recurse"));
        assert!(script.contains("CreateEntry($profile.Name+'/')"));
        assert!(script.contains("exit 42"));
    }

    #[test]
    fn restore_command_replaces_profile_from_backup_and_can_roll_back() {
        let profile = r"C:\Users\Agent\Profile";
        let backup = r"C:\Users\Agent\OPCOS\backup.zip";
        let command = powershell_extract_archive_command(backup, profile);
        let script = decode_powershell_command(&command);
        assert!(script.contains("Expand-Archive -LiteralPath $backupPath"));
        assert!(script.contains("-DestinationPath $restorePath"));
        assert!(script.contains("Move-Item -LiteralPath $profilePath"));
        assert!(script.contains("$previousPath"));
        assert!(script.contains("@(Get-ChildItem -LiteralPath $restorePath -Force)"));
        assert!(script.contains("Get-ChildItem -LiteralPath $archiveRoot[0].FullName -Force"));
        assert!(script.contains("Move-Item -LiteralPath $previousPath -Destination $profilePath"));
        assert!(!script.contains("Expand-Archive -LiteralPath $profilePath"));
    }

    #[test]
    fn archive_round_trip_strips_only_the_archive_root() {
        let archive_entries = [
            "Profile/Cookies",
            "Profile/Preferences",
            "Profile/.hidden/marker",
            "Profile/EmptyDirectory/",
        ];
        let restored = archive_entries
            .iter()
            .map(|entry| entry.strip_prefix("Profile/").unwrap_or(entry))
            .collect::<Vec<_>>();
        assert_eq!(
            restored,
            [
                "Cookies",
                "Preferences",
                ".hidden/marker",
                "EmptyDirectory/",
            ]
        );
    }

    fn decode_powershell_command(command: &str) -> String {
        let encoded = command
            .split_once("-EncodedCommand ")
            .expect("encoded PowerShell command")
            .1;
        let bytes = BASE64.decode(encoded).expect("valid command encoding");
        String::from_utf16(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
        .expect("valid UTF-16LE command")
    }
}
