//! Choosing the deployment that serves a request when an alias routes to
//! several (D-023, Phase 9): by weight among the healthy deployments of the
//! lowest priority that has any.
//!
//! A deployment is unhealthy while its circuit is open (or its probe is in
//! flight), while its provider's 429 retry-after lasts, or while its
//! availability evidence (D-018) is `refused` or `requires_action`. Circuits
//! and throttling are what this process has seen; evidence is the
//! organization's, read from the database at most every [`EVIDENCE_TTL`] and
//! only for routes with more than one deployment, so routes of one
//! deployment cost nothing new.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::accounting::MAX_ROUTE_ATTEMPTS;
use crate::{
    domain::{
        identity::OrganizationId,
        routing::{Choice, pick},
    },
    metrics::{ROUTE_SELECTIONS, global as metrics, provider_family_label},
    persistence::{
        catalog::RouteRow, config_versions::RouteCandidate, model_evidence::ModelEvidenceStore,
    },
};

use super::circuit_breaker::{CircuitBreakers, Unhealthy};

/// How long an organization's evidence is reused before it is read again.
pub(crate) const EVIDENCE_TTL: Duration = Duration::from_secs(30);
/// Organizations whose evidence a process remembers; past this it forgets
/// them all.
const MAX_ORGANIZATIONS: usize = 10_000;

/// `(adapter_instance_key, family, model)`, as evidence is keyed.
type Unavailable = Arc<HashSet<(String, String, String)>>;

/// The models each organization's evidence says it cannot call.
/// An organization's evidence as last read, and the read in progress, if any,
/// that concurrent requests wait for instead of reading it again.
#[derive(Default)]
struct Entry {
    read: Option<(Instant, Unavailable)>,
    reading: Option<Arc<tokio::sync::OnceCell<Unavailable>>>,
}

#[derive(Clone, Default)]
pub(crate) struct RouteEvidence {
    store: Option<ModelEvidenceStore>,
    cache: Arc<Mutex<HashMap<OrganizationId, Entry>>>,
}

impl RouteEvidence {
    pub(crate) fn new(store: ModelEvidenceStore) -> Self {
        Self {
            store: Some(store),
            cache: Arc::default(),
        }
    }

    /// `(adapter_instance_key, family, model)` to skip. One read per
    /// organization at a time: requests that find it stale while it is
    /// being read wait for that read. A failed read skips nothing for
    /// [`EVIDENCE_TTL`]: evidence steers traffic, it never refuses a request.
    async fn unavailable(&self, organization: OrganizationId) -> Unavailable {
        let Some(store) = &self.store else {
            return Unavailable::default();
        };
        let reading = {
            let mut cache = self.lock();
            if let Some((at, models)) = cache
                .get(&organization)
                .and_then(|entry| entry.read.as_ref())
            {
                if at.elapsed() < EVIDENCE_TTL {
                    return models.clone();
                }
            }
            if cache.len() >= MAX_ORGANIZATIONS && !cache.contains_key(&organization) {
                cache.clear();
            }
            cache
                .entry(organization)
                .or_default()
                .reading
                .get_or_insert_with(Default::default)
                .clone()
        };
        let models = reading
            .get_or_init(|| async {
                match store.unavailable(organization).await {
                    Ok(models) => Arc::new(models),
                    Err(error) => {
                        tracing::warn!(
                            organization_id = %organization.0,
                            %error,
                            "reading model evidence for routing failed; no deployment is skipped for it"
                        );
                        Unavailable::default()
                    }
                }
            })
            .await
            .clone();
        let mut cache = self.lock();
        let entry = cache.entry(organization).or_default();
        if entry
            .reading
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &reading))
        {
            entry.reading = None;
            entry.read = Some((Instant::now(), models.clone()));
        }
        models
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<OrganizationId, Entry>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Why a deployment was skipped, for the metric.
fn skipped(reason: Option<Unhealthy>, unavailable: bool) -> Option<&'static str> {
    match reason {
        Some(Unhealthy::CircuitOpen) => Some("skipped_circuit_open"),
        Some(Unhealthy::RateLimited) => Some("skipped_rate_limited"),
        None if unavailable => Some("skipped_unavailable"),
        None => None,
    }
}

/// The weighted draw for a request: from its idempotency key where it has
/// one, so attempts under one key (a retry, or two sent at once before
/// either reserved) draw alike and pick the same deployment while health
/// is unchanged; otherwise at random. Keys are spread uniformly by the hash.
pub(crate) fn roll(
    organization: OrganizationId,
    service_account: uuid::Uuid,
    idempotency_key: Option<&str>,
) -> u64 {
    use sha2::{Digest, Sha256};
    let Some(key) = idempotency_key else {
        return rand::random();
    };
    let mut digest = Sha256::new();
    digest.update(b"qucelerate-gateway/route-roll/v1");
    digest.update(organization.0.as_bytes());
    digest.update(service_account.as_bytes());
    digest.update((key.len() as u64).to_be_bytes());
    digest.update(key.as_bytes());
    let bytes: [u8; 32] = digest.finalize().into();
    u64::from_be_bytes(bytes[..8].try_into().expect("eight bytes"))
}

/// The candidates the organization's routing policy permits, before
/// anything else is considered: a deployment it does not permit is never
/// picked or failed over to. `None` when there were candidates and the
/// policy permits none of them, so the request is refused rather than sent
/// somewhere else; an alias with no routes at all stays empty.
pub(crate) fn permitted(mut candidates: Vec<RouteCandidate>) -> Option<Vec<RouteCandidate>> {
    let before = candidates.len();
    candidates.retain(|candidate| {
        if !candidate.permitted {
            metrics().inc(
                &ROUTE_SELECTIONS,
                &[
                    provider_family_label(Some(&candidate.row.family)),
                    "skipped_policy",
                ],
            );
        }
        candidate.permitted
    });
    (before == 0 || !candidates.is_empty()).then_some(candidates)
}

/// The deployment that serves a request, and those it may fail over to.
#[derive(Debug)]
pub(crate) struct Selection {
    pub row: RouteRow,
    /// The other healthy deployments of the route, preferred first (lowest
    /// priority, then highest weight), at most enough for
    /// [`MAX_ROUTE_ATTEMPTS`] in all.
    pub fallbacks: Vec<RouteRow>,
}

/// The deployment of `candidates` that serves this request, `None` when
/// there are none. One candidate is returned as it is, and so is the one
/// whose route is `bound` (the route the request's latest attempt under its
/// idempotency key reserved), healthy or not and with nothing to fail over
/// to, since the retry replays that attempt. Otherwise `roll` (see
/// [`roll`]) draws by weight among the healthy deployments, and the other
/// healthy ones are its fallbacks.
pub(crate) async fn select(
    organization: OrganizationId,
    mut candidates: Vec<RouteCandidate>,
    bound: Option<uuid::Uuid>,
    roll: u64,
    breakers: &CircuitBreakers,
    evidence: &RouteEvidence,
) -> Option<Selection> {
    let alone = |row: RouteRow| Selection {
        row,
        fallbacks: Vec::new(),
    };
    if candidates.len() <= 1 {
        return candidates.pop().map(|candidate| alone(candidate.row));
    }
    if let Some(index) = bound.and_then(|route_id| {
        candidates
            .iter()
            .position(|candidate| candidate.row.route_id == route_id)
    }) {
        return Some(alone(candidates.swap_remove(index).row));
    }
    let unavailable = evidence.unavailable(organization).await;
    let health: Vec<Option<&'static str>> = candidates
        .iter()
        .map(|candidate| {
            let row = &candidate.row;
            skipped(
                breakers.health(
                    organization,
                    Some(&row.adapter_instance_key),
                    &row.deployment_name,
                ),
                unavailable.contains(&(
                    row.adapter_instance_key.clone(),
                    row.family.clone(),
                    row.model.clone(),
                )),
            )
        })
        .collect();
    let choices: Vec<Choice> = candidates
        .iter()
        .zip(&health)
        .map(|(candidate, skip)| Choice {
            weight: candidate.weight,
            priority: candidate.priority,
            healthy: skip.is_none(),
        })
        .collect();
    let picked = pick(&choices, roll)?;
    let registry = metrics();
    for (index, (candidate, skip)) in candidates.iter().zip(&health).enumerate() {
        let family = provider_family_label(Some(&candidate.row.family));
        if index == picked.index {
            let outcome = if picked.healthy {
                "picked"
            } else {
                "picked_unhealthy"
            };
            registry.inc(&ROUTE_SELECTIONS, &[family, outcome]);
        } else if let Some(skip) = skip {
            registry.inc(&ROUTE_SELECTIONS, &[family, skip]);
        }
    }
    let mut fallbacks: Vec<(u32, std::cmp::Reverse<u32>, RouteRow)> = Vec::new();
    let mut row = None;
    for (index, (candidate, skip)) in candidates.into_iter().zip(health).enumerate() {
        if index == picked.index {
            row = Some(candidate.row);
        } else if skip.is_none() {
            fallbacks.push((
                candidate.priority,
                std::cmp::Reverse(candidate.weight),
                candidate.row,
            ));
        }
    }
    fallbacks.sort_by_key(|(priority, weight, _)| (*priority, *weight));
    Some(Selection {
        row: row?,
        fallbacks: fallbacks
            .into_iter()
            .take(usize::from(MAX_ROUTE_ATTEMPTS) - 1)
            .map(|(_, _, row)| row)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_under_one_key_draw_alike_and_keys_spread() {
        let organization = OrganizationId::new();
        let account = uuid::Uuid::new_v4();
        assert_eq!(
            roll(organization, account, Some("k-1")),
            roll(organization, account, Some("k-1"))
        );
        assert_ne!(
            roll(organization, account, Some("k-1")),
            roll(organization, account, Some("k-2"))
        );
        assert_ne!(
            roll(organization, account, Some("k-1")),
            roll(organization, uuid::Uuid::new_v4(), Some("k-1"))
        );
        // Keys split a 3:1 route close to its weights.
        let choices = [
            Choice {
                weight: 3,
                priority: 1,
                healthy: true,
            },
            Choice {
                weight: 1,
                priority: 1,
                healthy: true,
            },
        ];
        let first = (0..4000)
            .filter(|n| {
                pick(
                    &choices,
                    roll(organization, account, Some(&format!("key-{n}"))),
                )
                .unwrap()
                .index
                    == 0
            })
            .count();
        assert!((2850..=3150).contains(&first), "{first}");
    }
}
