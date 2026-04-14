// Direct cookie extract is blocked by upstream cookie-rs
// zero-copy parsing (CoW). May be solvable with yoke (Cookies as Arc Cart)

pub trait CookieDefinition {
    const NAME: &'static str;
}

use std::marker::PhantomData;

use axum::extract::FromRequestParts;
use eyre::{ContextCompat, eyre};
use shrinkwraprs::Shrinkwrap;
use tower_cookies::Cookies;

pub use crate::_define_cookie as define_cookie;
use crate::errors::WebError;
#[macro_export(local_inner_macros)]
macro_rules! _define_cookie {
    ($vis:vis $struct:ident = $name:literal) => {
        $vis struct $struct;
        $impl $crate::cookies::Cookie for $struct {
            const NAME: &'static str = $name;
        }
    };
}

#[derive(Shrinkwrap)]
pub struct Cookie<'a, T> {
    _def: PhantomData<T>,
    #[shrinkwrap(main_field)]
    inner: tower_cookies::Cookie<'a>,
}

impl<'a, S, T> FromRequestParts<S> for Cookie<'a, T>
where
    S: Send + Sync,
    T: CookieDefinition,
{
    type Rejection = WebError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let cookies = Cookies::from_request_parts(parts, state).await?;
        Ok(cookies
            .get(T::NAME)
            .wrap_err_with(|| eyre!(""))
            .map(|cookie| Cookie {
                inner: cookie,
                _def: PhantomData,
            })?)
    }
}
