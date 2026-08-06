use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use opcos_hosts::{BrowserController, BrowserRequest, HostError};
use reqwest::Client;
use serde_json::{Value, json};
use std::{env, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

struct BrowserSession {
    child: Child,
    _profile: TempDir,
    ws_url: String,
    next_id: u64,
    current_url: String,
}

pub struct LocalBrowser {
    binary: Option<PathBuf>,
    session: Mutex<Option<BrowserSession>>,
    http: Client,
}

impl LocalBrowser {
    pub fn new(binary: Option<PathBuf>) -> Self {
        Self {
            binary: binary.or_else(|| env::var_os("OPCOS_BROWSER_BINARY").map(PathBuf::from)),
            session: Mutex::new(None),
            http: Client::new(),
        }
    }

    fn discover_binary(&self) -> Result<PathBuf, HostError> {
        if let Some(path) = &self.binary {
            if path.is_file() {
                return Ok(path.clone());
            }
            return Err(HostError::Unsupported(format!(
                "configured browser binary does not exist: {}",
                path.display()
            )));
        }
        let mut candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
            [
                env::var_os("PROGRAMFILES").map(PathBuf::from),
                env::var_os("PROGRAMFILES(X86)").map(PathBuf::from),
                env::var_os("LOCALAPPDATA").map(PathBuf::from),
            ]
            .into_iter()
            .flatten()
            .flat_map(|root| {
                [
                    root.join("Google/Chrome/Application/chrome.exe"),
                    root.join("Chromium/Application/chrome.exe"),
                ]
            })
            .collect()
        } else if cfg!(target_os = "macos") {
            vec![
                PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
                PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
            ]
        } else {
            vec![
                PathBuf::from("/usr/bin/google-chrome"),
                PathBuf::from("/usr/bin/google-chrome-stable"),
                PathBuf::from("/usr/bin/chromium"),
                PathBuf::from("/usr/bin/chromium-browser"),
            ]
        };
        let names: Vec<&str> = if cfg!(target_os = "windows") {
            vec!["chrome.exe", "chromium.exe"]
        } else if cfg!(target_os = "macos") {
            vec!["google-chrome", "chromium"]
        } else {
            vec![
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
            ]
        };
        for directory in env::var_os("PATH")
            .into_iter()
            .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
        {
            candidates.extend(names.iter().map(|name| directory.join(name)));
        }
        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            for root in [
                home.join(".cache/ms-playwright"),
                home.join(".cache/puppeteer"),
            ] {
                if let Ok(versions) = std::fs::read_dir(root) {
                    for version in versions.flatten() {
                        for child in ["chrome-linux/chrome", "chrome-linux64/chrome"] {
                            candidates.push(version.path().join(child));
                        }
                    }
                }
            }
        }
        candidates
            .into_iter()
            .find(|path| {
                path.is_file()
                    && std::process::Command::new(path)
                        .arg("--version")
                        .output()
                        .is_ok_and(|output| {
                            output.status.success()
                                && String::from_utf8_lossy(&output.stdout)
                                    .to_ascii_lowercase()
                                    .contains("chrom")
                        })
            })
            .ok_or_else(|| {
                HostError::Unsupported(
                    "no system Chrome/Chromium discovered; set OPCOS_BROWSER_BINARY to an executable path"
                        .into(),
                )
            })
    }

    async fn ensure_session(&self) -> Result<(), HostError> {
        let mut guard = self.session.lock().await;
        if guard
            .as_ref()
            .is_some_and(|session| session.child.id().is_some() && !session.ws_url.is_empty())
        {
            return Ok(());
        }
        if let Some(mut old) = guard.take() {
            let _ = old.child.start_kill();
        }
        let binary = self.discover_binary()?;
        let profile = tempfile::tempdir().map_err(HostError::Io)?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(HostError::Io)?;
        let port = listener.local_addr().map_err(HostError::Io)?.port();
        drop(listener);
        let child = Command::new(binary)
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-allow-origins=*")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-popup-blocking")
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("about:blank")
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(HostError::Io)?;
        let version_url = format!("http://127.0.0.1:{port}/json/version");
        let mut ws_url = None;
        for _ in 0..40 {
            if let Ok(response) = self.http.get(&version_url).send().await
                && let Ok(value) = response.json::<Value>().await
            {
                ws_url = value
                    .get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if ws_url.is_some() {
                    break;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
        let ws_url = ws_url.ok_or_else(|| {
            HostError::Unsupported("Chrome started but CDP endpoint did not become ready".into())
        })?;
        let target = self
            .http
            .put(format!("http://127.0.0.1:{port}/json/new?about:blank"))
            .send()
            .await
            .map_err(|error| HostError::InvalidResponse(error.to_string()))?
            .json::<Value>()
            .await
            .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
        let target_ws = target
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| HostError::InvalidResponse("CDP target has no websocket URL".into()))?;
        *guard = Some(BrowserSession {
            child,
            _profile: profile,
            ws_url: target_ws.to_owned(),
            next_id: 0,
            current_url: "about:blank".into(),
        });
        let _ = ws_url;
        Ok(())
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value, HostError> {
        self.ensure_session().await?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| HostError::Unsupported("browser session is unavailable".into()))?;
        session.next_id += 1;
        let id = session.next_id;
        let ws_url = session.ws_url.clone();
        drop(guard);
        let (mut socket, _) = connect_async(ws_url)
            .await
            .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
        socket
            .send(Message::Text(
                json!({"id": id, "method": method, "params": params})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
        while let Some(message) = socket.next().await {
            let message = message.map_err(|error| HostError::InvalidResponse(error.to_string()))?;
            if let Message::Text(text) = message {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(error) = value.get("error") {
                        return Err(HostError::InvalidResponse(error.to_string()));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
        Err(HostError::InvalidResponse("CDP websocket closed".into()))
    }

    fn safe_url(value: &str) -> Result<&str, HostError> {
        let url = Url::parse(value)
            .map_err(|error| HostError::InvalidResponse(format!("invalid browser URL: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HostError::Unsupported(
                "browser navigation only permits http and https URLs".into(),
            ));
        }
        Ok(value)
    }

    async fn evaluate(&self, expression: &str) -> Result<Value, HostError> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            return Err(HostError::InvalidResponse(exception.to_string()));
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn operation(&self, request: BrowserRequest) -> Result<Value, HostError> {
        match request.operation.as_str() {
            "status" => {
                self.ensure_session().await?;
                Ok(json!({"ready": true, "isolated_profile": true}))
            }
            "navigate" => {
                let url = request
                    .arguments
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| HostError::InvalidResponse("missing url".into()))?;
                Self::safe_url(url)?;
                self.command("Page.navigate", json!({"url": url})).await?;
                sleep(Duration::from_millis(250)).await;
                if let Some(session) = self.session.lock().await.as_mut() {
                    session.current_url = url.to_owned();
                }
                Ok(json!({"url": url}))
            }
            "set_viewport" => {
                let width = request
                    .arguments
                    .get("width")
                    .and_then(Value::as_u64)
                    .unwrap_or(1280);
                let height = request
                    .arguments
                    .get("height")
                    .and_then(Value::as_u64)
                    .unwrap_or(720);
                self.command(
                    "Emulation.setDeviceMetricsOverride",
                    json!({
                        "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false
                    }),
                )
                .await?;
                Ok(json!({"width": width, "height": height}))
            }
            "click" => {
                let selector = request.arguments.get("selector").and_then(Value::as_str);
                let role = request.arguments.get("role").and_then(Value::as_str);
                let text = request.arguments.get("text").and_then(Value::as_str);
                if selector.is_none() && role.is_none() && text.is_none() {
                    return Err(HostError::InvalidResponse(
                        "click requires selector, role, or text".into(),
                    ));
                }
                let expression = format!(
                    "(() => {{ const nodes=[...document.querySelectorAll('*')]; const n={}; if(!n) return false; n.click(); return true; }})()",
                    selector
                        .map(|value| format!(
                            "document.querySelector({})",
                            serde_json::to_string(value).unwrap_or_default()
                        ))
                        .or_else(|| role.map(|value| format!(
                            "nodes.find(n=>n.getAttribute('role')==={})",
                            serde_json::to_string(value).unwrap_or_default()
                        )))
                        .or_else(|| text.map(|value| format!(
                            "nodes.find(n=>n.innerText?.trim()==={})",
                            serde_json::to_string(value).unwrap_or_default()
                        )))
                        .unwrap_or_else(|| "null".into())
                );
                Ok(json!({"clicked": self.evaluate(&expression).await?}))
            }
            "read" => {
                let selector = request
                    .arguments
                    .get("selector")
                    .and_then(Value::as_str)
                    .unwrap_or("body");
                let expression = format!(
                    "(() => {{ const n=document.querySelector({}); return {{text:n?.innerText||'', html:n?.outerHTML||''}}; }})()",
                    serde_json::to_string(selector).unwrap_or_default()
                );
                self.evaluate(&expression).await
            }
            "measure" => {
                let selector = request
                    .arguments
                    .get("selector")
                    .and_then(Value::as_str)
                    .ok_or_else(|| HostError::InvalidResponse("missing selector".into()))?;
                let expression = format!(
                    "(() => {{ const n=document.querySelector({}); if(!n) return null; const r=n.getBoundingClientRect(),s=getComputedStyle(n); return {{left:r.left,top:r.top,width:r.width,height:r.height,right:r.right,bottom:r.bottom,marginLeft:s.marginLeft,marginRight:s.marginRight}}; }})()",
                    serde_json::to_string(selector).unwrap_or_default()
                );
                self.evaluate(&expression).await
            }
            "assert_geometry" => {
                let first = request
                    .arguments
                    .get("first")
                    .and_then(Value::as_str)
                    .ok_or_else(|| HostError::InvalidResponse("missing first selector".into()))?;
                let second = request.arguments.get("second").and_then(Value::as_str);
                let container = request.arguments.get("container").and_then(Value::as_str);
                let expression = format!(
                    "(() => {{ const a=document.querySelector({}); if(!a) return {{ok:false,reason:'first element not found'}}; const ar=a.getBoundingClientRect(); const result={{ok:true,firstWidth:ar.width,firstRight:ar.right}}; {} {} result.ok=!result.firstTooWide&&!result.overlap; return result; }})()",
                    serde_json::to_string(first).unwrap_or_default(),
                    container.map(|value| format!(
                        "const c=document.querySelector({}); if(!c) return {{ok:false,reason:'container not found'}}; const cr=c.getBoundingClientRect(); result.firstTooWide=ar.left<cr.left||ar.right>cr.right;",
                        serde_json::to_string(value).unwrap_or_default()
                    )).unwrap_or_else(|| "result.firstTooWide=false;".into()),
                    second.map(|value| format!(
                        "const b=document.querySelector({}); if(!b) return {{ok:false,reason:'second element not found'}}; const br=b.getBoundingClientRect(); result.overlap=!(ar.right<=br.left||br.right<=ar.left||ar.bottom<=br.top||br.bottom<=ar.top); result.ok=!result.overlap; result.secondWidth=br.width; result.secondRight=br.right;",
                        serde_json::to_string(value).unwrap_or_default()
                    )).unwrap_or_else(|| "result.overlap=false;".into())
                );
                self.evaluate(&expression).await
            }
            "screenshot" => {
                let result = self
                    .command("Page.captureScreenshot", json!({"format":"png"}))
                    .await?;
                Ok(
                    json!({"format":"png","image":result.get("data").cloned().unwrap_or(Value::Null)}),
                )
            }
            other => Err(HostError::Unsupported(format!(
                "unknown browser operation: {other}"
            ))),
        }
    }
}

impl Drop for LocalBrowser {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.session.try_lock()
            && let Some(mut session) = guard.take()
        {
            let _ = session.child.start_kill();
        }
    }
}

#[async_trait]
impl BrowserController for LocalBrowser {
    async fn execute(&self, request: BrowserRequest) -> Result<Value, HostError> {
        self.operation(request).await
    }

    async fn current_origin(&self) -> Option<String> {
        let guard = self.session.lock().await;
        let url = Url::parse(&guard.as_ref()?.current_url).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        Some(url.origin().ascii_serialization())
    }
}

pub fn shared_local_browser(binary: Option<PathBuf>) -> Arc<dyn BrowserController> {
    Arc::new(LocalBrowser::new(binary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcos_hosts::BrowserController;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn explicit_missing_binary_is_reported() {
        let browser = LocalBrowser::new(Some(PathBuf::from("/definitely/missing/chrome")));
        let error = browser.discover_binary().unwrap_err().to_string();
        assert!(error.contains("configured browser binary does not exist"));
    }

    #[tokio::test]
    async fn fixture_geometry_assertions_catch_width_and_overlap() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let body = r#"<!doctype html><style>
              #container{width:320px;height:500px;position:relative}
              #wide{width:430px;height:20px}
              #art{position:absolute;left:100px;top:0;width:120px;height:120px}
              #hero{position:absolute;left:110px;top:10px;width:100px;height:20px}
            </style><main id="container"><div id="wide"></div><div id="art"></div><div id="hero"></div></main>"#;
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let browser = LocalBrowser::new(None);
        browser
            .execute(BrowserRequest {
                operation: "navigate".into(),
                arguments: json!({"url": format!("http://127.0.0.1:{port}/fixture")}),
            })
            .await
            .unwrap();
        browser
            .execute(BrowserRequest {
                operation: "set_viewport".into(),
                arguments: json!({"width": 430, "height": 720}),
            })
            .await
            .unwrap();
        let result = browser
            .execute(BrowserRequest {
                operation: "assert_geometry".into(),
                arguments: json!({"first":"#hero","second":"#art","container":"#container"}),
            })
            .await
            .unwrap();
        assert_eq!(result["overlap"], true);
        assert_eq!(result["firstTooWide"], false);
        assert_eq!(result["ok"], false);

        let width = browser
            .execute(BrowserRequest {
                operation: "assert_geometry".into(),
                arguments: json!({"first":"#wide","container":"#container"}),
            })
            .await
            .unwrap();
        assert_eq!(width["firstTooWide"], true);
        assert_eq!(width["ok"], false);
    }
}
