use eyre::{Context, Result, bail};
use scc::{
    HashMap,
    hash_map::{Entry, OccupiedEntry},
};
use shrinkwraprs::Shrinkwrap;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

use crate::utils::{scheduler, timed_uuid::TimedUuid};

// TODO: proper support for key types other than UUID

type ID = TimedUuid;

pub trait Value: Send + Sync + 'static {
    const LIFETIME: Duration;
}

// TODO: consider readonly refs

#[derive(Shrinkwrap)]
#[shrinkwrap(mutable)]
// Safety: .0 on any value is redundant,
// modifying OccupiedEntry is safe
#[shrinkwrap(unsafe_ignore_visibility)]
pub struct ValueRef<'a, T>(OccupiedEntry<'a, ID, T>);

impl<'a, T: Value> ValueRef<'a, T> {
    fn expires(&self) -> OffsetDateTime {
        self.key().timestamp() + T::LIFETIME
    }
}

#[derive(Shrinkwrap)]
pub struct Store<T> {
    inner: Arc<StoreInner<T>>,
}

impl<T> Clone for Store<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<'a, T: Value> Store<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StoreInner::new()),
        }
    }

    pub fn with_cleanup(self, interval: scheduler::Interval) -> Self {
        // TODO: it might be useful to cleanup more often under high memory pressure

        let accessor = self.clone();

        scheduler::schedule_task(
            &format!("{} value cleanup", std::any::type_name::<T>()),
            interval,
            move || {
                let store = accessor.clone();
                async move {
                    store.cleanup().await;
                }
            },
        );
        self
    }

    pub fn with_auto_cleanup(self) -> Self {
        self.with_cleanup(T::LIFETIME)
    }
}

pub struct StoreInner<T> {
    data: HashMap<ID, T>,
}

impl<T: Value> StoreInner<T> {
    fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    pub async fn insert(&self, data: T) -> Result<ValueRef<'_, T>> {
        let key = TimedUuid::new_now().context("Failed to get UUID")?;

        let entry = match self.data.entry_async(key).await {
            Entry::Occupied(_) => bail!("Key already exists"),
            Entry::Vacant(place) => place.insert_entry(data),
        };

        Ok(ValueRef(entry))
    }

    /// Same as query, except does not perform expiry check,
    /// allowing retrieval of technically invalid values
    /// Do NOT use in security-sensitive scenarions
    #[inline]
    pub async fn query_relaxed(&self, id: &ID) -> Option<ValueRef<'_, T>> {
        self.data.get_async(id).await.map(|v| ValueRef(v))
    }

    pub async fn query(&self, id: &ID) -> Option<ValueRef<'_, T>> {
        let value_ref = self.query_relaxed(&id).await?;

        let now = OffsetDateTime::now_utc();
        let expired = now > value_ref.expires();

        (!expired).then_some(value_ref)
    }

    #[allow(dead_code)]
    pub async fn delete(&self, id: &ID) {
        self.data.remove_async(id).await;
    }

    async fn cleanup(&self) {
        let now = OffsetDateTime::now_utc();

        self.data
            .retain_async(|uuid, _| {
                let expires = uuid.timestamp() + T::LIFETIME;
                now < expires
            })
            .await;
    }

    #[inline]
    pub async fn retain<F: FnMut(&ID, &mut T) -> bool>(&self, pred: F) {
        self.data.retain_async(pred).await
    }
}
