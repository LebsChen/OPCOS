use opcos_rvm::{ExecRequest, HttpRvmClient, RvmClient, RvmClientConfig, join_remote_path};
use serde_json::json;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = env::var("RVM_WIN_URL")?;
    let token = env::var("RVM_WIN_TOKEN")?;
    let workspace = r"C:\Users\Team";
    let root = join_remote_path(workspace, ".opcos-git-smoke");
    let client = HttpRvmClient::new(RvmClientConfig::new(url.parse()?, token)?)?;
    let result = async {
        client
            .write(&join_remote_path(&root, "README.md"), "# OPCOS M7 smoke\n")
            .await?;
        run(&client, &root, "git init").await?;
        run(&client, &root, "git switch -c devin/1785513605-m7-smoke").await?;
        run(&client, &root, "git add -- README.md").await?;
        let commit = run_with_env(
            &client,
            &root,
            "git commit -m 'M7 smoke commit'",
            json!({
                "GIT_AUTHOR_NAME":"OPCOS Smoke",
                "GIT_AUTHOR_EMAIL":"opcos-smoke@example.invalid",
                "GIT_COMMITTER_NAME":"OPCOS Smoke",
                "GIT_COMMITTER_EMAIL":"opcos-smoke@example.invalid"
            }),
        )
        .await?;
        let log = run(&client, &root, "git log -1 --oneline").await?;
        println!(
            "git_smoke_commit_exit={} commit_stdout={} log_stdout={}",
            commit["result"]["exit_code"], commit["result"]["stdout"], log["result"]["stdout"]
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let cleanup = client
        .exec_sync(ExecRequest {
            command: format!(
                "Remove-Item -LiteralPath '{}' -Recurse -Force",
                root.replace('\'', "''")
            ),
            cwd: Some(workspace.into()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await;
    let removed = client.ls(Some(&root)).await.is_err();
    println!(
        "git_smoke_cleanup={} cleanup_exit={}",
        removed,
        cleanup.map(|value| value.result.exit_code).unwrap_or(-1)
    );
    result?;
    if !removed {
        return Err("smoke repository was not removed".into());
    }
    Ok(())
}

async fn run(
    client: &HttpRvmClient,
    cwd: &str,
    command: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    run_with_env(client, cwd, command, serde_json::Value::Null).await
}

async fn run_with_env(
    client: &HttpRvmClient,
    cwd: &str,
    command: &str,
    env: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(serde_json::to_value(
        client
            .exec_sync(ExecRequest {
                command: command.into(),
                cwd: Some(cwd.into()),
                timeout_seconds: 30,
                session: None,
                env: (!env.is_null()).then_some(env),
            })
            .await?,
    )?)
}
