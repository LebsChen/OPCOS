use opcos_rvm::{ExecRequest, HttpRvmClient, RvmClient, RvmClientConfig, WorklogCursor};
use std::env;
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = Url::parse(&env::var("RVM_WINDOWS_URL")?)?;
    let token = env::var("RVM_WINDOWS_TOKEN")?;
    let client = HttpRvmClient::new(RvmClientConfig::new(base, token)?)?;

    let health = client.health().await?;
    println!(
        "health status={} version={} host={} workspace={} capabilities={}",
        health.status,
        health.version.as_deref().unwrap_or("unknown"),
        health.host.as_deref().unwrap_or("unknown"),
        health.workspace.as_deref().unwrap_or("unknown"),
        health.capabilities.len()
    );
    let hostname = client
        .exec_sync(ExecRequest {
            command: "hostname".into(),
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await?;
    println!("hostname={}", hostname.result.stdout.trim());

    let workspace = health.workspace.as_deref().unwrap_or(".");
    let temp_path = format!("{workspace}/opcos-m1-smoke.txt");
    client.write(&temp_path, "opcos-m1\n").await?;
    let content = client.read(&temp_path).await?;
    println!("file_read_bytes={}", content.size);
    client
        .exec_sync(ExecRequest {
            command: if health.platform.as_deref() == Some("win32") {
                format!("del /q \"{temp_path}\"")
            } else {
                format!("rm -f -- '{temp_path}'")
            },
            cwd: Some(workspace.to_owned()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await?;

    let git = client.git_changes(workspace, "HEAD").await;
    println!(
        "git_changes={}",
        if git.is_ok() { "ok" } else { "unavailable" }
    );
    let page = client.worklog_query("", 200).await?;
    let mut cursor = WorklogCursor::new();
    println!(
        "worklog_events={} cursor_accepted={}",
        page.events.len(),
        cursor.accept(&page)
    );
    Ok(())
}
