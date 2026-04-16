// Modules here are designed in a way that makes them
// copy-pastable across projects

pub mod shutdown {
    use tokio::signal;

    pub async fn signal() {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }

        tracing::info!("Shutting down");
    }
}

pub mod scheduler {
    pub use time::Duration as Interval;

    pub fn schedule_task<F, Fut>(name: &str, interval: Interval, task_fn: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send,
    {
        tracing::info!("Scheduled {name} to run every {interval}");

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            interval.whole_seconds() as u64,
        ));

        tokio::spawn(async move {
            loop {
                interval.tick().await;
                task_fn().await;
            }
        });
    }
}

pub mod network {
    use axum_client_ip::ClientIpSource;
    use eyre::{Context, Result};
    use tokio::net::TcpListener;

    use crate::BuiltInConfig;

    pub async fn setup_network(config: &BuiltInConfig) -> Result<(TcpListener, ClientIpSource)> {
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
}

#[cfg(feature = "database")]
pub mod database {
    use std::sync::OnceLock;

    use eyre::{Context, Result};

    use crate::BuiltInConfig;

    // TODO: this is only used if accessing db outside of axum. consider removal
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

// TODO: better semantics. ext of result instead of eyre::Error?
pub mod errors {
    use axum::{BoxError, http::StatusCode, response::IntoResponse};

    #[derive(Debug)]
    enum WebErrorKind {
        Client,
        Internal,
    }

    #[derive(Debug)]
    pub struct WebError {
        kind: WebErrorKind,
        inner: BoxError,
        code: Option<StatusCode>,
    }

    impl WebError {
        pub fn code(mut self, value: StatusCode) -> Self {
            self.code.replace(value);
            self
        }

        pub fn internal(error: impl Into<BoxError>) -> Self {
            WebError {
                kind: WebErrorKind::Internal,
                inner: error.into(),
                code: None,
            }
        }

        pub fn client(error: impl Into<BoxError>) -> Self {
            WebError {
                kind: WebErrorKind::Client,
                inner: error.into(),
                code: None,
            }
        }
    }

    // TODO: color-eyre does not play well with tracing

    impl IntoResponse for WebError {
        fn into_response(self) -> axum::response::Response {
            match self.kind {
                WebErrorKind::Client => {
                    tracing::warn!("Client error: {}", self.inner);
                    (
                        self.code.unwrap_or(StatusCode::BAD_REQUEST),
                        format!("{:?}", self.inner),
                    )
                }
                WebErrorKind::Internal => {
                    tracing::error!("Internal server error: {}", self.inner);
                    (
                        self.code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                        "Something went wrong".to_string(),
                    )
                }
            }
            .into_response()
        }
    }

    pub type WebResult<T> = Result<T, WebError>;

    impl From<(StatusCode, &'static str)> for WebError {
        fn from(value: (StatusCode, &'static str)) -> Self {
            let (code, string) = value;
            Self::internal(string).code(code)
        }
    }

    // Eyre integration

    impl From<eyre::Error> for WebError {
        fn from(value: eyre::Error) -> Self {
            Self::internal(value)
        }
    }

    pub trait EyreWebExt {
        fn client_error(self) -> WebError;
    }

    impl EyreWebExt for eyre::Error {
        fn client_error(self) -> WebError {
            WebError::client(self)
        }
    }
}

pub mod assets {
    use axum::{
        extract::Request,
        http::{Method, StatusCode, header},
        response::{IntoResponse, Response},
    };
    use eyre::eyre;
    use rust_embed::{EmbeddedFile, RustEmbed};

    use super::errors::{EyreWebExt, WebResult};

    #[derive(Clone)]
    pub struct ServeAssets {
        get: fn(&str) -> Option<EmbeddedFile>,
    }

    impl<T> From<T> for ServeAssets
    where
        T: RustEmbed,
    {
        fn from(_: T) -> Self {
            Self { get: T::get }
        }
    }

    impl ServeAssets {
        fn serve(&self, request: Request) -> WebResult<Response> {
            let uri = request.uri().path().trim_start_matches('/');

            let content = (self.get)(uri).ok_or(
                eyre!("404 Not Found")
                    .client_error()
                    .code(StatusCode::NOT_FOUND),
            )?;

            if request.method() != Method::GET {
                return Err(eyre!("Method Not Allowed")
                    .client_error()
                    .code(StatusCode::METHOD_NOT_ALLOWED));
            };

            Ok((
                [(header::CONTENT_TYPE, content.metadata.mimetype())],
                content.data,
            )
                .into_response())
        }
    }

    impl tower::Service<Request> for ServeAssets {
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Response, Self::Error>>;
        type Response = Response;

        fn call(&mut self, request: Request) -> Self::Future {
            std::future::ready(Ok(self.serve(request).into_response()))
        }

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
}

#[cfg(feature = "store")]
pub mod timed_uuid {
    use std::{fmt::Display, ops::Deref, str::FromStr};

    use eyre::{Context, ContextCompat, Ok, Result};
    use time::{Duration, OffsetDateTime};
    use uuid::Uuid;

    pub fn timestamp_from_uuid(uuid: &Uuid) -> Result<OffsetDateTime> {
        let ts = uuid
            .get_timestamp()
            .context("UUID is not a time-based version (expected v1, v6, or v7)")?;

        let (seconds, subsec_nanos) = ts.to_unix();

        let seconds = seconds
            .try_into()
            .wrap_err("Overflow: Unix timestamp too large for i64")?;

        let subsec_nanos = Duration::nanoseconds(subsec_nanos.into());

        let base_time =
            OffsetDateTime::from_unix_timestamp(seconds).context("Invalid Unix timestamp")?;

        Ok(base_time + subsec_nanos)
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct TimedUuid(Uuid);

    impl Deref for TimedUuid {
        type Target = Uuid;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Display for TimedUuid {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl TimedUuid {
        pub fn new_now() -> Result<Self> {
            let uuid = Uuid::now_v7();
            uuid.try_into()
        }

        pub fn timestamp(&self) -> OffsetDateTime {
            // This .expect is guarded by _check at try_from
            timestamp_from_uuid(&self).expect("Timestamp conversion failed")
        }
    }

    impl TryFrom<Uuid> for TimedUuid {
        type Error = eyre::Error;

        fn try_from(value: Uuid) -> std::result::Result<Self, Self::Error> {
            let _check = timestamp_from_uuid(&value).context("Invalid timestamp")?;
            Ok(Self(value))
        }
    }

    impl FromStr for TimedUuid {
        type Err = eyre::Error;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            let uuid = Uuid::from_str(s)?;
            uuid.try_into()
        }
    }
}

pub mod json_merge {
    use serde_json::{Value, map::Entry};

    // TODO: compile-time config overrides (not a priority)
    #[allow(unused)]
    pub fn merge(destination: &mut Value, other: &Value) {
        match (destination, other) {
            (Value::Object(a), Value::Object(b)) => {
                for (k, v) in b {
                    match a.entry(k) {
                        Entry::Occupied(mut e) => merge(e.get_mut(), v),
                        Entry::Vacant(e) => {
                            e.insert(v.clone());
                        }
                    }
                }
            }

            (Value::Array(a), Value::Array(b)) => a.extend(b.clone()),
            (Value::Array(a), Value::Object(b)) => a.push(Value::from(b.clone())),

            // no-op
            (_any, Value::Null) => {}

            // any other
            (a, b) => *a = b.clone(),
        }
    }
}
