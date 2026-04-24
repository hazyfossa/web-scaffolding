// TODO: support bind: localhost
pub use std::net::SocketAddr as NetworkAddr;

pub type Connection<A> = axum_server::Server<NetworkAddr, A>;
pub type Handle = axum_server::Handle<NetworkAddr>;

use axum_client_ip::ClientIpSource;
use eyre::Result;
use serde::{Deserialize, Serialize};

use crate::{Settings, utils::shutdown::ShutdownHandle};

#[cfg_attr(feature = "cli", derive(Clone, clap::ValueEnum))]
#[derive(Serialize, Deserialize, Default)]
pub enum ReverseProxy {
    Nginx,
    Cloudflare,
    Cloudfront,
    FlyIo,
    Akamai,
    Envoy,
    Other,

    #[default]
    None,
}

impl ReverseProxy {
    pub fn ip_source(&self) -> ClientIpSource {
        match self {
            Self::Nginx => ClientIpSource::XRealIp,
            Self::Cloudflare => ClientIpSource::CfConnectingIp,
            Self::Cloudfront => ClientIpSource::CloudFrontViewerAddress,
            Self::FlyIo => ClientIpSource::FlyClientIp,
            Self::Akamai => ClientIpSource::TrueClientIp,
            Self::Envoy => ClientIpSource::XEnvoyExternalAddress,
            Self::None => ClientIpSource::ConnectInfo,
            Self::Other => {
                tracing::info!("Expecting the reverse-proxy to provide X-Forwarded-For headers");
                ClientIpSource::RightmostXForwardedFor
            }
        }
    }
}

#[allow(unused)]
mod http {
    use axum_server::accept::DefaultAcceptor;

    use super::*;

    #[derive(Serialize, Deserialize)]
    #[cfg_attr(feature = "cli", derive(clap::Args))]
    #[serde(default)]
    pub struct NetworkConfig {
        #[cfg_attr(feature = "cli", clap(long, default_value = "127.0.0.1:80"))]
        pub address: NetworkAddr,
    }

    impl Default for NetworkConfig {
        fn default() -> Self {
            Self {
                address: "127.0.0.1:80".parse().unwrap(),
            }
        }
    }

    pub async fn connect(
        _: &Settings,
        config: &NetworkConfig,
    ) -> Result<Connection<DefaultAcceptor>> {
        let address = config.address;
        tracing::info!("Listening on http://{address}");

        let connection = Connection::bind(address);

        let handle = ShutdownHandle::new();
        let connection = handle.register_connection(connection);
        handle.finalize();

        Ok(connection)
    }
}

#[cfg(not(feature = "tls"))]
pub use http::*;

#[cfg(feature = "tls")]
pub use https::*;

#[cfg(feature = "tls")]
mod https {
    use std::path::PathBuf;

    use axum::{
        handler::HandlerWithoutStateExt,
        http::Uri,
        response::{IntoResponse, Redirect},
    };
    use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
    use bon::Builder;
    use eyre::{Context, OptionExt};

    use crate::errors::WebError;

    use super::*;

    // TODO: cli renames
    #[derive(Serialize, Deserialize, Builder)]
    #[serde(default)]
    #[cfg_attr(feature = "cli", derive(clap::Args))]
    pub struct NetworkConfig {
        #[cfg_attr(feature = "cli", clap(flatten))]
        #[serde(flatten)]
        http: Option<super::http::NetworkConfig>,

        #[cfg_attr(feature = "cli", clap(long, default_value = "127.0.0.1:443"))]
        https_address: NetworkAddr,

        #[serde(flatten)]
        #[cfg_attr(feature = "cli", clap(flatten))]
        certificates: Option<CertificateConfig>,
    }

    impl Default for NetworkConfig {
        fn default() -> Self {
            Self {
                http: None,
                https_address: "127.0.0.1:443".parse().unwrap(),
                certificates: None,
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    #[cfg_attr(feature = "cli", derive(clap::Args))]
    pub struct CertificateConfig {
        #[cfg_attr(feature = "cli", clap(long))]
        pub cert: PathBuf,
        #[cfg_attr(feature = "cli", clap(long))]
        pub key: PathBuf,
    }

    impl CertificateConfig {
        async fn read_into_rustls(&self) -> Result<RustlsConfig> {
            RustlsConfig::from_pem_file(&self.cert, &self.key)
                .await
                .context("Failed to create CertificateConfig from PEM files")
        }
    }

    fn make_https(uri: Uri, https_port: u16) -> Result<Uri> {
        let mut parts = uri.into_parts();

        parts.scheme = Some(axum::http::uri::Scheme::HTTPS);
        parts.authority = Some(format!("localhost:{https_port}").parse()?);

        if parts.path_and_query.is_none() {
            parts.path_and_query = Some("/".parse()?);
        }

        Ok(Uri::from_parts(parts)?)
    }

    async fn http_redirect(config: &NetworkConfig, shutdown_handle: &ShutdownHandle) -> Result<()> {
        let http = match &config.http {
            Some(v) => v,
            None => return Ok(()),
        };

        let address = http.address;
        tracing::info!("Redirecting from http://{address}");

        let connection = Connection::bind(address);
        let connection = shutdown_handle.register_connection(connection);

        let https_port = config.https_address.port();

        let redirect = move |uri: Uri| async move {
            make_https(uri, https_port)
                .context("Failed to convert URI to https")
                .map_err(WebError::internal)
                .map(|uri| Redirect::permanent(&uri.to_string()))
                .into_response()
        };

        Ok(connection.serve(redirect.into_make_service()).await?)
    }

    pub async fn connect(
        settings: &Settings,
        config: &NetworkConfig,
    ) -> Result<Connection<RustlsAcceptor>> {
        let handle = ShutdownHandle::new();

        http_redirect(config, &handle)
            .await
            .context("Failed to set up http -> https redirection")?;

        let address = config.https_address;
        tracing::info!("Listening on https://{address}");

        let tls_config = match &settings.rustls {
            Some(v) => v,
            None => {
                &config
                    .certificates
                    .as_ref()
                    .ok_or_eyre("Either provide RustlsSettings, or `cert`, `key`")?
                    .read_into_rustls()
                    .await?
            }
        };

        // TODO: this clone should be fine... check later what tls_config inner is
        let connection = axum_server::bind_rustls(address, tls_config.clone());

        let connection = handle.register_connection(connection);
        handle.finalize();

        Ok(connection)
    }
}
