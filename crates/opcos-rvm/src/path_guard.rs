use std::fmt;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathGuardError {
    #[error("path is empty")]
    Empty,
    #[error("path contains NUL")]
    Nul,
    #[error("path traversal is not allowed")]
    Traversal,
    #[error("absolute repository paths are not allowed")]
    AbsoluteRepositoryPath,
    #[error("path is outside the configured remote workspace")]
    OutsideWorkspace,
}

#[derive(Clone, Debug)]
pub struct RemotePathGuard {
    workspace: String,
    windows: bool,
}

impl RemotePathGuard {
    pub fn new(workspace: impl Into<String>) -> Self {
        let workspace = workspace.into();
        let windows = workspace.as_bytes().get(1) == Some(&b':') || workspace.contains('\\');
        Self { workspace, windows }
    }

    pub fn path(&self, value: &str) -> Result<String, PathGuardError> {
        let decoded = decode_repeated(value)?;
        validate_components(&decoded)?;
        let normalized = normalize(&decoded);
        let root = normalize(&self.workspace);
        let comparable_path = if self.windows {
            normalized.to_ascii_lowercase()
        } else {
            normalized.clone()
        };
        let comparable_root = if self.windows {
            root.to_ascii_lowercase()
        } else {
            root
        };
        if comparable_path != comparable_root
            && !comparable_path.starts_with(&(comparable_root + "/"))
        {
            return Err(PathGuardError::OutsideWorkspace);
        }
        Ok(normalized)
    }

    pub fn repository_path(&self, value: &str) -> Result<String, PathGuardError> {
        let decoded = decode_repeated(value)?;
        validate_components(&decoded)?;
        if is_absolute(&decoded) {
            return Err(PathGuardError::AbsoluteRepositoryPath);
        }
        Ok(normalize(&decoded))
    }
}

impl fmt::Display for RemotePathGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.workspace)
    }
}

fn decode_repeated(value: &str) -> Result<String, PathGuardError> {
    if value.is_empty() {
        return Err(PathGuardError::Empty);
    }
    if value.contains('\0') {
        return Err(PathGuardError::Nul);
    }
    let mut current = value.to_owned();
    for _ in 0..8 {
        let decoded = percent_decode(&current)?;
        if decoded == current {
            return Ok(decoded);
        }
        current = decoded;
    }
    Ok(current)
}

fn percent_decode(value: &str) -> Result<String, PathGuardError> {
    let mut bytes = Vec::with_capacity(value.len());
    let raw = value.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            if index + 2 >= raw.len() {
                return Err(PathGuardError::Traversal);
            }
            let high = hex(raw[index + 1]).ok_or(PathGuardError::Traversal)?;
            let low = hex(raw[index + 2]).ok_or(PathGuardError::Traversal)?;
            bytes.push(high * 16 + low);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| PathGuardError::Traversal)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn validate_components(value: &str) -> Result<(), PathGuardError> {
    if value.replace('\\', "/").split('/').any(|part| part == "..") {
        Err(PathGuardError::Traversal)
    } else {
        Ok(())
    }
}

fn is_absolute(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|byte| *byte == b'/' || *byte == b'\\'))
}

fn normalize(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_posix_and_windows_paths() {
        let posix = RemotePathGuard::new("/workspace");
        assert!(posix.path("/workspace/src/main.rs").is_ok());
        assert_eq!(
            posix.path("/workspace/%252e%252e/etc"),
            Err(PathGuardError::Traversal)
        );
        let windows = RemotePathGuard::new(r"C:\Users\Team");
        assert!(windows.path(r"C:\Users\Team\repo").is_ok());
        assert_eq!(
            windows.path(r"C:\Windows"),
            Err(PathGuardError::OutsideWorkspace)
        );
    }

    #[test]
    fn repository_paths_are_relative() {
        let guard = RemotePathGuard::new("/workspace");
        assert!(guard.repository_path("src/lib.rs").is_ok());
        assert_eq!(
            guard.repository_path("/etc/passwd"),
            Err(PathGuardError::AbsoluteRepositoryPath)
        );
    }
}
