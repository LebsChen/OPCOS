//! GitHub instance identity.
//!
//! OPCOS talks to one API shape but to more than one GitHub deployment:
//! `github.com` and any number of GitHub Enterprise Server instances. Every
//! repository, credential, push authorization and API call is therefore
//! qualified by the instance host, so that the same `owner/name` on two
//! instances never shares a credential or an authorization grant.

use opcos_store::GitHubInstanceRecord;
use serde::{Deserialize, Serialize};

pub const DOTCOM_HOST: &str = "github.com";
pub const DOTCOM_API_BASE: &str = "https://api.github.com";
/// GitHub Enterprise Server exposes the REST API under this prefix.
pub const ENTERPRISE_API_PATH: &str = "/api/v3";

/// A configured GitHub Enterprise Server instance.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct GitHubInstanceConfig {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<String>,
}

/// A resolved GitHub deployment: `github.com` or an allow-listed Enterprise host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubInstance {
    host: String,
    api_base: String,
    token_secret: Option<String>,
}

impl GitHubInstance {
    pub fn dotcom() -> Self {
        Self {
            host: DOTCOM_HOST.into(),
            api_base: DOTCOM_API_BASE.into(),
            token_secret: None,
        }
    }

    /// Build an Enterprise instance from a registered configuration entry.
    pub fn from_config(config: &GitHubInstanceConfig) -> Result<Self, String> {
        let host = normalize_host(&config.host)?;
        if host == DOTCOM_HOST || host == "api.github.com" {
            return Err(
                "github.com is always available and must not be registered as a GitHub Enterprise instance"
                    .into(),
            );
        }
        let token_secret = config
            .token_secret
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let api_base = match config.api_base.as_deref().map(str::trim) {
            Some(value) if !value.is_empty() => normalize_enterprise_api_base(&host, value)?,
            _ => format!("https://{host}{ENTERPRISE_API_PATH}"),
        };
        Ok(Self {
            host,
            api_base,
            token_secret,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn is_dotcom(&self) -> bool {
        self.host == DOTCOM_HOST
    }

    /// Secret name bound to this instance in the `connector-token` scope.
    ///
    /// `github.com` keeps the historical `github` name; every Enterprise
    /// instance is namespaced by host so credentials cannot be borrowed.
    pub fn connector_secret_name(&self) -> String {
        if self.is_dotcom() {
            "github".into()
        } else {
            format!("github@{}", self.host)
        }
    }

    /// Secret name in the `asset-secret` scope that is bound to this instance,
    /// if the instance requires a specific one.
    pub fn bound_token_secret(&self) -> Option<&str> {
        self.token_secret.as_deref()
    }

    /// Reject a token secret that belongs to a different GitHub instance.
    pub fn authorize_token_secret(&self, requested: &str) -> Result<(), String> {
        let Some(bound) = self.bound_token_secret() else {
            return Ok(());
        };
        if bound == requested.trim() {
            return Ok(());
        }
        Err(format!(
            "token secret is not bound to GitHub instance {}",
            self.host
        ))
    }

    pub fn repo_endpoint(&self, repo: &str) -> String {
        format!("{}/repos/{}", self.api_base, repo.trim_matches('/'))
    }

    pub fn api_endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.api_base, path.trim_start_matches('/'))
    }

    pub fn web_base(&self) -> String {
        format!("https://{}", self.host)
    }

    /// Instance-qualified repository identity, used for authorization targets,
    /// idempotency keys and event subjects.
    pub fn canonical_repo(&self, repo: &str) -> String {
        format!("{}/{}", self.host, repo.trim_matches('/'))
    }
}

/// A repository on a specific GitHub instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubRepoRef {
    pub instance: GitHubInstance,
    /// `owner/name`, without host and without a `.git` suffix.
    pub repo: String,
}

impl GitHubRepoRef {
    pub fn canonical(&self) -> String {
        self.instance.canonical_repo(&self.repo)
    }
}

fn normalize_host(host: &str) -> Result<String, String> {
    let host = host.trim().trim_end_matches('/');
    let host = host
        .strip_prefix("https://")
        .or_else(|| host.strip_prefix("http://"))
        .unwrap_or(host);
    let host = host.split('/').next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or_default();
    let lowered = host.to_ascii_lowercase();
    if lowered.is_empty() {
        return Err("GitHub host is required".into());
    }
    let (name, port) = match lowered.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (lowered.as_str(), None),
    };
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '.' || character == '-'
        })
        || name.starts_with('.')
        || name.starts_with('-')
        || name.ends_with('.')
        || name.ends_with('-')
        || !name.contains('.')
    {
        return Err(format!("GitHub host is not a valid hostname: {name}"));
    }
    if let Some(port) = port
        && (port.is_empty() || port.parse::<u16>().is_err())
    {
        return Err("GitHub host port is invalid".into());
    }
    Ok(lowered)
}

fn normalize_enterprise_api_base(host: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.starts_with("http://") {
        return Err("GitHub Enterprise API base must use https".into());
    }
    let with_scheme = if raw.starts_with("https://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let parsed = url::Url::parse(&with_scheme)
        .map_err(|_| "GitHub Enterprise API base is not a valid URL".to_owned())?;
    if parsed.scheme() != "https" {
        return Err("GitHub Enterprise API base must use https".into());
    }
    let parsed_host = match (parsed.host_str(), parsed.port()) {
        (Some(name), Some(port)) => format!("{}:{port}", name.to_ascii_lowercase()),
        (Some(name), None) => name.to_ascii_lowercase(),
        (None, _) => return Err("GitHub Enterprise API base has no host".into()),
    };
    if parsed_host != host {
        return Err(format!(
            "GitHub Enterprise API base host {parsed_host} does not match instance host {host}"
        ));
    }
    let path = parsed.path().trim_end_matches('/');
    if path.is_empty() {
        return Ok(format!("https://{host}{ENTERPRISE_API_PATH}"));
    }
    if path == ENTERPRISE_API_PATH {
        return Ok(format!("https://{host}{ENTERPRISE_API_PATH}"));
    }
    Err(format!(
        "GitHub Enterprise API base must end with {ENTERPRISE_API_PATH}"
    ))
}

/// Parse registered Enterprise instances from settings, rejecting invalid entries.
pub fn parse_instances(value: &serde_json::Value) -> Result<Vec<GitHubInstance>, String> {
    let Some(entries) = value.as_array() else {
        if value.is_null() {
            return Ok(Vec::new());
        }
        return Err("github_instances must be an array".into());
    };
    let mut instances: Vec<GitHubInstance> = Vec::new();
    for entry in entries {
        let config: GitHubInstanceConfig = serde_json::from_value(entry.clone())
            .map_err(|_| "github_instances entries must have a host".to_owned())?;
        let instance = GitHubInstance::from_config(&config)?;
        if instances
            .iter()
            .any(|existing| existing.host() == instance.host())
        {
            return Err(format!(
                "GitHub Enterprise instance {} is registered more than once",
                instance.host()
            ));
        }
        instances.push(instance);
    }
    Ok(instances)
}

/// Build instances from the persisted registry, rejecting invalid entries so a
/// misconfigured instance can never silently route to the wrong deployment.
pub fn instances_from_records(
    records: &[GitHubInstanceRecord],
) -> Result<Vec<GitHubInstance>, String> {
    records
        .iter()
        .map(|record| {
            GitHubInstance::from_config(&GitHubInstanceConfig {
                host: record.host.clone(),
                api_base: Some(record.api_base.clone()),
                token_secret: record.token_secret.clone(),
            })
        })
        .collect()
}

/// Resolve a host against the registered instances. `github.com` is always
/// allowed; any other host must be registered, and there is no fallback.
pub fn resolve_host(host: &str, instances: &[GitHubInstance]) -> Result<GitHubInstance, String> {
    let host = normalize_host(host)?;
    if host == DOTCOM_HOST || host == "api.github.com" || host == "www.github.com" {
        return Ok(GitHubInstance::dotcom());
    }
    instances
        .iter()
        .find(|instance| instance.host() == host)
        .cloned()
        .ok_or_else(|| format!("GitHub host {host} is not a registered GitHub Enterprise instance"))
}

/// Split a repository URL into its host and `owner/name` path.
pub fn split_repo_url(repo_url: &str) -> Result<(String, String), String> {
    let trimmed = repo_url.trim();
    if trimmed.is_empty() {
        return Err("repository URL is required".into());
    }
    let (host, path) = if let Some(rest) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("ssh://"))
        .or_else(|| trimmed.strip_prefix("git://"))
    {
        let rest = rest.rsplit('@').next().unwrap_or_default();
        let (authority, path) = rest
            .split_once('/')
            .ok_or_else(|| "repository URL has no repository path".to_owned())?;
        (authority.to_owned(), path.to_owned())
    } else if let Some((authority, path)) = trimmed.split_once(':') {
        // scp-style: git@host:owner/name.git
        let authority = authority.rsplit('@').next().unwrap_or_default();
        (authority.to_owned(), path.to_owned())
    } else {
        return Err("repository URL is not a recognized Git URL".into());
    };
    let host = normalize_host(&host)?;
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_matches('/')
        .trim_end_matches(".git");
    let mut segments = path.split('/');
    let owner = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() {
        return Err("repository URL has an invalid owner/repository path".into());
    }
    Ok((host, format!("{owner}/{name}")))
}

/// Resolve a repository URL to an instance-qualified repository reference.
pub fn resolve_repo_url(
    repo_url: &str,
    instances: &[GitHubInstance],
) -> Result<GitHubRepoRef, String> {
    let (host, repo) = split_repo_url(repo_url)?;
    let instance = resolve_host(&host, instances)?;
    Ok(GitHubRepoRef { instance, repo })
}

/// Resolve a pull request URL to its instance, repository and number.
pub fn resolve_pull_request_url(
    pr_url: &str,
    instances: &[GitHubInstance],
) -> Result<(GitHubRepoRef, u64), String> {
    let trimmed = pr_url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .ok_or_else(|| "expected an https pull request URL".to_owned())?;
    let (host, path) = rest
        .split_once('/')
        .ok_or_else(|| "expected a valid GitHub pull request URL".to_owned())?;
    let instance = resolve_host(host, instances)?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[2] != "pull" {
        return Err("expected a valid GitHub pull request URL".into());
    }
    let repo = format!("{}/{}", parts[0], parts[1]);
    if parts[0].is_empty() || parts[1].is_empty() {
        return Err("expected a valid GitHub pull request URL".into());
    }
    let number = parts[3]
        .parse::<u64>()
        .map_err(|_| "expected a valid pull request number".to_owned())?;
    Ok((GitHubRepoRef { instance, repo }, number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instances() -> Vec<GitHubInstance> {
        parse_instances(&json!([
            {"host": "ghe.example.com", "token_secret": "ghe-token"},
            {"host": "git.internal.test", "api_base": "https://git.internal.test/api/v3"}
        ]))
        .unwrap()
    }

    #[test]
    fn dotcom_keeps_public_api_base_and_secret_name() {
        let dotcom = GitHubInstance::dotcom();
        assert_eq!(dotcom.api_base(), "https://api.github.com");
        assert_eq!(dotcom.connector_secret_name(), "github");
        assert_eq!(
            dotcom.repo_endpoint("owner/repo"),
            "https://api.github.com/repos/owner/repo"
        );
    }

    #[test]
    fn enterprise_api_base_is_normalized_to_api_v3() {
        let bare = GitHubInstance::from_config(&GitHubInstanceConfig {
            host: "ghe.example.com".into(),
            api_base: None,
            token_secret: None,
        })
        .unwrap();
        assert_eq!(bare.api_base(), "https://ghe.example.com/api/v3");

        let explicit = GitHubInstance::from_config(&GitHubInstanceConfig {
            host: "ghe.example.com".into(),
            api_base: Some("https://ghe.example.com/".into()),
            token_secret: None,
        })
        .unwrap();
        assert_eq!(explicit.api_base(), "https://ghe.example.com/api/v3");

        let suffixed = GitHubInstance::from_config(&GitHubInstanceConfig {
            host: "ghe.example.com:8443".into(),
            api_base: Some("https://ghe.example.com:8443/api/v3".into()),
            token_secret: None,
        })
        .unwrap();
        assert_eq!(suffixed.api_base(), "https://ghe.example.com:8443/api/v3");
    }

    #[test]
    fn enterprise_api_base_rejects_mismatched_or_insecure_targets() {
        let mismatch = GitHubInstance::from_config(&GitHubInstanceConfig {
            host: "ghe.example.com".into(),
            api_base: Some("https://other.example.com/api/v3".into()),
            token_secret: None,
        });
        assert!(mismatch.is_err());

        let insecure = GitHubInstance::from_config(&GitHubInstanceConfig {
            host: "ghe.example.com".into(),
            api_base: Some("http://ghe.example.com/api/v3".into()),
            token_secret: None,
        });
        assert!(insecure.is_err());

        let wrong_path = GitHubInstance::from_config(&GitHubInstanceConfig {
            host: "ghe.example.com".into(),
            api_base: Some("https://ghe.example.com/api/v4".into()),
            token_secret: None,
        });
        assert!(wrong_path.is_err());
    }

    #[test]
    fn dotcom_cannot_be_registered_as_enterprise() {
        assert!(parse_instances(&json!([{"host": "github.com"}])).is_err());
        assert!(parse_instances(&json!([{"host": "api.github.com"}])).is_err());
    }

    #[test]
    fn duplicate_instances_are_rejected() {
        assert!(
            parse_instances(&json!([{"host": "ghe.example.com"}, {"host": "GHE.example.com"}]))
                .is_err()
        );
    }

    #[test]
    fn unregistered_hosts_are_rejected_without_fallback() {
        let error = resolve_host("ghe.unknown.test", &instances()).unwrap_err();
        assert!(error.contains("not a registered"));
        assert!(resolve_host("github.com", &[]).unwrap().is_dotcom());
    }

    #[test]
    fn same_repository_on_two_instances_has_distinct_identity() {
        let instances = instances();
        let dotcom = resolve_repo_url("https://github.com/acme/app.git", &instances).unwrap();
        let enterprise =
            resolve_repo_url("https://ghe.example.com/acme/app.git", &instances).unwrap();
        let other = resolve_repo_url("git@git.internal.test:acme/app.git", &instances).unwrap();
        assert_eq!(dotcom.repo, "acme/app");
        assert_eq!(dotcom.canonical(), "github.com/acme/app");
        assert_eq!(enterprise.canonical(), "ghe.example.com/acme/app");
        assert_eq!(other.canonical(), "git.internal.test/acme/app");
        assert_ne!(dotcom.canonical(), enterprise.canonical());
        assert_ne!(
            dotcom.instance.connector_secret_name(),
            enterprise.instance.connector_secret_name()
        );
        assert_eq!(
            enterprise.instance.repo_endpoint("acme/app"),
            "https://ghe.example.com/api/v3/repos/acme/app"
        );
    }

    #[test]
    fn token_secret_is_bound_to_its_instance() {
        let instances = instances();
        let enterprise = resolve_host("ghe.example.com", &instances).unwrap();
        assert!(enterprise.authorize_token_secret("ghe-token").is_ok());
        assert!(enterprise.authorize_token_secret("dotcom-token").is_err());
        // github.com keeps caller-chosen secret names.
        assert!(
            GitHubInstance::dotcom()
                .authorize_token_secret("anything")
                .is_ok()
        );
    }

    #[test]
    fn pull_request_urls_resolve_to_their_instance() {
        let instances = instances();
        let (repo, number) =
            resolve_pull_request_url("https://github.com/acme/app/pull/12", &instances).unwrap();
        assert_eq!(repo.canonical(), "github.com/acme/app");
        assert_eq!(number, 12);
        let (repo, number) =
            resolve_pull_request_url("https://ghe.example.com/acme/app/pull/12", &instances)
                .unwrap();
        assert_eq!(repo.canonical(), "ghe.example.com/acme/app");
        assert_eq!(number, 12);
        assert!(
            resolve_pull_request_url("https://evil.test/acme/app/pull/12", &instances).is_err()
        );
        assert!(resolve_pull_request_url("https://github.com/acme/app/issues/12", &[]).is_err());
    }

    #[test]
    fn repository_urls_must_be_complete() {
        assert!(split_repo_url("https://github.com/acme").is_err());
        assert!(split_repo_url("acme/app").is_err());
        assert_eq!(
            split_repo_url("https://user@github.com/acme/app.git").unwrap(),
            ("github.com".into(), "acme/app".into())
        );
    }
}
