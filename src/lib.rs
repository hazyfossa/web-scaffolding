use std::{io::ErrorKind, marker::PhantomData, net::SocketAddr, path::PathBuf};

pub use axum_client_ip::ClientIp;
use bon::Builder;
use derive_where::derive_where;
pub use rust_embed::Embed as LoadAssets;

use eyre::{Context, Result, bail};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::fs;

use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
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
pub use utils::database::get as database;

// TODO: drop alias once toasty becomes
// properly importable as a crate
#[cfg(feature = "database")]
pub use toasty;

mod utils;
pub use utils::{errors, scheduler};

// Config

#[derive(Deserialize, Builder)]
#[serde(default)]
struct BuiltInConfig {
    #[builder(default = "localhost".into())]
    host: String,
    #[builder(default = 80)]
    port: u16,
    reverse_proxy: Option<String>,
    #[cfg(feature = "database")]
    db: Option<String>,
    #[cfg(feature = "session")]
    session_key: Option<Vec<u8>>,
}

impl Default for BuiltInConfig {
    fn default() -> Self {
        Self::builder().build()
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

// State

#[derive_where(Clone)]
#[derive(Builder)]
pub struct ServerState<T: WebServer> {
    #[builder(default = PhantomData)]
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

#[allow(async_fn_in_trait)]
pub trait WebServer: DeserializeOwned + Default + Send + Sync + 'static {
    #[cfg(feature = "session")]
    type SessionData: store::Value;

    #[cfg(feature = "session")]
    fn session_settings() -> SessionSettings {
        SessionSettings::builder().build()
    }

    fn cors() -> Option<Cors> {
        None
    }

    // TODO: better asset handling
    fn assets() -> impl LoadAssets;

    async fn init(self) -> Result<axum::Router<ServerState<Self>>>;
}

pub async fn run_server<Server: WebServer>() -> Result<()> {
    simple_eyre::install()?;
    tracing_subscriber::fmt::init();

    let config = load_config::<WithBuiltinConfig<Server>>()
        .await
        .context("Failed to load config")?;

    let (listener, ip_source) = utils::network::setup_network(&config.built_in)
        .await
        .context("Failed to set up network")?;

    let router = Server::init(config.user_defined).await?;

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
