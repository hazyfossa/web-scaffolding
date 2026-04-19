use axum::{
    extract::{FromRef, FromRequestParts},
    http::StatusCode,
};
use bon::Builder;
use derive_where::derive_where;
use eyre::{ContextCompat, Result};
use time::Duration;
use tower_cookies::{Cookie, Cookies, SignedCookies, cookie::SameSite};

use crate::{
    BuiltInConfig, WebServer,
    errors::{WebError, WebResult},
    store::{ID, Store, ValueRef},
};

pub use tower_cookies::Key;

// NOTE: no need to explicitly send removal cookie, once entry is removed from store
// next .exists() will return false, and the effect is like cookie is not set

#[derive_where(Clone)]
pub struct SessionState<T> {
    store: Store<T>,
    cookie_name: &'static str,
    key: Key,
    insecure: bool,
}

// TODO: proper Session as 'static ValueRef requires const generics
// and is also less performant for cases where a writable ref is not needed
pub struct Session<T> {
    pub id: ID,
    store_ref: Store<T>,
}

impl<T> Session<T> {
    pub async fn resolve(&self) -> WebResult<ValueRef<'_, T>> {
        let entry = self
            .store_ref
            .query(&self.id)
            .await
            // NOTE: the error case happens if a request handler passes a session
            // off to a long-running task, which tries to actually use the session much later
            .ok_or(WebError::client("Session expired").code(StatusCode::UNAUTHORIZED))?;

        Ok(entry)
    }

    // Is faster than .resolve().remove(), but deadlocks if already resolved:
    //
    // ```
    // let data = session.resolve().await?;
    // session.remove_unresolved().await?; <-- deadlock
    // ```
    // pub async fn remove_unresolved(self) -> Option<()> {
    //     self.store_ref.delete(&self.id).await.map(|_| ())
    // }
}

#[derive_where(Clone)]
pub struct SessionManager<T> {
    state: SessionState<T>,
    _unsigned_cookies: Cookies,
}

impl<T> SessionManager<T> {
    fn cookies(&self) -> SignedCookies<'_> {
        self._unsigned_cookies.signed(&self.state.key)
    }

    pub async fn current(&self) -> WebResult<Session<T>> {
        let SessionState {
            store, cookie_name, ..
        } = &self.state;

        let resolve_current = async || {
            let id = &self.cookies().get(cookie_name)?.value().parse().ok()?;

            store.exists(id).await.then_some(Session {
                id: id.clone(),
                store_ref: store.clone(),
            })
        };

        resolve_current()
            .await
            .ok_or(WebError::client("Unauthorized").code(StatusCode::UNAUTHORIZED))
    }

    pub async fn create(&self, data: T) -> Result<ValueRef<'_, T>> {
        let SessionState {
            store,
            cookie_name,
            insecure,
            ..
        } = &self.state;

        let entry = store.insert(data).await?;

        // TODO: test options' security
        let cookie = Cookie::build((*cookie_name, entry.id().to_string()))
            .http_only(true)
            .max_age(entry.lifetime.clone())
            .same_site(SameSite::Strict)
            .secure(!insecure)
            .build();

        self.cookies().add(cookie);

        Ok(entry)
    }
}

impl<S, T> FromRequestParts<S> for SessionManager<T>
where
    S: Send + Sync,
    T: Send + Sync,
    SessionState<T>: FromRef<S>,
{
    type Rejection = <Cookies as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let state = SessionState::from_ref(&state);
        let cookies = Cookies::from_request_parts(parts, &state).await?;

        Ok(Self {
            state,
            _unsigned_cookies: cookies,
        })
    }
}

impl<S, T> FromRequestParts<S> for Session<T>
where
    S: Send + Sync,
    T: Send + Sync,
    SessionState<T>: FromRef<S>,
{
    type Rejection = WebError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let manager = SessionManager::from_request_parts(parts, state).await?;
        manager.current().await
    }
}

#[derive(Builder)]
pub struct SessionSettings {
    #[builder(default = "session")]
    pub cookie_name: &'static str,

    #[builder(default = Duration::days(14))]
    pub lifetime: Duration,

    #[builder(default = lifetime)]
    pub cleanup_interval: Duration,

    pub key: Option<Key>,

    #[builder(default)]
    pub insecure: bool,
}

pub(crate) fn setup_sessions<Server: WebServer>(
    config: &BuiltInConfig,
) -> Result<SessionState<Server::SessionData>> {
    let SessionSettings {
        cookie_name,
        lifetime,
        cleanup_interval,
        key,
        insecure,
    } = Server::session_settings();

    let store = Store::<Server::SessionData>::new(lifetime).with_cleanup(cleanup_interval);

    let key = key
        .or_else(|| {
            config.session_key_file.as_deref().and_then(|file| {
                let data = std::fs::read(file).ok()?;
                Key::try_from(data.as_ref()).ok()
            })
        })
        .or_else(|| {
            tracing::info!("Generated ephemeral session key");
            Key::try_generate()
        })
        .context("Failed to get or generate session key")?;

    Ok(SessionState {
        cookie_name,
        store,
        key,
        insecure,
    })
}
