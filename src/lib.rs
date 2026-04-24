#![allow(private_interfaces, private_bounds)] // TODO: this is only used for ConfigOverride

use std::marker::PhantomData;

use bon::Builder;
use derive_where::derive_where;
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::Dispatch;
use tracing_subscriber::util::SubscriberInitExt;

mod network;
use network::{NetworkAddr, NetworkConfig, ReverseProxy};

mod utils;

pub use axum_client_ip::ClientIp;
pub use rust_embed::Embed as LoadAssets;
pub use tower_http::cors::CorsLayer as Cors;
pub use tracing;
pub use tracing_subscriber as tracing_settings;
pub use utils::{errors, scheduler};

#[cfg(feature = "store")]
pub mod store;

#[cfg(feature = "cookies")]
pub mod cookies;

#[cfg(feature = "session")]
pub mod session;
#[cfg(feature = "session")]
pub use crate::session::{SessionSettings, SessionState};

#[cfg(feature = "htmx")]
pub use axum_htmx as htmx;

#[cfg(feature = "tls")]
pub use axum_server::tls_rustls::RustlsConfig as RustlsSettings;

// Runtime config

#[cfg(feature = "cli")]
#[derive(clap::Parser)]
struct Cli<S: WebServer> {
    /// configuration file (cli options take precedence)
    #[clap(short, long)]
    config_file: Option<String>,
    #[clap(flatten)]
    config: Config<S>,
}

#[derive(Serialize, Deserialize, Builder)]
#[serde(default)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
struct RuntimeConfig {
    #[cfg_attr(feature = "cli", clap(flatten))]
    #[serde(flatten)]
    #[builder(default)]
    network: NetworkConfig,

    #[cfg_attr(feature = "cli", clap(long, default_value = "none"))]
    #[builder(default)]
    reverse_proxy: ReverseProxy,

    #[cfg(feature = "database")]
    #[cfg_attr(feature = "cli", clap(long))]
    db: Option<String>,

    #[cfg(feature = "session")]
    #[cfg_attr(feature = "cli", clap(long))]
    session_key_file: Option<std::path::PathBuf>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

pub type ConfigOverride = RuntimeConfig;

#[derive(Builder)]
#[derive_where(Default, Serialize, Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
struct Config<S: WebServer> {
    #[serde(flatten)]
    #[cfg_attr(feature = "cli", clap(flatten))]
    runtime: RuntimeConfig,

    #[cfg(not(feature = "config"))]
    #[builder(skip = PhantomData)]
    #[serde(skip)]
    #[cfg_attr(feature = "cli", clap(skip))]
    _s: PhantomData<S>,

    #[cfg(feature = "config")]
    #[serde(flatten)]
    #[cfg_attr(feature = "cli", clap(flatten))]
    user_defined: Option<S>,
}

async fn load_config<S: WebServer>() -> Result<(Settings, Config<S>)> {
    let mut settings = S::settings();

    #[cfg(feature = "cli")]
    let (config_file, from_cli) = {
        use clap::Parser;
        let cli = Cli::<S>::parse();
        (cli.config_file, Some(cli.config))
    };

    #[cfg(not(feature = "cli"))]
    let (config_file, from_cli) = {
        (
            std::env::var("WEB_CONFIG")
                .ok()
                .map(std::path::PathBuf::from),
            None,
        )
    };

    // Only override the built-in runtime config,
    // since user_defined overrides are just... defaults
    let from_compile_time = settings
        .config_override
        .take()
        .map(|v| Config::builder().runtime(v).build());

    let from_file: Option<Config<S>> = match config_file {
        None => None,
        Some(path) => async {
            let data = fs::read_to_string(&path).await?;
            serde_json::from_str(&data).context("Failed to deserialize")
        }
        .await
        .wrap_err_with(|| format!("Failed to read config from {path:?}"))?,
    };

    let config = merge!(from_file, from_cli, from_compile_time)?;
    Ok((settings, config))
}

// Settings (compile-time config)
#[derive(Builder, Default)]
pub struct Settings {
    config_override: Option<ConfigOverride>,

    #[builder(into)]
    tracing: Option<Dispatch>,

    cors: Option<Cors>,

    #[cfg(feature = "session")]
    session: Option<SessionSettings>,

    #[cfg(feature = "tls")]
    rustls: Option<RustlsSettings>,
}

// State

#[derive_where(Clone)]
#[derive(Builder)]
#[builder(builder_type(vis = ""))]
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

// Loaders

// TODO: these can be simplified once cfg attrs are supporten on `where` bounds

#[allow(unused)]
use serde::de::DeserializeOwned;

#[cfg(not(feature = "config"))]
trait_alias!(trait WebServerLoad: Default);

#[cfg(all(feature = "config", not(feature = "cli")))]
trait_alias!(trait WebServerLoad: Serialize + DeserializeOwned + Default);

#[cfg(all(feature = "config", feature = "cli"))]
trait_alias!(trait WebServerLoad: Serialize + DeserializeOwned + Default + clap::Args);

// Main

pub type Router<S> = axum::Router<ServerState<S>>;

#[allow(async_fn_in_trait)]
pub trait WebServer: WebServerLoad + Send + Sync + 'static + Sized {
    async fn init(self) -> Result<Router<Self>>;

    fn settings() -> Settings {
        Settings::default()
    }

    // TODO: better asset handling
    fn assets() -> impl LoadAssets;

    // TODO: support custom Connection

    #[cfg(feature = "session")]
    type SessionData: store::Value;

    #[cfg(feature = "database")]
    fn db_models() -> toasty::ModelSet;
}

pub async fn run_server<Server: WebServer>() -> Result<()> {
    simple_eyre::install()?;

    let (mut settings, config) = load_config::<Server>()
        .await
        .context("Failed to load config")?;

    if let Some(custom_tracing) = settings.tracing.take() {
        custom_tracing.init();
    } else {
        tracing_subscriber::fmt::init();
    };

    #[cfg(feature = "config")]
    let user_config = config.user_defined.unwrap_or_default();
    #[cfg(not(feature = "config"))]
    let user_config = Server::default();

    let config = config.runtime;

    let router = Server::init(user_config).await?;
    let state = ServerState::<Server>::builder();

    let middleware = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(config.reverse_proxy.ip_source().into_extension())
        .option_layer(settings.cors.take());

    #[cfg(feature = "compression")]
    let middleware = middleware.layer(
        tower_http::compression::CompressionLayer::new()
            .gzip(true)
            .deflate(true)
            .br(true)
            .zstd(true),
    );

    #[cfg(feature = "database")]
    let state = state.db(utils::database::setup(config.db.as_deref(), Server::db_models()).await?);

    #[cfg(feature = "cookies")]
    let middleware = middleware.layer(tower_cookies::CookieManagerLayer::new());

    #[cfg(feature = "session")]
    let state = {
        state.session_state(session::setup_sessions::<Server>(
            settings.session.take().unwrap_or_default(),
            config.session_key_file.as_deref(),
        )?)
    };

    #[cfg(feature = "htmx")]
    let middleware = middleware.layer(axum_htmx::AutoVaryLayer);

    let router = router
        .fallback_service(utils::assets::ServeAssets::from(Server::assets()))
        .layer(middleware)
        .with_state(state.build());

    let service = router.into_make_service_with_connect_info::<NetworkAddr>();

    let connection = network::connect(&settings, &config.network)
        .await
        .context("Failed to connect to network")?;

    connection.serve(service).await?;

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

#[cfg(feature = "database")]
#[macro_export]
macro_rules! db_models {
    ($($body:tt)*) => {
        fn db_models() -> toasty::ModelSet {
            $crate::db_models!(ret = $($body:tt)* @or_default);
            ret
        }
    };

    ($ret:ident = $($body:tt)+ @or_default ) => { $ret = toasty::models!($($body:tt)+); };
    ($ret:ident = @or_default) => { $ret = toasty::models!(crate::*); };
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
