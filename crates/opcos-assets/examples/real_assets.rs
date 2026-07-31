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
    if env::var_os("OPCOS_SMOKE_FIXTURE").is_some() {
        return run_fixture_smoke(&client, &workspace).await;
    }
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

async fn run_fixture_smoke(
    client: &HttpRvmClient,
    workspace: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = opcos_rvm::join_remote_path(workspace, ".opcos-smoke");
    let create = format!(
        "cmd /c \"mkdir \\\"{root}\\.cursor\\rules\\\" & mkdir \\\"{root}\\.agents\\skills\\demo\\\" & mkdir \\\"{root}\\.agents\\knowledge\\\" & mkdir \\\"{root}\\.agents\\playbooks\\\" & mkdir \\\"{root}\\.devin\\\"\""
    );
    let result = async {
        client
            .exec_sync(ExecRequest {
                command: create,
                cwd: Some(workspace.to_owned()),
                timeout_seconds: 30,
                session: Some("opcos-m6-fixture".into()),
                env: None,
            })
            .await?;
    let files = [
        (
            "AGENTS.md",
            "# Agent instructions\nUse the repository policy.",
        ),
        ("CLAUDE.md", "Read AGENTS.md for repository instructions."),
        (".windsurfrules", "Use the repository rules."),
        (".cursor\\rules\\project.mdc", "Use the project rule."),
        (
            ".agents\\skills\\demo\\SKILL.md",
            "---\nname: demo\n---\nUse the demo skill.",
        ),
        (
            ".agents\\knowledge\\demo.md",
            "---\nid: demo-knowledge\nname: Demo Knowledge\ntrigger: smoke\nscope: repository\n---\nKnown smoke context.",
        ),
        (
            ".agents\\playbooks\\demo.md",
            "---\nid: demo-playbook\nname: Demo Playbook\ntrigger: explicit\nscope: repository\n---\nRun the smoke playbook.",
        ),
        (
            ".devin\\blueprint.yaml",
            "initialize:\n  - node --version\ndependencies: []\nbuild: []\n",
        ),
    ];
    for (path, content) in files {
        client
            .write(&opcos_rvm::join_remote_path(&root, path), content)
            .await?;
    }
        let bundle = discover(client, &root).await?;
        let instruction_files = bundle.agents.len();
        let blueprint_text = RvmClient::read(
            client,
            &opcos_rvm::join_remote_path(&root, ".devin\\blueprint.yaml"),
        )
        .await?
        .content;
        let blueprint = parse_blueprint(&blueprint_text)?;
        let command = blueprint
            .initialize
            .first()
            .ok_or("missing initialize command")?;
        let exec = client
            .exec_sync(ExecRequest {
                command: command.clone(),
                cwd: Some(root.clone()),
                timeout_seconds: 30,
                session: Some("opcos-m6-fixture".into()),
                env: None,
            })
            .await?;
        let system = bundle.system_instructions();
        println!(
            "fixture instruction_files={} knowledge={} playbook={} skills={} phases={} commands={}",
            instruction_files,
            bundle.knowledge.len(),
            bundle.playbook.is_some(),
            bundle.skills.len(),
            usize::from(!blueprint.initialize.is_empty())
                + usize::from(!blueprint.dependencies.is_empty())
                + usize::from(!blueprint.build.is_empty()),
            blueprint.initialize.len() + blueprint.dependencies.len() + blueprint.build.len()
        );
        println!(
            "fixture blueprint_exit={} stdout={} system={:?} token_or_secret={}",
            exec.result.exit_code,
            exec.result.stdout.trim(),
            system,
            system.to_ascii_lowercase().contains("token")
                || system.to_ascii_lowercase().contains("secret")
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let cleanup = client
        .exec_sync(ExecRequest {
            command: format!("cmd /c \"rmdir /s /q \\\"{root}\\\"\""),
            cwd: Some(workspace.to_owned()),
            timeout_seconds: 30,
            session: Some("opcos-m6-fixture".into()),
            env: None,
        })
        .await;
    let verify = client.ls(Some(&root)).await.is_err();
    println!(
        "fixture_cleanup={} cleanup_exit={}",
        verify,
        cleanup.map(|r| r.result.exit_code).unwrap_or(-1)
    );
    result?;
    if !verify {
        return Err("fixture directory still exists".into());
    }
    Ok(())
}
