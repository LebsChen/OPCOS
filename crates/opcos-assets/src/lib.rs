use async_trait::async_trait;
use opcos_rvm::{RvmClient, RvmError};
use serde::{Deserialize, Serialize};
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
    pub agents: Vec<InstructionSource>,
    pub knowledge: Vec<KnowledgeEntry>,
    pub playbook: Option<Playbook>,
    pub skills: Vec<SkillEntry>,
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

impl AssetBundle {
    pub fn system_instructions(&self) -> String {
        let mut sections = Vec::new();
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
        let path = format!("{workspace}/{name}");
        if let Ok(content) = reader.read(&path).await {
            bundle.agents.push(InstructionSource { path, content });
        }
    }
    for path in [".cursor/rules", ".agents/skills"] {
        let root = format!("{workspace}/{path}");
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
        let entries = reader.list(Some(&current)).await?;
        for (name, dir) in entries {
            let child = format!("{current}/{name}");
            if dir {
                pending.push(child);
            } else if name == "SKILL.md" {
                bundle
                    .skills
                    .push(parse_skill(&child, &reader.read(&child).await?));
            } else if name.ends_with(".md") {
                bundle.agents.push(InstructionSource {
                    path: child.clone(),
                    content: reader.read(&child).await?,
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
    fn system_instruction_order_is_agents_knowledge_playbook_skill() {
        let bundle = AssetBundle {
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
        };
        let rendered = bundle.system_instructions();
        assert!(rendered.find("agents").unwrap() < rendered.find("knowledge").unwrap());
        assert!(rendered.find("knowledge").unwrap() < rendered.find("playbook").unwrap());
        assert!(rendered.find("playbook").unwrap() < rendered.find("skill").unwrap());
    }
}
