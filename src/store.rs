use derive_where::derive_where;
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

pub type ID = TimedUuid;

pub trait Value: Send + Sync + 'static {
    const LIFETIME: Duration;
}

// TODO: consider readonly refs

#[derive(Shrinkwrap)]
#[shrinkwrap(mutable)]
#[shrinkwrap(unsafe_ignore_visibility)]
pub struct ValueRef<'a, T>(OccupiedEntry<'a, ID, T>);

impl<'a, T: Value> ValueRef<'a, T> {
    pub fn id(&self) -> &ID {
        self.key()
    }

    pub fn expires(&self) -> OffsetDateTime {
        self.key().timestamp() + T::LIFETIME
    }

    pub fn entry(self) -> OccupiedEntry<'a, ID, T> {
        self.0
    }

    pub fn remove(self) -> T {
        self.entry().remove()
    }
}

#[derive(Shrinkwrap)]
#[derive_where(Clone)]
pub struct Store<T> {
    inner: Arc<StoreInner<T>>,
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

    pub async fn exists(&self, id: &ID) -> bool {
        self.data.contains_async(id).await
    }

    pub async fn query(&self, id: &ID) -> Option<ValueRef<'_, T>> {
        let value_ref = self.data.get_async(id).await.map(ValueRef)?;

        let now = OffsetDateTime::now_utc();
        let expired = now > value_ref.expires();

        (!expired).then_some(value_ref)
    }

    pub async fn delete(&self, id: &ID) -> Option<(ID, T)> {
        self.data.remove_async(id).await
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
