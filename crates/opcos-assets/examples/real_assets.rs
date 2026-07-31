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
    for path in [
        "C:\\Users\\Team",
        "C:\\Users\\Team\\work",
        "C:\\Users\\Team\\work\\Cloud-Dev",
    ] {
        let listing = client.ls(Some(path)).await?;
        println!(
            "ls {}: {}",
            path,
            listing
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    for path in [
        "C:\\Users\\Team\\work\\Cloud-Dev\\AGENTS.md",
        "C:\\Users\\Team/work/Cloud-Dev/AGENTS.md",
    ] {
        println!("read {}: {}", path, client.read(path).await.is_ok());
    }
    let repo = format!("{workspace}/work/Cloud-Dev");
    for path in [
        "AGENTS.md",
        "CLAUDE.md",
        ".windsurfrules",
        ".cursor/rules",
        ".agents/skills",
        ".devin/blueprint.yaml",
    ] {
        println!(
            "probe {}={}",
            path,
            client.ls(Some(&format!("{repo}\\{path}"))).await.is_ok()
                || client.read(&format!("{repo}\\{path}")).await.is_ok()
        );
    }
    let bundle = discover(&client, &repo).await?;
    let instruction_count = bundle
        .agents
        .iter()
        .filter(|item| {
            item.path.ends_with("AGENTS.md")
                || item.path.ends_with("CLAUDE.md")
                || item.path.contains(".cursor/rules/")
                || item.path.ends_with(".windsurfrules")
        })
        .count();
    println!(
        "repo={} instruction_files={} knowledge={} playbook={} skills={}",
        repo,
        instruction_count,
        bundle.knowledge.len(),
        bundle.playbook.is_some(),
        bundle.skills.len()
    );
    let system_message = bundle.system_instructions();
    println!(
        "system_order agents={} knowledge={} playbook={} skill={} secret_or_token={}",
        system_message.find("[AGENTS").is_some(),
        system_message.find("[Knowledge").is_some(),
        system_message.find("[Playbook").is_some(),
        system_message.find("[Skill").is_some(),
        system_message.to_ascii_lowercase().contains("token")
            || system_message.to_ascii_lowercase().contains("secret")
    );
    if env::var_os("OPCOS_SMOKE_SKIP_EXEC").is_some() {
        println!("safe_command=skipped");
        return Ok(());
    }
    let blueprint_text = RvmClient::read(&client, &format!("{repo}/.devin/blueprint.yaml"))
        .await?
        .content;
    let blueprint = parse_blueprint(&blueprint_text)?;
    let phase_count = usize::from(!blueprint.initialize.is_empty())
        + usize::from(!blueprint.dependencies.is_empty())
        + usize::from(!blueprint.build.is_empty());
    let command_count =
        blueprint.initialize.len() + blueprint.dependencies.len() + blueprint.build.len();
    println!("blueprint_phases={phase_count} blueprint_commands={command_count}");
    let result = client
        .exec_sync(ExecRequest {
            command: "node --version".into(),
            cwd: Some(repo),
            timeout_seconds: 60,
            session: Some("opcos-m6-smoke".into()),
            env: None,
        })
        .await?;
    println!(
        "safe_command exit_code={} stdout_bytes={} stderr_bytes={}",
        result.result.exit_code,
        result.result.stdout.len(),
        result.result.stderr.len()
    );
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
