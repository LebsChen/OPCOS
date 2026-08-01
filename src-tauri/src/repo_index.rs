use chrono::{DateTime, Utc};
use opcos_hosts::{ExecRequest, Host};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MAX_FILES: usize = 20_000;
const MAX_FILE_BYTES: i64 = 10 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RepoIndex {
    pub host_id: String,
    pub workspace: String,
    pub built_at: DateTime<Utc>,
    pub status: String,
    pub files: Vec<IndexFile>,
    pub symbols: Vec<IndexSymbol>,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexFile {
    pub path: String,
    pub size_bytes: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexSymbol {
    pub path: String,
    pub line: u32,
    pub text: String,
}

pub fn index_path(root: &Path, host_id: &str, workspace: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(host_id.as_bytes());
    digest.update([0]);
    digest.update(workspace.as_bytes());
    root.join(format!("{:x}.json", digest.finalize()))
}

pub fn load(root: &Path, host_id: &str, workspace: &str) -> Result<Option<RepoIndex>, String> {
    let path = index_path(root, host_id, workspace);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|error| error.to_string())
}

pub fn save(root: &Path, index: &RepoIndex) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let path = index_path(root, &index.host_id, &index.workspace);
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(index).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

pub async fn build(
    root: &Path,
    host_id: &str,
    workspace: &str,
    host: &dyn Host,
) -> Result<RepoIndex, String> {
    let file_result = host
        .exec(ExecRequest {
            command: "find . -type d \\( -name .git -o -name node_modules -o -name target -o -name .venv -o -name dist -o -name build \\) -prune -o -type f -printf '%p\\t%s\\n' | head -n 20001".into(),
            cwd: Some(workspace.to_owned()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("repository index file scan failed: {error}"))?;
    let file_stdout = file_result.result.stdout;
    let mut files = Vec::new();
    let mut truncated = false;
    for line in file_stdout.lines().take(MAX_FILES + 1) {
        let Some((path, size)) = line.split_once('\t') else {
            continue;
        };
        if files.len() >= MAX_FILES {
            truncated = true;
            break;
        }
        let size_bytes = size.parse::<i64>().unwrap_or(0);
        if size_bytes <= MAX_FILE_BYTES {
            files.push(IndexFile {
                path: path.trim_start_matches("./").to_owned(),
                size_bytes,
            });
        }
    }
    if file_stdout.lines().count() > MAX_FILES {
        truncated = true;
    }

    let symbol_result = host
        .exec(ExecRequest {
            command: "rg -n --hidden --glob '!.git/**' --glob '!node_modules/**' --glob '!target/**' --glob '!.venv/**' --glob '!dist/**' --glob '!build/**' '^[[:space:]]*(export[[:space:]]+)?(async[[:space:]]+)?(fn|function|class|interface|trait|struct|enum|const|def|module)[[:space:]]+' . | head -n 20000".into(),
            cwd: Some(workspace.to_owned()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("repository index symbol scan failed: {error}"))?;
    let symbol_stdout = symbol_result.result.stdout;
    let mut symbols = Vec::new();
    for line in symbol_stdout.lines() {
        if line.len() > 1024 {
            continue;
        }
        let mut parts = line.splitn(3, ':');
        let Some(path) = parts.next() else { continue };
        let Some(line_number) = parts.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let Some(text) = parts.next() else { continue };
        symbols.push(IndexSymbol {
            path: path.trim_start_matches("./").to_owned(),
            line: line_number,
            text: text.trim().to_owned(),
        });
    }
    if symbol_stdout.len() > MAX_OUTPUT_BYTES {
        truncated = true;
    }
    let index = RepoIndex {
        host_id: host_id.to_owned(),
        workspace: workspace.to_owned(),
        built_at: Utc::now(),
        status: if truncated { "limited" } else { "ready" }.to_owned(),
        files,
        symbols,
        truncated,
        error: None,
    };
    save(root, &index)?;
    Ok(index)
}

pub fn glob(index: &RepoIndex, pattern: &str) -> Vec<Value> {
    index
        .files
        .iter()
        .filter(|file| glob_match(pattern, &file.path))
        .map(|file| {
            serde_json::json!({
                "path": file.path,
                "size_bytes": file.size_bytes,
                "artifact_ref": format!("repo-index://{}/{}", index.host_id, file.path),
            })
        })
        .collect()
}

pub fn find_symbol(index: &RepoIndex, query: &str) -> Vec<Value> {
    let query = query.to_ascii_lowercase();
    index
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.text.to_ascii_lowercase().contains(&query)
                || symbol.path.to_ascii_lowercase().contains(&query)
        })
        .map(symbol_value)
        .collect()
}

pub fn search(index: &RepoIndex, query: &str) -> Vec<Value> {
    let query = query.to_ascii_lowercase();
    index
        .symbols
        .iter()
        .filter(|symbol| symbol.text.to_ascii_lowercase().contains(&query))
        .map(symbol_value)
        .collect()
}

fn symbol_value(symbol: &IndexSymbol) -> Value {
    serde_json::json!({
        "path": symbol.path,
        "line": symbol.line,
        "text": symbol.text,
        "artifact_ref": format!("repo-index://{}#{}:{}", symbol.path, symbol.line, symbol.text),
    })
}

fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern == "*" || pattern == "**" {
        return true;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return path == pattern;
    }
    let mut cursor = 0;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(found) = path[cursor..].find(part) else {
            return false;
        };
        if index == 0 && found != 0 {
            return false;
        }
        cursor += found + part.len();
    }
    pattern.ends_with('*') || cursor == path.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_path_is_stable_and_workspace_scoped() {
        assert_eq!(
            index_path(Path::new("/tmp/index"), "host", "/repo"),
            index_path(Path::new("/tmp/index"), "host", "/repo")
        );
        assert_ne!(
            index_path(Path::new("/tmp/index"), "host", "/repo"),
            index_path(Path::new("/tmp/index"), "host", "/other")
        );
    }

    #[test]
    fn glob_results_are_bounded_to_indexed_paths() {
        let index = RepoIndex {
            files: vec![
                IndexFile {
                    path: "src/main.rs".into(),
                    size_bytes: 1,
                },
                IndexFile {
                    path: "README.md".into(),
                    size_bytes: 2,
                },
            ],
            ..RepoIndex::default()
        };
        assert_eq!(glob(&index, "src/*").len(), 1);
        assert_eq!(glob(&index, "*.md").len(), 1);
    }
}
