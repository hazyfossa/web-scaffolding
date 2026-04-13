use std::{io::ErrorKind, net::SocketAddr, path::PathBuf};

pub use axum_client_ip::ClientIp;
pub use rust_embed::Embed as LoadAssets;

use axum_client_ip::ClientIpSource;
use eyre::{Context, Result, bail};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{fs, net::TcpListener};

use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;

#[cfg(feature = "compression")]
use tower_http::compression::CompressionLayer;

#[cfg(feature = "store")]
pub mod store;

#[cfg(feature = "database")]
pub use database::get as database;

// TODO: drop alias once toasty becomes
// properly importable as a crate
#[cfg(feature = "database")]
pub use toasty;

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

#[cfg(feature = "database")]
mod database {
    use std::sync::OnceLock;

    use super::*;

    // TODO: use axum state?
    static DB: OnceLock<toasty::Db> = OnceLock::new();

    pub async fn setup(config: &BuiltInConfig) -> Result<()> {
        let uri = config.db.as_deref().unwrap_or_else(|| {
            tracing::warn!("Using an in-memory database. Data will not be saved!");
            ":memory:"
        });

        let db = toasty::Db::builder()
            .connect(&uri)
            .await
            .context("Failed to connect to database")?;

        db.push_schema()
            .await
            .context("Failed to push schema to database")?;

        tracing::info!("Connected to database");

        DB.set(db).expect("Database already initialized");
        Ok(())
    }

    pub fn get() -> toasty::Db {
        DB.get().expect("Database not initialized").clone()
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct BuiltInConfig {
    host: String,
    port: u16,
    reverse_proxy: Option<String>,
    #[cfg(feature = "database")]
    db: Option<String>,
}

impl Default for BuiltInConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 8080,
            reverse_proxy: None,
            #[cfg(feature = "database")]
            db: None,
        }
    }
}

#[derive(Deserialize, Default)]
struct WithBuiltinConfig<T> {
    #[serde(flatten)]
    built_in: BuiltInConfig,
    #[serde(flatten)]
    user_defined: T,
}

pub async fn load_config<T: DeserializeOwned + Default>() -> Result<T> {
    let mut args = pico_args::Arguments::from_env();

    const DEFAULT_PATH: &str = "./config.json";

    let path: Option<PathBuf> = args
        .opt_value_from_str(["-c", "--config"])
        .context("Failed to parse cli argument: --config")?;

    let (path, is_default_path) =
        path.map_or((DEFAULT_PATH.into(), true), |custom| (custom, false));

    let file = match fs::read_to_string(&path).await {
        Ok(string) => string,
        Err(e) if is_default_path && e.kind() == ErrorKind::NotFound => {
            return Ok(T::default());
        }
        Err(e) => bail!(e),
    };

    serde_json::from_str(&file).context("Failed to parse")
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

    #[cfg(feature = "database")]
    database::setup(&config.built_in).await?;

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
pub trait WebServer: DeserializeOwned + Default {
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
