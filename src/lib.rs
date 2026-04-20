#![allow(private_interfaces, private_bounds)] // TODO: this is only used for ConfigOverride

use std::{marker::PhantomData, net::SocketAddr};

use bon::Builder;
use derive_where::derive_where;
pub use rust_embed::Embed as LoadAssets;

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
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
struct BuiltInConfig {
    #[cfg_attr(feature = "cli", clap(long))]
    #[cfg_attr(feature = "cli", clap(default_value = "localhost"))]
    #[builder(default = "localhost".into())]
    host: String,

    #[cfg_attr(feature = "cli", clap(short, long))]
    #[cfg_attr(feature = "cli", clap(default_value = "8080"))]
    #[builder(default = 8080)]
    port: u16,

    #[cfg_attr(feature = "cli", clap(long))]
    reverse_proxy: Option<String>,

    #[cfg(feature = "database")]
    #[cfg_attr(feature = "cli", clap(long))]
    db: Option<String>,

    #[cfg(feature = "session")]
    #[cfg_attr(feature = "cli", clap(long))]
    session_key_file: Option<std::path::PathBuf>,
}

impl Default for BuiltInConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

pub type ConfigOverride = BuiltInConfig;

#[derive(Builder)]
#[derive_where(Default, Serialize, Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
struct Config<S: WebServer> {
    #[serde(flatten)]
    #[cfg_attr(feature = "cli", clap(flatten))]
    built_in: BuiltInConfig,

    #[cfg(not(feature = "config"))]
    #[builder(default = PhantomData)]
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

    // Only override built-ins, since user_defined overrides are just... defaults
    let from_compile_time = settings
        .config_override
        .take()
        .map(|v| Config::builder().built_in(v).build());

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

    cors: Option<Cors>,

    #[cfg(feature = "session")]
    session: Option<SessionSettings>,
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

#[allow(async_fn_in_trait, private_bounds)]
pub trait WebServer: WebServerLoad + Send + Sync + 'static + Sized {
    async fn init(self) -> Result<Router<Self>>;

    fn settings() -> Settings {
        Settings::default()
    }

    // TODO: better asset handling
    fn assets() -> impl LoadAssets;

    #[cfg(feature = "session")]
    type SessionData: store::Value;
}

pub async fn run_server<Server: WebServer>() -> Result<()> {
    simple_eyre::install()?;
    tracing_subscriber::fmt::init();

    let (mut settings, config) = load_config::<Server>()
        .await
        .context("Failed to load config")?;

    let (listener, ip_source) = utils::network::setup_network(&config.built_in)
        .await
        .context("Failed to set up network")?;

    #[cfg(feature = "config")]
    let user_config = config.user_defined.unwrap_or_default();
    #[cfg(not(feature = "config"))]
    let user_config = Server::default();

    let router = Server::init(user_config).await?;

    let state = ServerState::<Server>::builder();

    let middleware = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(ip_source.into_extension())
        .option_layer(settings.cors);

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
    let state = {
        state.session_state(session::setup_sessions::<Server>(
            settings.session.take().unwrap_or_default(),
            config.built_in.session_key_file.as_deref(),
        )?)
    };

    #[cfg(feature = "htmx")]
    let middleware = middleware.layer(axum_htmx::AutoVaryLayer);

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
