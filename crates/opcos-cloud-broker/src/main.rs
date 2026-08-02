use opcos_cloud_broker::{BrokerConfig, ProviderConfig, run};
use std::{collections::HashMap, env, net::SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind: SocketAddr = env::var("OPCOS_BROKER_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()?;
    let public_base_url = required("OPCOS_BROKER_PUBLIC_BASE_URL")?;
    let provider = env::var("OPCOS_BROKER_PROVIDER").unwrap_or_else(|_| "linear".into());
    let config = BrokerConfig {
        public_base_url,
        callback_path: "/oauth/callback".into(),
        providers: HashMap::from([(
            provider,
            ProviderConfig {
                client_id: required("OPCOS_OAUTH_CLIENT_ID")?,
                client_secret: required("OPCOS_OAUTH_CLIENT_SECRET")?,
                authorize_url: required("OPCOS_OAUTH_AUTHORIZE_URL")?,
                token_url: required("OPCOS_OAUTH_TOKEN_URL")?,
                scopes: env::var("OPCOS_OAUTH_SCOPES")
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            },
        )]),
    };
    run(config, bind).await?;
    Ok(())
}

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("{name} is required").into())
}
