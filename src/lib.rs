use std::net::SocketAddr;

pub use eyre;
pub use facet::Facet;
pub use figue as args;
pub use rust_embed::Embed as LoadAssets;

use axum_client_ip::ClientIpSource;
use eyre::{Context, Result, bail};
use tokio::net::TcpListener;

mod utils;
use tower::ServiceBuilder;
use tower_http::{catch_panic::CatchPanicLayer, compression::CompressionLayer};
pub use utils::{errors, scheduler};

use crate::utils::assets::ServeAssets;

async fn setup_network(config: &BuiltInConfig) -> Result<(TcpListener, ClientIpSource)> {
    let addr = &config.bind_address;
    let listener = TcpListener::bind(addr)
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
            // TODO: support full axum-client-ip
            other => bail!("{other} is not a supported proxy type"),
        },
    };

    Ok((listener, ip_source))
}

#[derive(Facet)]
struct BuiltInConfig {
    bind_address: String,
    reverse_proxy: Option<String>,
    #[facet(flatten)]
    builtins: figue::FigueBuiltins,
}

#[derive(Facet)]
struct WithServerConfig<T> {
    #[facet(flatten)]
    built_in: BuiltInConfig,
    #[facet(flatten)]
    user_defined: T,
}

pub async fn web_serve<Server: WebServer>() -> Result<()> {
    simple_eyre::install()?;
    tracing_subscriber::fmt::init();

    let config = figue::builder::<WithServerConfig<Server>>()?
        .cli(|c| c.args_os(std::env::args_os()))
        .env(|c| c.prefix(Server::APPNAME.to_uppercase()))
        .file(|c| c.default_paths(["./config.json"]))
        // TODO: support version, description
        .help(|c| c.program_name(Server::APPNAME))
        .build();

    // NOTE: this unwrap is preferred to Result
    let config = figue::Driver::new(config).run().unwrap();

    let (listener, ip_source) = setup_network(&config.built_in).await?;

    let router = Server::init(config.user_defined).await?;

    let router = router
        .fallback_service(ServeAssets(Server::assets()))
        .layer(
            ServiceBuilder::new()
                .layer(CatchPanicLayer::new())
                .layer(CompressionLayer::new())
                .layer(ip_source.into_extension()),
        );

    let service = router.into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, service)
        .with_graceful_shutdown(utils::shutdown::signal())
        .await?;

    Ok(())
}

#[allow(async_fn_in_trait)]
pub trait WebServer: for<'a> Facet<'a> {
    const APPNAME: &str;

    // TODO: better asset handling
    fn assets() -> &'static (impl rust_embed::Embed + Send + Sync);

    async fn init(self) -> Result<axum::Router>;
}
