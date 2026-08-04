use async_trait::async_trait;
use opcos_rvm::{RvmClient, RvmError, join_remote_path};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("remote asset: {0}")]
    Remote(#[from] RvmError),
    #[error("invalid asset: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AssetBundle {
    pub instructions: Option<InstructionSource>,
    pub agents: Vec<InstructionSource>,
    pub knowledge: Vec<KnowledgeEntry>,
    pub playbook: Option<Playbook>,
    pub skills: Vec<SkillEntry>,
    pub commands: Vec<CommandEntry>,
    pub mcp_servers: Vec<McpServerEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstructionSource {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct KnowledgeEntry {
    pub title: String,
    pub body: String,
    pub trigger: String,
    pub scope: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Playbook {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub path: String,
    pub content: String,
    pub active: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandArgument {
    pub name: String,
    #[serde(rename = "type", default = "default_argument_type")]
    pub argument_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub arguments: Vec<CommandArgument>,
    pub body: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    pub path: String,
    pub content: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpCatalogEntry {
    pub slug: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    pub enabled: bool,
    pub requires_approval: bool,
    pub transport: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub auth: String,
    #[serde(default)]
    pub required_inputs: Vec<String>,
    #[serde(default)]
    pub credential_inputs: Vec<String>,
}

const MCP_CATALOG_JSON: &str = include_str!("../data/mcp_catalog.json");

pub fn builtin_mcp_catalog() -> Result<Vec<McpCatalogEntry>, AssetError> {
    let entries: Vec<McpCatalogEntry> = serde_json::from_str(MCP_CATALOG_JSON)
        .map_err(|error| AssetError::Invalid(format!("MCP catalog: {error}")))?;
    let mut slugs = std::collections::HashSet::new();
    for entry in &entries {
        if entry.slug.trim().is_empty()
            || entry.name.trim().is_empty()
            || entry.description.trim().is_empty()
            || entry.auth.trim().is_empty()
            || !slugs.insert(entry.slug.clone())
            || entry.enabled
        {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry is invalid: {}",
                entry.slug
            )));
        }
        if entry
            .links
            .values()
            .any(|link| link.to_ascii_lowercase().contains("devin.ai"))
        {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry contains a Devin link: {}",
                entry.slug
            )));
        }
        if entry.env.values().any(|value| !value.is_empty()) {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry contains an environment value: {}",
                entry.slug
            )));
        }
    }
    Ok(entries)
}

fn default_argument_type() -> String {
    "string".to_owned()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Blueprint {
    #[serde(default)]
    pub clone: Vec<String>,
    #[serde(default)]
    pub initialize: Vec<String>,
    #[serde(default, alias = "install")]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default, alias = "post-build")]
    pub post_build: Vec<String>,
    #[serde(default)]
    pub maintenance: Vec<String>,
    #[serde(default, alias = "pre-push")]
    pub pre_push: Vec<String>,
}

pub fn parse_blueprint(yaml: &str) -> Result<Blueprint, AssetError> {
    serde_yaml::from_str(yaml).map_err(|error| AssetError::Invalid(format!("blueprint: {error}")))
}

pub fn redact_secret(value: &mut serde_json::Value, secret: &str) {
    match value {
        serde_json::Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        serde_json::Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_secret(item, secret)),
        serde_json::Value::Object(items) => items
            .values_mut()
            .for_each(|item| redact_secret(item, secret)),
        _ => {}
    }
}

impl AssetBundle {
    pub fn system_instructions(&self) -> String {
        let mut sections = Vec::new();
        if let Some(instructions) = &self.instructions {
            sections.push(format!("[Global Instructions]\n{}", instructions.content));
        }
        for source in &self.agents {
            sections.push(format!(
                "[AGENTS source: {}]\n{}",
                source.path, source.content
            ));
        }
        for entry in self.knowledge.iter().filter(|entry| entry.enabled) {
            sections.push(format!(
                "[Knowledge: {} | trigger: {} | scope: {}]\n{}",
                entry.title, entry.trigger, entry.scope, entry.body
            ));
        }
        if let Some(playbook) = &self.playbook {
            sections.push(format!("[Playbook: {}]\n{}", playbook.title, playbook.body));
        }
        for skill in self.skills.iter().filter(|skill| skill.active) {
            sections.push(format!("[Skill: {}]\n{}", skill.name, skill.content));
        }
        sections.join("\n\n")
    }
}

pub fn parse_knowledge(path: &str, markdown: &str) -> Result<KnowledgeEntry, AssetError> {
    let (frontmatter, body) = split_frontmatter(markdown);
    let title = frontmatter
        .get("name")
        .or_else(|| frontmatter.get("title"))
        .cloned()
        .unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .into()
        });
    Ok(KnowledgeEntry {
        title,
        body: body.to_owned(),
        trigger: frontmatter.get("trigger").cloned().unwrap_or_default(),
        scope: frontmatter.get("scope").cloned().unwrap_or_default(),
        enabled: frontmatter
            .get("enabled")
            .map(|value| value != "false")
            .unwrap_or(true),
    })
}

pub fn parse_playbook(path: &str, markdown: &str) -> Playbook {
    let (frontmatter, body) = split_frontmatter(markdown);
    Playbook {
        title: frontmatter.get("name").cloned().unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .into()
        }),
        body: body.to_owned(),
    }
}

pub fn parse_skill(path: &str, markdown: &str) -> SkillEntry {
    SkillEntry {
        name: path.split('/').rev().nth(1).unwrap_or(path).into(),
        path: path.into(),
        content: markdown.into(),
        active: false,
    }
}

pub fn parse_command(path: &str, markdown: &str) -> Result<CommandEntry, AssetError> {
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return Err(AssetError::Invalid(format!(
            "command {path} is missing YAML frontmatter"
        )));
    };
    let Some(end) = rest.find("\n---") else {
        return Err(AssetError::Invalid(format!(
            "command {path} has unterminated YAML frontmatter"
        )));
    };
    #[derive(Deserialize)]
    struct Frontmatter {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        arguments: Vec<CommandArgument>,
    }
    let frontmatter: Frontmatter = serde_yaml::from_str(&rest[..end])
        .map_err(|error| AssetError::Invalid(format!("command {path}: {error}")))?;
    if frontmatter.name.trim().is_empty() {
        return Err(AssetError::Invalid(format!(
            "command {path} has an empty name"
        )));
    }
    let body = rest[end + 4..].trim_start_matches('\n').to_owned();
    let declared = frontmatter
        .arguments
        .iter()
        .map(|argument| argument.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut cursor = body.as_str();
    while let Some(start) = cursor.find("{{") {
        let after = &cursor[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(AssetError::Invalid(format!(
                "command {} has an unterminated template variable",
                frontmatter.name
            )));
        };
        let variable = after[..end].trim();
        if variable.is_empty() || !declared.contains(variable) {
            return Err(AssetError::Invalid(format!(
                "command {} references undeclared variable '{variable}'",
                frontmatter.name
            )));
        }
        cursor = &after[end + 2..];
    }
    Ok(CommandEntry {
        name: frontmatter.name,
        description: frontmatter.description,
        arguments: frontmatter.arguments,
        body,
        path: path.to_owned(),
    })
}

pub fn expand_command(
    command: &CommandEntry,
    values: &std::collections::HashMap<String, String>,
) -> Result<String, AssetError> {
    let declared = command
        .arguments
        .iter()
        .map(|argument| (argument.name.as_str(), argument))
        .collect::<std::collections::HashMap<_, _>>();
    if let Some(unknown) = values
        .keys()
        .find(|name| !declared.contains_key(name.as_str()))
    {
        return Err(AssetError::Invalid(format!(
            "command argument is unknown: {unknown}"
        )));
    }
    let mut rendered = command.body.clone();
    for argument in &command.arguments {
        let value = values
            .get(&argument.name)
            .cloned()
            .or_else(|| argument.default.clone());
        let Some(value) = value else {
            if argument.required {
                return Err(AssetError::Invalid(format!(
                    "command argument missing: {}",
                    argument.name
                )));
            }
            continue;
        };
        rendered = rendered.replace(&format!("{{{{{}}}}}", argument.name), &value);
    }
    Ok(rendered)
}

fn split_frontmatter(markdown: &str) -> (std::collections::HashMap<String, String>, &str) {
    let mut values = std::collections::HashMap::new();
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return (values, markdown);
    };
    let Some(end) = rest.find("\n---") else {
        return (values, markdown);
    };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once(':') {
            values.insert(key.trim().into(), value.trim().trim_matches('"').into());
        }
    }
    (values, rest[end + 4..].trim_start_matches('\n'))
}

#[async_trait]
pub trait RemoteAssetReader: Send + Sync {
    async fn read(&self, path: &str) -> Result<String, AssetError>;
    async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError>;
}

#[async_trait]
impl RemoteAssetReader for opcos_rvm::HttpRvmClient {
    async fn read(&self, path: &str) -> Result<String, AssetError> {
        Ok(RvmClient::read(self, path).await?.content)
    }

    async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
        Ok(RvmClient::ls(self, path)
            .await?
            .items
            .into_iter()
            .map(|entry| (entry.name, entry.dir))
            .collect())
    }
}

pub async fn discover<R: RemoteAssetReader>(
    reader: &R,
    workspace: &str,
) -> Result<AssetBundle, AssetError> {
    let mut bundle = AssetBundle::default();
    for name in ["AGENTS.md", "CLAUDE.md", ".windsurfrules"] {
        let path = join_remote_path(workspace, name);
        if let Ok(content) = reader.read(&path).await {
            bundle.agents.push(InstructionSource { path, content });
        }
    }
    for path in [
        ".cursor/rules",
        ".agents/skills",
        ".agents/knowledge",
        ".agents/playbooks",
        ".agents/commands",
        ".agents/mcp",
    ] {
        let root = join_remote_path(workspace, path);
        let _ = discover_tree(reader, &root, &mut bundle).await;
    }
    Ok(bundle)
}

async fn discover_tree<R: RemoteAssetReader>(
    reader: &R,
    path: &str,
    bundle: &mut AssetBundle,
) -> Result<(), AssetError> {
    let mut pending = vec![path.to_owned()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = reader.list(Some(&current)).await else {
            continue;
        };
        for (name, dir) in entries {
            let child = join_remote_path(&current, &name);
            if dir {
                pending.push(child);
            } else if name == "SKILL.md" {
                if let Ok(content) = reader.read(&child).await {
                    bundle.skills.push(parse_skill(&child, &content));
                }
            } else if child.replace('\\', "/").contains("/.agents/knowledge/")
                && name.ends_with(".md")
            {
                if let Ok(content) = reader.read(&child).await {
                    bundle.knowledge.push(parse_knowledge(&child, &content)?);
                }
            } else if child.replace('\\', "/").contains("/.agents/playbooks/")
                && name.ends_with(".md")
            {
                if let Ok(content) = reader.read(&child).await {
                    bundle.playbook = Some(parse_playbook(&child, &content));
                }
            } else if child.replace('\\', "/").contains("/.agents/commands/")
                && name.ends_with(".md")
            {
                let content = reader.read(&child).await?;
                bundle.commands.push(parse_command(&child, &content)?);
            } else if child.replace('\\', "/").contains("/.agents/mcp/")
                && (name.ends_with(".json") || name.ends_with(".yaml") || name.ends_with(".yml"))
            {
                let content = reader.read(&child).await?;
                bundle.mcp_servers.push(McpServerEntry {
                    name: name
                        .trim_end_matches(".json")
                        .trim_end_matches(".yaml")
                        .trim_end_matches(".yml")
                        .to_owned(),
                    path: child,
                    content,
                    enabled: false,
                });
            } else if (child.replace('\\', "/").contains("/.cursor/rules/")
                || name.ends_with(".md"))
                && let Ok(content) = reader.read(&child).await
            {
                bundle.agents.push(InstructionSource {
                    path: child.clone(),
                    content,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_instruction_order_is_global_agents_knowledge_playbook_skill() {
        let bundle = AssetBundle {
            instructions: Some(InstructionSource {
                path: "global".into(),
                content: "global".into(),
            }),
            agents: vec![InstructionSource {
                path: "AGENTS.md".into(),
                content: "agents".into(),
            }],
            knowledge: vec![KnowledgeEntry {
                title: "K".into(),
                body: "knowledge".into(),
                trigger: "task".into(),
                scope: "repo".into(),
                enabled: true,
            }],
            playbook: Some(Playbook {
                title: "P".into(),
                body: "playbook".into(),
            }),
            skills: vec![SkillEntry {
                name: "S".into(),
                path: ".agents/skills/s/SKILL.md".into(),
                content: "skill".into(),
                active: true,
            }],
            commands: Vec::new(),
            mcp_servers: Vec::new(),
        };
        let rendered = bundle.system_instructions();
        assert!(rendered.find("global").unwrap() < rendered.find("agents").unwrap());
        assert!(rendered.find("agents").unwrap() < rendered.find("knowledge").unwrap());
        assert!(rendered.find("knowledge").unwrap() < rendered.find("playbook").unwrap());
        assert!(rendered.find("playbook").unwrap() < rendered.find("skill").unwrap());
    }

    #[test]
    fn builtin_mcp_catalog_is_disabled_and_excludes_private_builders() {
        let entries = builtin_mcp_catalog().unwrap();
        assert_eq!(entries.len(), 123);
        assert!(entries.iter().any(|entry| entry.slug == "linear"));
        assert!(entries.iter().any(|entry| entry.slug == "postgres"));
        assert!(!entries.iter().any(|entry| entry.slug.contains("cognition")));
        assert!(!entries.iter().any(|entry| entry.slug == "metabase"));
        assert!(entries.iter().all(|entry| !entry.enabled));
        let combined = serde_json::to_string(&entries)
            .unwrap()
            .to_ascii_lowercase();
        for marker in ["devin.ai", "api.devin", "cog_"] {
            assert!(!combined.contains(marker), "catalog contains {marker}");
        }
    }

    #[test]
    fn parses_frontmatter_and_skill_shape() {
        let knowledge = parse_knowledge(
            ".agents/knowledge/repo.md",
            "---\nname: Repository\ntrigger: build\nscope: repo\nenabled: true\n---\nUse the repository build command.",
        )
        .unwrap();
        assert_eq!(knowledge.title, "Repository");
        assert_eq!(knowledge.trigger, "build");
        assert_eq!(knowledge.body, "Use the repository build command.");
        let skill = parse_skill(
            ".agents/skills/review/SKILL.md",
            "# Review\nCheck the diff.",
        );
        assert_eq!(skill.name, "review");
        assert!(skill.content.contains("Check the diff."));
    }

    #[test]
    fn commands_require_declared_arguments_and_expand_without_execution() {
        let command = parse_command(
            ".agents/commands/verify.md",
            "---\nname: verify\ndescription: Verify\narguments:\n  - name: scope\n    required: true\n---\nRun {{scope}} checks.",
        )
        .unwrap();
        let values = [("scope".to_owned(), "backend".to_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            expand_command(&command, &values).unwrap(),
            "Run backend checks."
        );
        assert!(expand_command(&command, &std::collections::HashMap::new()).is_err());
        assert!(
            expand_command(
                &command,
                &[("typo".to_owned(), "x".to_owned())].into_iter().collect()
            )
            .is_err()
        );
    }

    #[test]
    fn command_parser_rejects_undeclared_template_variables() {
        let error = parse_command(
            ".agents/commands/bad.md",
            "---\nname: bad\n---\nRun {{missing}}.",
        )
        .unwrap_err();
        assert!(error.to_string().contains("undeclared variable"));
    }

    #[test]
    fn commands_and_mcp_discovery_are_not_active_system_instructions() {
        let bundle = AssetBundle {
            commands: vec![CommandEntry {
                name: "verify".into(),
                description: "verify".into(),
                arguments: Vec::new(),
                body: "run checks".into(),
                path: ".agents/commands/verify.md".into(),
            }],
            mcp_servers: vec![McpServerEntry {
                name: "remote".into(),
                path: ".agents/mcp/remote.json".into(),
                content: "{}".into(),
                enabled: false,
            }],
            ..AssetBundle::default()
        };
        assert!(!bundle.system_instructions().contains("run checks"));
        assert!(!bundle.mcp_servers[0].enabled);
    }

    #[test]
    fn parses_structured_blueprint_steps() {
        let blueprint =
            parse_blueprint("initialize:\n  - setup\ninstall:\n  - deps\nbuild:\n  - compile\n")
                .unwrap();
        assert_eq!(blueprint.initialize, vec!["setup"]);
        assert_eq!(blueprint.dependencies, vec!["deps"]);
        assert_eq!(blueprint.build, vec!["compile"]);
    }

    #[test]
    fn redacts_secret_values_from_serialized_outputs() {
        let secret = "known-test-secret";
        let mut value = serde_json::json!({
            "provider": format!("prefix-{secret}"),
            "worklog": [secret],
            "transcript": {"text": secret},
            "log": secret
        });
        redact_secret(&mut value, secret);
        assert!(!value.to_string().contains(secret));
        assert_eq!(value["worklog"][0], "[REDACTED]");
    }

    struct SkillOnlyReader;

    #[async_trait]
    impl RemoteAssetReader for SkillOnlyReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            if path == "/repo/.agents/skills/demo/SKILL.md" {
                Ok("# Demo\nUse the demo skill.".into())
            } else {
                Err(AssetError::Invalid("missing".into()))
            }
        }

        async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            match path {
                Some("/repo/.agents/skills") => Ok(vec![("demo".into(), true)]),
                Some("/repo/.agents/skills/demo") => Ok(vec![("SKILL.md".into(), false)]),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }
    }

    #[tokio::test]
    async fn discovers_skill_when_optional_rules_are_missing() {
        let bundle = discover(&SkillOnlyReader, "/repo").await.unwrap();
        assert_eq!(bundle.skills.len(), 1);
        assert_eq!(bundle.skills[0].name, "demo");
        assert!(bundle.agents.is_empty());
    }
}
