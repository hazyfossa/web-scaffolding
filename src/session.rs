use axum::{
    extract::{FromRef, FromRequestParts},
    http::StatusCode,
};
use derive_where::derive_where;
use eyre::Result;
use tower_cookies::{Cookie, Cookies};

use crate::{
    errors::{WebError, WebResult},
    store::{ID, Store, Value, ValueRef},
};

// NOTE: no need to explicitly send removal cookie, once entry is removed from store
// next .exists() will return false, and the effect is like cookie is not set

#[derive_where(Clone)]
pub struct SessionState<T> {
    pub store: Store<T>,
    pub cookie_name: &'static str,
}

// TODO: proper Session as 'static ValueRef requires const generics
// and is also less performant for cases where a writable ref is not needed
pub struct Session<T> {
    pub id: ID,
    store_ref: Store<T>,
}

impl<T: Value> Session<T> {
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
}

#[derive_where(Clone)]
pub struct SessionManager<T> {
    state: SessionState<T>,
    cookies: Cookies,
}

impl<T: Value> SessionManager<T> {
    pub async fn current(&self) -> WebResult<Session<T>> {
        let SessionState { store, cookie_name } = &self.state;

        let resolve_current = async || {
            let id = &self.cookies.get(cookie_name)?.value().parse().ok()?;

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
        let SessionState { store, cookie_name } = &self.state;

        let entry = store.insert(data).await?;

        // TODO: test options' security
        let cookie = Cookie::build((*cookie_name, entry.id().to_string()))
            .expires(entry.expires())
            .http_only(true)
            .build();

        self.cookies.add(cookie);

        Ok(entry)
    }
}

impl<S, T> FromRequestParts<S> for SessionManager<T>
where
    S: Send + Sync,
    T: Value,
    SessionState<T>: FromRef<S>,
{
    type Rejection = <Cookies as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let state = SessionState::from_ref(&state);
        let cookies = Cookies::from_request_parts(parts, &state).await?;
        Ok(Self { state, cookies })
    }
}

impl<S, T> FromRequestParts<S> for Session<T>
where
    S: Send + Sync,
    T: Value,
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
