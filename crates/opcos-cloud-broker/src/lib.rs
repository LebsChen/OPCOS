use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use getrandom::fill as random_fill;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::Mutex};
use url::Url;

const SESSION_TTL_SECONDS: i64 = 300;

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub client_id: String,
    pub client_secret: String,
    pub authorize_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub public_base_url: String,
    pub callback_path: String,
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Clone)]
pub struct BrokerState {
    config: Arc<BrokerConfig>,
    sessions: Arc<Mutex<HashMap<String, PendingSession>>>,
    http: Client,
}

#[derive(Clone, Debug)]
struct PendingSession {
    provider: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
    expires_at: DateTime<Utc>,
    authorization_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub provider: String,
    pub redirect_uri: String,
    pub code_challenge: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StartResponse {
    pub session_code: String,
    pub authorize_url: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct PollResponse {
    status: &'static str,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

pub fn router(config: BrokerConfig) -> Router {
    let state = BrokerState {
        config: Arc::new(config),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        http: Client::new(),
    };
    Router::new()
        .route("/v1/oauth/sessions", post(start_session))
        .route("/v1/oauth/sessions/{session_code}", get(poll_session))
        .route("/oauth/callback", get(oauth_callback))
        .with_state(state)
}

pub async fn serve(listener: TcpListener, config: BrokerConfig) -> Result<(), std::io::Error> {
    axum::serve(listener, router(config)).await
}

pub async fn run(config: BrokerConfig, bind: SocketAddr) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(bind).await?;
    serve(listener, config).await
}

async fn start_session(
    State(state): State<BrokerState>,
    Json(input): Json<StartRequest>,
) -> Result<Json<StartResponse>, BrokerError> {
    let provider = state
        .config
        .providers
        .get(&input.provider)
        .ok_or_else(|| BrokerError::bad_request("provider is not configured"))?;
    if input.redirect_uri.trim().is_empty()
        || input.code_challenge.trim().is_empty()
        || !input.redirect_uri.starts_with("http")
    {
        return Err(BrokerError::bad_request(
            "redirect_uri and code_challenge are required",
        ));
    }
    let session_code = random_hex(32)?;
    let state_value = random_hex(32)?;
    let expires_at = Utc::now() + Duration::seconds(SESSION_TTL_SECONDS);
    let callback_url = format!(
        "{}{}",
        state.config.public_base_url.trim_end_matches('/'),
        state.config.callback_path
    );
    let mut authorize = Url::parse(&provider.authorize_url)
        .map_err(|_| BrokerError::bad_request("provider authorize URL is invalid"))?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &provider.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &callback_url)
        .append_pair("state", &state_value)
        .append_pair("code_challenge", &input.code_challenge)
        .append_pair("code_challenge_method", "S256");
    if !provider.scopes.is_empty() {
        authorize
            .query_pairs_mut()
            .append_pair("scope", &provider.scopes.join(" "));
    }
    state.sessions.lock().await.insert(
        session_code.clone(),
        PendingSession {
            provider: input.provider,
            redirect_uri: input.redirect_uri,
            code_challenge: input.code_challenge,
            state: state_value,
            expires_at,
            authorization_code: None,
        },
    );
    Ok(Json(StartResponse {
        session_code,
        authorize_url: authorize.to_string(),
        expires_at,
    }))
}

async fn oauth_callback(
    State(state): State<BrokerState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(callback_state) = query.state else {
        return BrokerError::bad_request("missing OAuth state").into_response();
    };
    let mut sessions = state.sessions.lock().await;
    let Some((_, pending)) = sessions.iter_mut().find(|(_, session)| {
        session
            .state
            .as_bytes()
            .ct_eq(callback_state.as_bytes())
            .into()
    }) else {
        return BrokerError::unauthorized("invalid OAuth state").into_response();
    };
    if pending.expires_at <= Utc::now() {
        return BrokerError::gone("OAuth session expired").into_response();
    }
    if query.error.is_some() {
        return BrokerError::bad_request("OAuth authorization failed").into_response();
    }
    let Some(code) = query.code else {
        return BrokerError::bad_request("missing OAuth code").into_response();
    };
    pending.authorization_code = Some(code);
    Html("<html><body>Authorization received. You can return to OPCOS.</body></html>")
        .into_response()
}

async fn poll_session(
    State(state): State<BrokerState>,
    axum::extract::Path(session_code): axum::extract::Path<String>,
    Query(query): Query<PollQuery>,
) -> Result<Response, BrokerError> {
    let verifier = query
        .code_verifier
        .ok_or_else(|| BrokerError::bad_request("missing PKCE code_verifier"))?;
    let (pending, provider) = {
        let sessions = state.sessions.lock().await;
        let key = sessions
            .keys()
            .find(|key| key.as_bytes().ct_eq(session_code.as_bytes()).into())
            .cloned()
            .ok_or_else(|| BrokerError::not_found("OAuth session not found"))?;
        let pending = sessions
            .get(&key)
            .cloned()
            .ok_or_else(|| BrokerError::not_found("OAuth session not found"))?;
        let provider = state
            .config
            .providers
            .get(&pending.provider)
            .cloned()
            .ok_or_else(|| BrokerError::bad_request("provider is not configured"))?;
        (pending, provider)
    };
    if pending.expires_at <= Utc::now() {
        state
            .sessions
            .lock()
            .await
            .retain(|_, item| item.state != pending.state);
        return Err(BrokerError::gone("OAuth session expired"));
    }
    let expected_challenge = pkce_challenge(&verifier);
    if !bool::from(
        expected_challenge
            .as_bytes()
            .ct_eq(pending.code_challenge.as_bytes()),
    ) {
        return Err(BrokerError::unauthorized("PKCE verification failed"));
    }
    let Some(ref code) = pending.authorization_code else {
        return Ok((
            StatusCode::ACCEPTED,
            Json(PollResponse {
                status: "pending",
                expires_at: pending.expires_at,
            }),
        )
            .into_response());
    };
    let token = exchange_code(&state.http, &provider, code, &verifier, &pending).await?;
    let mut sessions = state.sessions.lock().await;
    sessions.retain(|_, item| item.state != pending.state);
    Ok(Json(token).into_response())
}

#[derive(Debug, Deserialize)]
struct PollQuery {
    code_verifier: Option<String>,
}

async fn exchange_code(
    http: &Client,
    provider: &ProviderConfig,
    code: &str,
    verifier: &str,
    pending: &PendingSession,
) -> Result<TokenResponse, BrokerError> {
    let response = http
        .post(&provider.token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", pending.redirect_uri.as_str()),
            ("client_id", provider.client_id.as_str()),
            ("client_secret", provider.client_secret.as_str()),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| BrokerError::bad_gateway("provider token exchange failed"))?;
    if !response.status().is_success() {
        return Err(BrokerError::bad_gateway("provider token exchange failed"));
    }
    response
        .json()
        .await
        .map_err(|_| BrokerError::bad_gateway("provider returned an invalid token response"))
}

fn random_hex(size: usize) -> Result<String, BrokerError> {
    let mut bytes = vec![0; size];
    random_fill(&mut bytes).map_err(|_| BrokerError::internal("OS randomness unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Debug)]
struct BrokerError {
    status: StatusCode,
    message: String,
}

impl BrokerError {
    fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }
    fn not_found(message: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    fn gone(message: &str) -> Self {
        Self {
            status: StatusCode::GONE,
            message: message.into(),
        }
    }
    fn bad_gateway(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
    fn internal(message: &str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for BrokerError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use reqwest::Client;
    use serde_json::json;
    use tokio::time::{Duration as TokioDuration, sleep};

    fn config(token_url: String, base_url: String) -> BrokerConfig {
        BrokerConfig {
            public_base_url: base_url,
            callback_path: "/oauth/callback".into(),
            providers: HashMap::from([(
                "test".into(),
                ProviderConfig {
                    client_id: "client-id".into(),
                    client_secret: "client-secret".into(),
                    authorize_url: "https://provider.invalid/authorize".into(),
                    token_url,
                    scopes: vec!["identity".into()],
                },
            )]),
        }
    }

    #[tokio::test]
    async fn loopback_oauth_flow_returns_token_once_and_never_twice() {
        let provider = Router::new().route(
            "/token",
            post(|| async {
                Json(json!({"access_token":"local-test-token","token_type":"Bearer"}))
            }),
        );
        let provider_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_base = format!("http://{}", provider_listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(provider_listener, provider).await.unwrap();
        });
        let broker_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let broker_addr = broker_listener.local_addr().unwrap();
        let base = format!("http://{broker_addr}");
        tokio::spawn(serve(
            broker_listener,
            config(format!("{provider_base}/token"), base.clone()),
        ));
        sleep(TokioDuration::from_millis(10)).await;
        let verifier = "a-strong-local-verifier";
        let start = Client::new()
            .post(format!("{base}/v1/oauth/sessions"))
            .json(&json!({
                "provider":"test",
                "redirect_uri":"http://127.0.0.1:9/local",
                "code_challenge":pkce_challenge(verifier)
            }))
            .send()
            .await
            .unwrap()
            .json::<StartResponse>()
            .await
            .unwrap();
        let authorize = Url::parse(&start.authorize_url).unwrap();
        let state = authorize
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let callback = Client::new()
            .get(format!(
                "{base}/oauth/callback?code=provider-code&state={state}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(callback.status(), StatusCode::OK);
        let token = Client::new()
            .get(format!(
                "{base}/v1/oauth/sessions/{}?code_verifier={verifier}",
                start.session_code
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(token.status(), StatusCode::OK);
        assert_eq!(
            token.json::<TokenResponse>().await.unwrap().access_token,
            "local-test-token"
        );
        let second = Client::new()
            .get(format!(
                "{base}/v1/oauth/sessions/{}?code_verifier={verifier}",
                start.session_code
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_FOUND);
    }
}
