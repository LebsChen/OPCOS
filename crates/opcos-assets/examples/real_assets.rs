use opcos_assets::{discover, parse_blueprint};
use opcos_rvm::{ExecRequest, HttpRvmClient, RvmClient, RvmClientConfig};
use serde_json::json;
use std::env;
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = env::var("RVM_WIN_URL").or_else(|_| env::var("RVM_WINDOWS_URL"))?;
    let token = env::var("RVM_WIN_TOKEN").or_else(|_| env::var("RVM_WINDOWS_TOKEN"))?;
    let client = HttpRvmClient::new(RvmClientConfig::new(Url::parse(&url)?, token)?)?;
    let health = client.health().await?;
    let workspace = health
        .workspace
        .clone()
        .unwrap_or_else(|| "/workspace".into());
    let client = client.with_workspace(workspace.clone());
    let root = client.ls(Some(&workspace)).await?;
    println!(
        "workspace_entries={}",
        root.items
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut repo = workspace.clone();
    for candidate in ["repos", "work", "devin"] {
        let path = format!("{workspace}\\{candidate}");
        if let Ok(entries) = client.ls(Some(&path)).await {
            println!(
                "candidate={} entries={}",
                candidate,
                entries
                    .items
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            for entry in entries.items.iter().filter(|entry| entry.dir).take(20) {
                let nested = format!("{path}\\{}", entry.name);
                if client.read(&format!("{nested}\\AGENTS.md")).await.is_ok() {
                    repo = nested;
                    break;
                }
            }
        }
        if repo != workspace {
            break;
        }
    }
    if repo == workspace {
        let candidate = format!("{workspace}\\work\\Cloud-Dev");
        if client.ls(Some(&candidate)).await.is_ok() {
            repo = candidate;
        }
    }
    let bundle = discover(&client, &repo).await?;
    println!(
        "assets agents={} knowledge={} playbook={} skills={}",
        bundle.agents.len(),
        bundle.knowledge.len(),
        bundle.playbook.is_some(),
        bundle.skills.len()
    );
    let blueprint_text = RvmClient::read(&client, &format!("{repo}/.devin/blueprint.yaml"))
        .await?
        .content;
    let blueprint = parse_blueprint(&blueprint_text)?;
    let command = blueprint
        .initialize
        .first()
        .or_else(|| blueprint.dependencies.first())
        .or_else(|| blueprint.build.first());
    if let Some(command) = command {
        let result = client
            .exec_sync(ExecRequest {
                command: command.clone(),
                cwd: Some(repo),
                timeout_seconds: 180,
                session: Some("opcos-m6-smoke".into()),
                env: None,
            })
            .await?;
        println!(
            "blueprint_step exit_code={} stdout_bytes={} stderr_bytes={}",
            result.result.exit_code,
            result.result.stdout.len(),
            result.result.stderr.len()
        );
    } else {
        println!("blueprint_step=none");
    }
    let mcp = client
        .mcp(json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .await?;
    println!(
        "mcp_tools={}",
        mcp.get("result")
            .and_then(|value| value.get("tools"))
            .and_then(|value| value.as_array())
            .map_or(0, Vec::len)
    );
    Ok(())
}
