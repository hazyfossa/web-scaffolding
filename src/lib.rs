use std::{net::SocketAddr, path::PathBuf};

pub use axum_client_ip::ClientIp;
pub use rust_embed::Embed as LoadAssets;

use axum_client_ip::ClientIpSource;
use eyre::{Context, Result};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{fs, net::TcpListener};

use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;

#[cfg(feature = "compression")]
use tower_http::compression::CompressionLayer;

mod utils;
pub use crate::utils::assets::ServeAssets as LoadedAssets;
pub use utils::{errors, scheduler};

async fn setup_network(config: &BuiltInConfig) -> Result<(TcpListener, ClientIpSource)> {
    let addr = format!("{}:{}", config.host, config.port);

    let listener = TcpListener::bind(&addr)
        .await
        .context("Failed to bind listener")?;

    tracing::info!("Listening on http://{addr}");

    let ip_source = match &config.reverse_proxy {
        None => ClientIpSource::ConnectInfo,
        Some(proxy) => match proxy.as_str() {
            "nginx" => ClientIpSource::XRealIp,
            "cloudflare" => ClientIpSource::CfConnectingIp,
            "cloudfront" => ClientIpSource::CloudFrontViewerAddress,
            "flyio" => ClientIpSource::FlyClientIp,
            "akamai" => ClientIpSource::TrueClientIp,
            "envoy" => ClientIpSource::XEnvoyExternalAddress,
            other => {
                tracing::info!(
                    "Expecting {other} reverse-proxy to provide X-Forwarded-For headers"
                );
                ClientIpSource::RightmostXForwardedFor
            }
        },
    };

    Ok((listener, ip_source))
}

#[derive(Deserialize)]
#[serde(default)]
struct BuiltInConfig {
    host: String,
    port: u16,
    reverse_proxy: Option<String>,
}

impl Default for BuiltInConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 8080,
            reverse_proxy: None,
        }
    }
}

#[derive(Deserialize)]
struct WithBuiltinConfig<T> {
    #[serde(flatten)]
    built_in: BuiltInConfig,
    #[serde(flatten)]
    user_defined: T,
}

pub async fn load_config<T: DeserializeOwned>() -> Result<T> {
    let mut args = pico_args::Arguments::from_env();

    const DEFAULT_PATH: &str = "./config.toml";

    let path: PathBuf = args
        .opt_value_from_str(["--config", "-c"])
        .context("Failed to parse cli argument: --config")?
        .unwrap_or(DEFAULT_PATH.into());

    let mut config = fs::read_to_string(&path)
        .await
        .wrap_err_with(|| format!("Failed to read from {path:?}"));

    if *path == *DEFAULT_PATH {
        config = config.context("You can change the configuration path with '-c' or '--config'")
    }

    serde_json::from_str(&config?).context("Failed to parse")
}

pub async fn run_server<Server: WebServer>() -> Result<()> {
    simple_eyre::install()?;
    tracing_subscriber::fmt::init();

    let config = load_config::<WithBuiltinConfig<Server>>()
        .await
        .context("Failed to load config")?;

    let (listener, ip_source) = setup_network(&config.built_in)
        .await
        .context("Failed to set up network")?;

    let router = Server::init(config.user_defined).await?;

    let middleware = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(ip_source.into_extension());

    #[cfg(feature = "compression")]
    let middleware = middleware.layer(
        CompressionLayer::new()
            .gzip(true)
            .deflate(true)
            .br(true)
            .zstd(true),
    );

    let router = router
        .fallback_service(Server::assets().into())
        .layer(middleware);

    let service = router.into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, service)
        .with_graceful_shutdown(utils::shutdown::signal())
        .await?;

    Ok(())
}

#[allow(async_fn_in_trait)]
pub trait WebServer: DeserializeOwned {
    // TODO: better asset handling
    fn assets() -> impl Into<LoadedAssets>;

    async fn init(self) -> Result<axum::Router>;
}

#[macro_export]
macro_rules! run {
    ($server:ident) => {
        #[tokio::main]
        async fn main() -> Result<()> {
            $crate::run_server::<$server>().await
        }
    };
}
