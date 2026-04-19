use std::{marker::PhantomData, net::SocketAddr};

use bon::Builder;
use clap::Args;
use clap::Parser;
use derive_where::derive_where;
pub use rust_embed::Embed as LoadAssets;

use eyre::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::fs;

use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;

pub use axum_client_ip::ClientIp;
pub use tower_http::cors::CorsLayer as Cors;

#[cfg(feature = "store")]
pub mod store;

#[cfg(feature = "cookies")]
pub mod cookies;

#[cfg(feature = "session")]
pub mod session;
#[cfg(feature = "session")]
pub use crate::session::{SessionSettings, SessionState};

#[cfg(feature = "database")]
pub use toasty;
#[cfg(feature = "database")]
pub use utils::database::get as database;

#[cfg(feature = "htmx")]
pub use axum_htmx as htmx;

mod utils;
pub use utils::{errors, scheduler};

// Config

#[derive(Parser)]
struct Cli<S: WebServer> {
    /// configuration file (cli options take precedence)
    #[clap(short, long)]
    config_file: Option<String>,
    #[clap(flatten)]
    config: Config<S>,
}

#[derive(Serialize, Deserialize, Builder, Args)]
#[serde(default)]
/// Runtime configuration
struct BuiltInConfig {
    #[clap(long)]
    #[clap(default_value = "localhost")]
    #[builder(default = "localhost".into())]
    host: String,

    #[clap(default_value = "8080")]
    #[builder(default = 8080)]
    #[clap(short, long)]
    port: u16,

    #[clap(long)]
    reverse_proxy: Option<String>,

    #[cfg(feature = "database")]
    #[clap(long)]
    db: Option<String>,

    #[cfg(feature = "session")]
    #[clap(long)]
    session_key_file: Option<String>,
}

impl Default for BuiltInConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

#[allow(private_interfaces)]
pub type ConfigOverride = BuiltInConfig;

#[derive(Serialize, Deserialize, Default, Args)]
struct Config<S: Args> {
    #[serde(flatten)]
    #[clap(flatten)]
    built_in: BuiltInConfig,

    #[serde(flatten)]
    #[clap(flatten)]
    user_defined: Option<S>,
}

async fn load_config<S: WebServer>() -> Result<Config<S>> {
    let cli = Cli::<S>::parse();

    let from_cli = cli.config;

    // Only override built-ins, since user_defined overrides
    // are just... defaults
    let from_compile_time = S::config_override().map(|v| Config {
        user_defined: None,
        built_in: v,
    });

    let from_file: Option<Config<S>> = match cli.config_file {
        None => None,
        Some(path) => async {
            let data = fs::read_to_string(&path).await?;
            serde_json::from_str(&data).context("Failed to deserialize")
        }
        .await
        .wrap_err_with(|| format!("Failed to read config from {path:?}"))?,
    };

    merge!(from_file, from_cli, from_compile_time)
}

// State

#[derive_where(Clone)]
#[derive(Builder)]
pub struct ServerState<T: WebServer> {
    #[builder(skip = PhantomData)]
    _never_empty: PhantomData<T>,

    #[cfg(feature = "database")]
    db: toasty::Db,
    #[cfg(feature = "session")]
    session_state: SessionState<T::SessionData>,
}

#[allow(unused)]
use axum::extract::FromRef;

#[cfg(feature = "database")]
impl<T: WebServer> FromRef<ServerState<T>> for toasty::Db {
    fn from_ref(input: &ServerState<T>) -> Self {
        input.db.clone()
    }
}

#[cfg(feature = "session")]
impl<T: WebServer> FromRef<ServerState<T>> for SessionState<T::SessionData> {
    fn from_ref(input: &ServerState<T>) -> Self {
        input.session_state.clone()
    }
}

// Main

pub type Router<S> = axum::Router<ServerState<S>>;

#[allow(async_fn_in_trait)]
pub trait WebServer: Args + Serialize + DeserializeOwned + Default + Send + Sync + 'static {
    #[cfg(feature = "session")]
    type SessionData: store::Value;

    #[cfg(feature = "session")]
    fn session_settings() -> SessionSettings {
        SessionSettings::builder().build()
    }

    #[cfg(feature = "htmx")]
    /// Single Page Application mode:
    /// will redirect any non-htmx requests to "/"
    const SPA: bool = false;

    fn cors() -> Option<Cors> {
        None
    }

    // TODO: better asset handling
    fn assets() -> impl LoadAssets;

    #[allow(private_interfaces)]
    fn config_override() -> Option<ConfigOverride> {
        None
    }

    async fn init(self) -> Result<Router<Self>>;
}

pub async fn run_server<Server: WebServer>() -> Result<()> {
    simple_eyre::install()?;
    tracing_subscriber::fmt::init();

    let config = load_config::<Server>()
        .await
        .context("Failed to load config")?;

    let (listener, ip_source) = utils::network::setup_network(&config.built_in)
        .await
        .context("Failed to set up network")?;

    let router = Server::init(config.user_defined.unwrap_or_default()).await?;

    let state = ServerState::<Server>::builder();

    let middleware = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(ip_source.into_extension())
        .option_layer(Server::cors());

    #[cfg(feature = "compression")]
    let middleware = middleware.layer(
        tower_http::compression::CompressionLayer::new()
            .gzip(true)
            .deflate(true)
            .br(true)
            .zstd(true),
    );

    #[cfg(feature = "database")]
    let state = {
        utils::database::setup(&config.built_in).await?;
        state.db(database())
    };

    #[cfg(feature = "cookies")]
    let middleware = middleware.layer(tower_cookies::CookieManagerLayer::new());

    #[cfg(feature = "session")]
    let state = { state.session_state(session::setup_sessions::<Server>(&config.built_in)?) };

    #[cfg(feature = "htmx")]
    let middleware = middleware
        .layer(axum_htmx::AutoVaryLayer)
        .option_layer(Server::SPA.then_some(axum_htmx::HxRequestGuardLayer::new("/")));

    let router = router
        .fallback_service(utils::assets::ServeAssets::from(Server::assets()))
        .layer(middleware)
        .with_state(state.build());

    let service = router.into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, service)
        .with_graceful_shutdown(utils::shutdown::signal())
        .await?;

    Ok(())
}

#[macro_export]
macro_rules! assets {
    ($folder:literal) => {
        fn assets() -> impl $crate::LoadAssets {
            #[derive($crate::LoadAssets)]
            #[folder = $folder]
            struct Assets;
            Assets
        }
    };
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
