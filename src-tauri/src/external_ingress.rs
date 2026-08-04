use chrono::{DateTime, Duration, Utc};
use opcos_store::{ExternalIngressSource, KeyringSecretStore, SecretStore, SqliteStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::watch;

const MAX_EVENTS_PER_POLL: usize = 100;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

pub fn start(
    store: Arc<SqliteStore>,
    secrets: KeyringSecretStore,
    shutdown: watch::Receiver<bool>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .user_agent("OPCOS/0.1")
            .build()
            .expect("external ingress HTTP client");
        let mut shutdown = shutdown;
        loop {
            if *shutdown.borrow() {
                break;
            }
            for source in store
                .load_external_ingress_sources(true)
                .unwrap_or_default()
            {
                if due(&source) {
                    poll_source(&client, &store, &secrets, &source).await;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

fn due(source: &ExternalIngressSource) -> bool {
    let now = Utc::now();
    if source
        .circuit_open_until
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|value| value.with_timezone(&Utc) > now)
    {
        return false;
    }
    source
        .next_attempt_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_none_or(|value| value.with_timezone(&Utc) <= now)
}

async fn poll_source(
    client: &reqwest::Client,
    store: &SqliteStore,
    secrets: &KeyringSecretStore,
    source: &ExternalIngressSource,
) {
    let result = match source.provider.as_str() {
        "rss" | "atom" => poll_feed(client, store, source).await,
        "github" => poll_github(client, store, secrets, source).await,
        _ => Err("unsupported external ingress provider".into()),
    };
    match result {
        Ok(cursor) => {
            let now = Utc::now().to_rfc3339();
            let _ = store.update_external_ingress_state(
                &source.source_id,
                cursor.as_deref(),
                true,
                Some(&(Utc::now() + Duration::seconds(30)).to_rfc3339()),
                0,
                None,
                Some(&now),
                None,
            );
        }
        Err(error) => {
            let failures = source.consecutive_failures.saturating_add(1);
            let delay = (30_i64 * 2_i64.pow(failures.min(5))).min(900);
            let next = (Utc::now() + Duration::seconds(delay)).to_rfc3339();
            let circuit =
                (failures >= 5).then(|| (Utc::now() + Duration::minutes(15)).to_rfc3339());
            let _ = store.update_external_ingress_state(
                &source.source_id,
                source.cursor.as_deref(),
                source.initialized,
                Some(&next),
                failures,
                circuit.as_deref(),
                source.last_success_at.as_deref(),
                Some(&sanitize_error(&error)),
            );
        }
    }
}

pub async fn poll_once(
    store: &SqliteStore,
    secrets: &KeyringSecretStore,
    source_id: &str,
) -> Result<(), String> {
    let source = store
        .load_external_ingress_source(source_id)
        .map_err(|error| error.to_string())?
        .ok_or("external ingress source not found")?;
    let client = reqwest::Client::builder()
        .user_agent("OPCOS/0.1")
        .build()
        .map_err(|error| error.to_string())?;
    match source.provider.as_str() {
        "rss" | "atom" => poll_feed(&client, store, &source).await,
        "github" => poll_github(&client, store, secrets, &source).await,
        _ => Err("unsupported external ingress provider".into()),
    }
    .map(|cursor| {
        let _ = store.update_external_ingress_state(
            &source.source_id,
            cursor.as_deref(),
            true,
            None,
            0,
            None,
            Some(&Utc::now().to_rfc3339()),
            None,
        );
    })
}

async fn poll_feed(
    client: &reqwest::Client,
    store: &SqliteStore,
    source: &ExternalIngressSource,
) -> Result<Option<String>, String> {
    let url = source
        .config
        .get("url")
        .and_then(Value::as_str)
        .ok_or("rss source url is required")?;
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("feed request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("feed request failed: {error}"))?
        .text()
        .await
        .map_err(|error| format!("feed response failed: {error}"))?;
    let items = parse_feed_items(&body);
    let newest = items.first().map(|item| item.id.clone());
    if !source.initialized {
        return Ok(newest);
    }
    if items.is_empty() {
        let identity = hex_hash(&body);
        let payload = json!({
            "provider": "rss",
            "source_id": source.source_id,
            "reason": "feed contained no parseable items",
            "summary_sha256": identity,
        });
        let key = format!("external:rss:{}:rejected:{identity}", source.source_id);
        store
            .publish_event(
                "external.ingress.rejected",
                &format!("rss:{}", source.source_id),
                &json!({"identity": identity}),
                &payload,
                Some(&key),
                None,
            )
            .map_err(|error| error.to_string())?;
        return Ok(Some(identity));
    }
    for item in items.into_iter().take(MAX_EVENTS_PER_POLL) {
        let key = format!("external:rss:{}:{}", source.source_id, item.id);
        let payload = normalized_payload("rss", &item.id, item.title, item.url, None);
        store
            .publish_event(
                "external.rss.item.published",
                &format!("rss:{}", source.source_id),
                &json!({"id": item.id}),
                &payload,
                Some(&key),
                None,
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(newest.or_else(|| source.cursor.clone()))
}

async fn poll_github(
    client: &reqwest::Client,
    store: &SqliteStore,
    secrets: &KeyringSecretStore,
    source: &ExternalIngressSource,
) -> Result<Option<String>, String> {
    let repo = source
        .config
        .get("repo")
        .and_then(Value::as_str)
        .ok_or("github repo is required")?;
    let token = secrets
        .get("connector-token:github")
        .map_err(|error| error.to_string())?
        .ok_or("GitHub connector credential is not configured")?;
    let since = source.cursor.as_deref().unwrap_or("");
    let response = client
        .get(format!(
            "https://api.github.com/repos/{repo}/events?per_page=100"
        ))
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("GitHub request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("GitHub request failed: {error}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|error| format!("GitHub response was invalid JSON: {error}"))?;
    let mut newest = source.cursor.clone();
    if !source.initialized {
        return Ok(response
            .first()
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned));
    }
    for item in response.into_iter().take(MAX_EVENTS_PER_POLL) {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or("GitHub event missing id")?;
        if !since.is_empty() && id <= since {
            continue;
        }
        let Some((resource, action)) = github_event_kind(&item) else {
            if newest.as_deref().is_none_or(|value| id > value) {
                newest = Some(id.to_owned());
            }
            continue;
        };
        let raw_payload = item.get("payload").cloned().unwrap_or_else(|| json!({}));
        let payload = normalized_payload(
            "github",
            id,
            github_event_title(&raw_payload),
            github_event_url(&raw_payload),
            Some(json!({"repository":repo,"event":resource,"action":action})),
        );
        let key = format!("external:github:{}:{}", source.source_id, id);
        store
            .publish_event(
                &format!("external.github.{resource}.{action}"),
                &format!("github:repo:{repo}"),
                &json!({"id": id}),
                &payload,
                Some(&key),
                None,
            )
            .map_err(|error| error.to_string())?;
        if newest.as_deref().is_none_or(|value| id > value) {
            newest = Some(id.to_owned());
        }
    }
    Ok(newest)
}

fn github_event_kind(value: &Value) -> Option<(&'static str, String)> {
    let event_type = value.get("type").and_then(Value::as_str)?;
    let action = value
        .get("payload")
        .and_then(|payload| payload.get("action"))
        .and_then(Value::as_str)
        .unwrap_or("updated")
        .replace('-', "_");
    match event_type {
        "PullRequestEvent" => Some(("pull_request", action)),
        "IssueCommentEvent" => Some(("pull_request_comment", action)),
        "IssuesEvent" => Some(("issue", action)),
        "CheckRunEvent" => Some(("check_run", action)),
        _ => None,
    }
}

fn github_event_title(payload: &Value) -> String {
    payload
        .get("pull_request")
        .or_else(|| payload.get("issue"))
        .or_else(|| payload.get("check_run"))
        .and_then(|value| value.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn github_event_url(payload: &Value) -> Option<String> {
    payload
        .get("pull_request")
        .or_else(|| payload.get("issue"))
        .or_else(|| payload.get("check_run"))
        .and_then(|value| value.get("html_url"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn normalized_payload(
    provider: &str,
    external_id: &str,
    title: String,
    url: Option<String>,
    extra: Option<Value>,
) -> Value {
    let mut payload = json!({
        "provider": provider,
        "external_id": external_id,
        "title": truncate(&title, 2048),
        "url": url,
    });
    if let Some(extra) = extra {
        payload["details"] = extra;
    }
    let serialized = payload.to_string();
    if serialized.len() > MAX_PAYLOAD_BYTES {
        payload["details"] = json!({"summary_sha256": hex_hash(&serialized)});
    }
    payload
}

fn parse_feed_items(body: &str) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for chunk in body
        .split("<item>")
        .skip(1)
        .chain(body.split("<entry>").skip(1))
    {
        let title = tag(chunk, "title").unwrap_or_default();
        let url = tag(chunk, "link");
        let id = tag(chunk, "guid")
            .or_else(|| tag(chunk, "id"))
            .or_else(|| url.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| hex_hash(&format!("{title}\n{}", url.as_deref().unwrap_or(""))));
        if id.is_empty() {
            continue;
        }
        items.push(FeedItem { id, title, url });
    }
    items
}

struct FeedItem {
    id: String,
    title: String,
    url: Option<String>,
}

fn tag(value: &str, name: &str) -> Option<String> {
    let start = value.find(&format!("<{name}"))?;
    let after = &value[start..];
    let open = after.find('>')? + 1;
    let end = after[open..].find(&format!("</{name}>"))?;
    Some(html_unescape(after[open..open + end].trim()))
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn hex_hash(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn sanitize_error(error: &str) -> String {
    let mut output = error.to_owned();
    for marker in ["token=", "access_token=", "api_key=", "authorization="] {
        if let Some(start) = output.to_ascii_lowercase().find(marker) {
            let value_start = start + marker.len();
            let end = output[value_start..]
                .find(['&', ' ', '\n'])
                .map_or(output.len(), |offset| value_start + offset);
            output.replace_range(value_start..end, "[redacted]");
            break;
        }
    }
    output.chars().take(1024).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn errors_redact_query_credentials() {
        let error = sanitize_error("request https://example.test/feed?token=secret&x=1 failed");
        assert!(!error.contains("secret"));
        assert!(error.contains("[redacted]"));
    }

    #[tokio::test]
    async fn rss_baseline_and_dedup_work_over_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                let body = r#"<rss><channel><item><guid>item-1</guid><title>Hello</title><link>https://example.test/1</link></item></channel></rss>"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .save_external_ingress_source(
                "feed:test",
                "rss",
                &json!({"url":format!("http://{address}/feed.xml")}),
            )
            .unwrap();
        store
            .set_external_ingress_enabled("feed:test", true)
            .unwrap();
        let secret_path =
            std::env::temp_dir().join(format!("opcos-ingress-{}", uuid::Uuid::new_v4()));
        let secrets = KeyringSecretStore::with_fallback("opcos-test", &secret_path);
        poll_once(&store, &secrets, "feed:test").await.unwrap();
        assert!(store.load_events_after("test", 10).unwrap().is_empty());
        poll_once(&store, &secrets, "feed:test").await.unwrap();
        assert_eq!(store.load_events_after("test", 10).unwrap().len(), 1);
        let rule = store
            .create_event_rule(
                "external.rss.*",
                "enqueue_work",
                &json!({"task_type":"process_feed","payload":{}}),
                10,
                3600,
                3,
            )
            .unwrap();
        let event = store
            .load_events_after("rule-test", 10)
            .unwrap()
            .pop()
            .unwrap();
        let dispatch = opcos_engine::event_bus::dispatch_event(&store, &event, &rule).unwrap();
        assert!(matches!(
            dispatch.effect,
            opcos_engine::event_bus::EventEffect::Enqueue(_)
        ));
        assert_eq!(store.load_work_queue(Some("ready"), 10).unwrap().len(), 1);
        server.join().unwrap();
        let _ = std::fs::remove_file(secret_path);
    }
}
