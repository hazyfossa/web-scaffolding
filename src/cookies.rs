// Direct cookie extract is blocked by upstream cookie-rs
// zero-copy parsing (CoW). May be solvable with yoke (Cookies as Arc Cart)

use std::ops::Deref;

use crate::errors::WebError;

use axum::extract::FromRequestParts;
use eyre::{Context, OptionExt, Result, eyre};
use shrinkwraprs::Shrinkwrap;
use stable_deref_trait::StableDeref;
use tower_cookies::Cookies;
use yoke::{Yoke, Yokeable};

pub use tower_cookies::Cookie as RawCookie;

pub trait CookieDefinition: CookieRepr {
    const NAME: &'static str;
}

pub trait CookieRepr: Sized {
    fn serialize(&self) -> String;
    fn deserialize(value: &str) -> Result<Self>;
}

pub use crate::_define_cookie as define_cookie;
#[macro_export(local_inner_macros)]
macro_rules! _define_cookie {
    ($vis:vis $struct:ident($value:path) = $(#$repr:tt)? $name:literal) => {
        $vis struct $struct($value);

        impl $crate::cookies::Cookie for $struct {
            const NAME: &'static str = $name;
        }

        impl std::ops::Deref for $struct {
            type Target = $value;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        $(impl $crate::CookieRepr for $struct {
            $crate::define_cookie!(@repr $repr);
        })?
    };

    (@repr string) => {
        fn serialize(&self) -> String { self.0.to_string() }
        fn deserialize(value: &str) -> Result<Self> { Ok(Self(value.parse()?)) }
    };

    (@repr json) => {
        fn serialize(&self) -> String { Ok(serde_json::to_string(self.0)?) }
        fn deserialize(value: &str) -> Result<Self> { Ok(Self(serde_json::from_str(value)?)) }
    };
}

#[derive(Shrinkwrap, Yokeable)]
pub struct ParsedCookie<'a, T> {
    #[shrinkwrap(main_field)]
    raw: tower_cookies::Cookie<'a>,
    parsed: T,
}

pub struct CookieCart {
    inner: Cookies,
}

impl Deref for CookieCart {
    type Target = Cookies;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// Safety: tower_cookies::Cookies is an Arc<...>
unsafe impl StableDeref for CookieCart {}

pub struct Cookie<T: 'static> {
    inner: Yoke<ParsedCookie<'static, T>, CookieCart>,
}

impl<'de, T: CookieDefinition> Cookie<T> {
    pub fn get_from(cookies: Cookies) -> Result<Self> {
        let cart = CookieCart {
            inner: cookies.clone(),
        };
        let yoke = Yoke::try_attach_to_cart(cart, |cookies: &Cookies| {
            let raw = cookies.get(T::NAME).ok_or_eyre("not found")?;
            let parsed = T::deserialize(raw.value()).context("contents are invalid")?;
            eyre::Ok(ParsedCookie { raw, parsed })
        })
        .wrap_err_with(|| eyre!("Error while getting a cookie: {}", T::NAME));

        Ok(Self { inner: yoke? })
    }

    pub fn value(&self) -> &T {
        &self.inner.get().parsed
    }

    pub fn as_string(&self) -> &str {
        self.inner.get().raw.value()
    }

    pub fn raw(&self) -> &RawCookie<'_> {
        &self.inner.get().raw
    }
}

impl<S, T> FromRequestParts<S> for Cookie<T>
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
        Ok(Cookie::<T>::get_from(cookies)?)
    }
}
