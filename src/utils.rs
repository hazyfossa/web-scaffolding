// Modules here are designed in a way that makes them
// copy-pastable across projects

pub mod shutdown {
    use std::time::Duration;

    use crate::network::{Connection, Handle};
    use tokio::signal;

    pub struct ShutdownHandle(Handle);

    impl ShutdownHandle {
        pub fn new() -> Self {
            Self(Handle::new())
        }

        pub fn register_connection<A>(&self, c: Connection<A>) -> Connection<A> {
            c.handle(self.0.clone())
        }

        pub fn finalize(self) {
            const GRACE_PERIOD: Duration = Duration::from_secs(10);

            tokio::spawn(async move {
                signal().await;
                self.0.graceful_shutdown(Some(GRACE_PERIOD));
            });
        }
    }

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

#[cfg(feature = "database")]
pub mod database {
    #[cfg(not(feature = "db-sqlite"))]
    use eyre::OptionExt;

    use eyre::{Context, Result};
    use toasty::{Db, ModelSet};

    // TODO: ponder on design on WebServer
    // consider:
    //
    // auto-initializing with models!(crate::*), overridable with settings
    // + intuitive
    // - requires a lot of macro magic
    //
    // merging into Settings
    // + clean
    // - very unintuitive

    pub async fn setup(url: Option<&str>, models: ModelSet) -> Result<Db> {
        #[cfg(feature = "db-sqlite")]
        let url = url.unwrap_or_else(|| {
            tracing::warn!("Using an in-memory database. Data will not be saved!");
            "sqlite::memory:"
        });

        #[cfg(not(feature = "db-sqlite"))]
        let url = url.ok_or_eyre(
            "No database URL was provided. Use db-sqlite driver for an in-memory database.",
        )?;

        let db = toasty::Db::builder()
            .models(models)
            .connect(&url)
            .await
            .context("Failed to connect to database")?;

        db.push_schema()
            .await
            .context("Failed to push schema to database")?;

        tracing::info!("Connected to database");

        Ok(db)
    }
}

// TODO: better semantics. ext of result instead of eyre::Error?
pub mod errors {
    use axum::{BoxError, http::StatusCode, response::IntoResponse};

    #[derive(Debug, Default)]
    enum WebErrorKind {
        Client,
        #[default]
        Internal,
    }

    #[derive(Debug)]
    pub struct WebError {
        kind: WebErrorKind,
        inner: BoxError,
        code: Option<StatusCode>,
    }

    impl WebError {
        pub fn internal(error: impl Into<BoxError>) -> Self {
            Self {
                kind: WebErrorKind::Internal,
                inner: error.into(),
                code: None,
            }
        }

        pub fn client(error: impl Into<BoxError>) -> Self {
            Self {
                kind: WebErrorKind::Client,
                inner: error.into(),
                code: None,
            }
        }

        pub fn from_tuple(v: (StatusCode, &str)) -> Self {
            let (code, text) = v;
            Self {
                kind: WebErrorKind::Internal,
                inner: text.into(),
                code: Some(code),
            }
        }
    }

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

    // Eyre integration

    impl<T> From<T> for WebError
    where
        T: Into<eyre::Error>,
    {
        fn from(value: T) -> Self {
            WebError::internal(value.into())
        }
    }

    pub trait ResultWebExt<T> {
        fn client_error(self) -> Result<T, WebError>;
    }

    impl<T, E: Into<eyre::Error>> ResultWebExt<T> for Result<T, E> {
        fn client_error(self) -> Result<T, WebError> {
            self.map_err(|e| WebError::client(e.into()))
        }
    }

    pub trait ResultCodeExt<T> {
        fn code(self, v: StatusCode) -> Result<T, WebError>;
    }

    impl<T> ResultCodeExt<T> for WebResult<T> {
        fn code(self, v: StatusCode) -> Result<T, WebError> {
            self.map_err(|mut e| {
                e.code.replace(v);
                e
            })
        }
    }
}

pub mod assets {
    use axum::{
        extract::Request,
        http::{Method, StatusCode, header},
        response::{IntoResponse, Response},
    };
    use eyre::{OptionExt, eyre};
    use rust_embed::{EmbeddedFile, RustEmbed};

    use crate::errors::ResultCodeExt;

    use super::errors::{ResultWebExt, WebResult};

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

            let content = (self.get)(uri)
                .ok_or_eyre("404 Not Found")
                .client_error()
                .code(StatusCode::NOT_FOUND)?;

            if request.method() != Method::GET {
                return Err(eyre!("Method Not Allowed"))
                    .client_error()
                    .code(StatusCode::METHOD_NOT_ALLOWED);
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

mod macros {
    #[macro_export(local_inner_macros)]
    macro_rules! trait_alias {
        ($vis:vis trait $name:ident : $($for:tt)*) => {
            $vis trait $name: $($for)* {}
            impl<T: $($for)*> $name for T {}
        };
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
    use eyre::{Context, Result};
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, map::Entry};

    // while merging, fields of next value overwrite fields of previous value
    // for [a, b, c],
    // c has highest priority (can overwrite a, b)
    // b can only overwrite a
    // a has lowest priority

    #[macro_export]
    macro_rules! merge {
        ($($value:ident),+) => {
            $crate::utils::json_merge::merge([$(
                (stringify!($value), Option::from($value))
            ),+])
        };
    }

    pub fn merge<I, T>(values: I) -> Result<T>
    where
        I: IntoIterator<Item = (&'static str, Option<T>)>,
        T: Serialize + DeserializeOwned + Default,
    {
        let mut target = value("target", T::default())?;

        for (name, diff) in values {
            if let Some(diff) = diff {
                merge_values(&mut target, &value(name, diff)?);
            }
        }

        serde_json::from_value(target).context("Value diverged from schema after merge")
    }

    fn value<T: Serialize>(name: &'static str, t: T) -> Result<Value> {
        serde_json::to_value(t)
            .wrap_err_with(|| format!("Cannot represent `{name}` as a dynamic value"))
    }

    fn merge_values(destination: &mut Value, other: &Value) {
        match (destination, other) {
            (Value::Object(a), Value::Object(b)) => {
                for (k, v) in b {
                    match a.entry(k) {
                        Entry::Occupied(mut e) => merge_values(e.get_mut(), v),
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

    #[cfg(test)]
    mod tests {
        use serde_json::{Value, json};

        #[test]
        fn test_merge_objects() {
            let a = json!({"a": 1});
            let b = json!({"b": 2});
            let result: Value = merge!(a, b).unwrap();
            assert_eq!(result, json!({"a": 1, "b": 2}));
        }

        #[test]
        fn test_nested_merge() {
            let a = json!({"obj": {"x": 1}});
            let b = json!({"obj": {"y": 2}});
            let result: Value = merge!(a, b).unwrap();
            assert_eq!(result, json!({"obj": {"x": 1, "y": 2}}));
        }

        #[test]
        fn test_array_extend() {
            let a = json!({"arr": [1]});
            let b = json!({"arr": [2, 3]});
            let result: Value = merge!(a, b).unwrap();
            assert_eq!(result, json!({"arr": [1, 2, 3]}));
        }

        #[test]
        fn test_array_push_object() {
            let a = json!({"arr": [1]});
            let b = json!({"arr": {"key": "value"}});
            let result: Value = merge!(a, b).unwrap();
            assert_eq!(result, json!({"arr": [1, {"key": "value"}]}));
        }

        #[test]
        fn test_overwrite() {
            let a = json!({"a": 1});
            let b = json!({"a": 2});
            let result: Value = merge!(a, b).unwrap();
            assert_eq!(result, json!({"a": 2}));
        }

        #[test]
        fn test_skip_none() {
            let a = json!({"a": 1});
            let b: Option<Value> = None;
            let result: Value = merge!(a, b).unwrap();
            assert_eq!(result, json!({"a": 1}));
        }
    }
}
