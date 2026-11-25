//! The configuration versions an API-serving process routes from.
//!
//! Every API-serving profile (`all-in-one`, `api`, `data-plane`) loads the
//! version each organization serves at startup and then polls the feed. A
//! request is routed from what is loaded, never from the database, so a
//! process cut off from the primary keeps its routing (and fails at
//! reservation instead). The process checks on every poll that the feed
//! entry it applied is still at its position, and reloads everything when it
//! is not: after a restore or a failover that lost entries, positions are
//! handed out again. See docs/architecture-plan.md §2.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    config::InstanceConfig,
    domain::identity::OrganizationId,
    persistence::config_versions::{
        ConfigStore, ConfigStoreError, FeedPosition, LoadedVersion, RouteCandidate,
    },
};

/// A route that misses (an alias activated moments ago, say) refreshes at
/// most this often, so unknown aliases cannot load the database.
const MISS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// When the last poll succeeded, for `gateway_config_age_seconds`. One
/// cache per process.
static LAST_SUCCESS: Mutex<Option<Instant>> = Mutex::new(None);

/// Seconds since this process last read the feed, if it routes at all.
pub(crate) fn age_seconds() -> Option<i64> {
    let last = *LAST_SUCCESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    last.map(|at| i64::try_from(at.elapsed().as_secs()).unwrap_or(i64::MAX))
}

#[derive(Default)]
struct Loaded {
    applied: Option<FeedPosition>,
    /// When the configuration was last (re)loaded; `None` until the first
    /// load.
    loaded_at: Option<DateTime<Utc>>,
    organizations: HashMap<OrganizationId, Arc<LoadedVersion>>,
}

struct Inner {
    store: ConfigStore,
    instance: InstanceConfig,
    profile: &'static str,
    started_at: DateTime<Utc>,
    loaded: RwLock<Loaded>,
    /// Serializes refreshes; holds when a missed route last refreshed.
    refreshing: tokio::sync::Mutex<Option<Instant>>,
    last_success: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub struct ConfigCache {
    inner: Arc<Inner>,
}

impl ConfigCache {
    /// Loads every organization's served version. Fails when the feed
    /// cannot be read or a version fails its digest check.
    pub async fn load(
        store: ConfigStore,
        instance: InstanceConfig,
        profile: &'static str,
    ) -> Result<Self, ConfigStoreError> {
        let cache = Self {
            inner: Arc::new(Inner {
                store,
                instance,
                profile,
                started_at: Utc::now(),
                loaded: RwLock::new(Loaded::default()),
                refreshing: tokio::sync::Mutex::new(None),
                last_success: Mutex::new(None),
            }),
        };
        cache.refresh().await?;
        Ok(cache)
    }

    /// Polls the feed every configured interval for the life of the process.
    pub fn spawn_polling(&self) {
        let cache = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(cache.inner.instance.poll).await;
                if let Err(error) = cache.refresh().await {
                    tracing::warn!(%error, "configuration poll failed; serving what is loaded");
                }
            }
        });
    }

    /// The deployments `organization` routes `alias` and `operation` to
    /// now; empty when it routes none.
    pub(crate) fn routes(
        &self,
        organization: OrganizationId,
        alias: &str,
        operation: &str,
    ) -> Vec<RouteCandidate> {
        let Some(version) = self.read().organizations.get(&organization).cloned() else {
            return Vec::new();
        };
        version.document.candidates(alias, operation, Utc::now())
    }

    /// As [`routes`](Self::routes), but a miss first reads the feed (at most
    /// once a second), so a release activated moments ago is served at once
    /// by the process that sees the request.
    pub(crate) async fn routes_or_refresh(
        &self,
        organization: OrganizationId,
        alias: &str,
        operation: &str,
    ) -> Vec<RouteCandidate> {
        let routes = self.routes(organization, alias, operation);
        if !routes.is_empty() {
            return routes;
        }
        {
            let mut refreshing = self.inner.refreshing.lock().await;
            if refreshing.is_some_and(|at| at.elapsed() < MISS_REFRESH_INTERVAL) {
                // A refresh this request waited for may have loaded it.
                return self.routes(organization, alias, operation);
            }
            *refreshing = Some(Instant::now());
            if let Err(error) = self.refresh_locked().await {
                tracing::warn!(%error, "configuration refresh failed");
            }
        }
        self.routes(organization, alias, operation)
    }

    /// Whether the process can serve: configuration loaded, and the feed read
    /// recently enough (three polls, at least 15 seconds) that the primary
    /// is reachable.
    pub fn ready(&self) -> bool {
        let fresh = Duration::max(self.inner.instance.poll * 3, Duration::from_secs(15));
        self.read().loaded_at.is_some()
            && self
                .inner
                .last_success
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some_and(|at| at.elapsed() < fresh)
    }

    /// The version this process serves for `organization`.
    pub fn served_version(&self, organization: OrganizationId) -> Option<Uuid> {
        self.read()
            .organizations
            .get(&organization)
            .map(|version| version.version_id)
    }

    /// The feed entry this process has applied.
    pub fn applied(&self) -> Option<FeedPosition> {
        self.read().applied
    }

    /// Reads the feed and applies what changed, then reports this instance.
    pub async fn refresh(&self) -> Result<(), ConfigStoreError> {
        let _refreshing = self.inner.refreshing.lock().await;
        self.refresh_locked().await
    }

    async fn refresh_locked(&self) -> Result<(), ConfigStoreError> {
        let store = &self.inner.store;
        let mut snapshot = store.snapshot().await?;
        let head = snapshot.head().await?;
        let (applied, first) = {
            let loaded = self.read();
            (loaded.applied, loaded.loaded_at.is_none())
        };
        let lost = match applied {
            Some(position) => !snapshot.still_applied(position).await?,
            None => false,
        };
        // Nothing applied yet but entries now exist: the first publication
        // after an empty start.
        let full = first || lost || (applied.is_none() && head.is_some());
        match (applied, head) {
            _ if full => {
                if lost {
                    tracing::warn!(
                        sequence = applied.map(|position| position.sequence),
                        "the applied configuration entry is gone (restore or failover); reloading"
                    );
                }
                let versions = match head {
                    Some(head) => snapshot.served(head.sequence, None).await?,
                    None => Vec::new(),
                };
                let mut loaded = self.write();
                loaded.organizations = versions
                    .into_iter()
                    .map(|version| (version.organization_id, Arc::new(version)))
                    .collect();
                loaded.applied = head;
                loaded.loaded_at = Some(Utc::now());
                crate::metrics::global().inc(
                    &crate::metrics::CONFIG_RELOADS,
                    &[if lost { "entry_lost" } else { "start" }],
                );
            }
            (Some(applied), Some(head)) if head.sequence > applied.sequence => {
                let changed = snapshot.changed(applied.sequence, head.sequence).await?;
                let versions = snapshot.served(head.sequence, Some(&changed)).await?;
                let mut loaded = self.write();
                for organization in &changed {
                    loaded.organizations.remove(organization);
                }
                for version in versions {
                    loaded
                        .organizations
                        .insert(version.organization_id, Arc::new(version));
                }
                loaded.applied = Some(head);
                loaded.loaded_at = Some(Utc::now());
            }
            _ => {}
        }
        drop(snapshot);
        let (applied, loaded_at) = {
            let loaded = self.read();
            (loaded.applied, loaded.loaded_at)
        };
        store
            .report(
                &self.inner.instance,
                self.inner.profile,
                self.inner.started_at,
                applied,
                loaded_at,
            )
            .await?;
        let now = Instant::now();
        *self
            .inner
            .last_success
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(now);
        *LAST_SUCCESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(now);
        crate::metrics::global().set(
            &crate::metrics::CONFIG_APPLIED_SEQUENCE,
            &[],
            applied.map_or(0, |position| position.sequence),
        );
        Ok(())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Loaded> {
        self.inner
            .loaded
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Loaded> {
        self.inner
            .loaded
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
