//! An organization's routing policy (D-023, Phase 9): the provider families
//! and deployment locations its requests may be sent to. Route selection
//! and failover consider only the deployments it permits, before weights
//! apply; a request that no permitted deployment can serve is refused, never
//! sent somewhere else.
//!
//! `None` for either list permits any. A deployment with no location is not
//! permitted while locations are restricted, since nothing says where it
//! runs.

use std::collections::BTreeSet;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::catalog::ProviderFamily;

/// Provider families a policy may list.
pub const MAX_PROVIDERS: usize = 16;
/// Locations a policy may list.
pub const MAX_LOCATIONS: usize = 64;
/// The longest location, as the catalog stores them.
pub const MAX_LOCATION_CHARS: usize = 255;

/// The organization's policy, as the admin API returns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoutingPolicyView {
    /// Provider families requests may be sent to; `None` for any.
    pub providers: Option<Vec<String>>,
    /// Deployment locations requests may be sent to; `None` for any.
    pub locations: Option<Vec<String>>,
    /// 0 until the policy is first set.
    pub version: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

impl RoutingPolicyView {
    /// The policy of an organization that never set one: anything goes.
    pub fn unset() -> Self {
        Self {
            providers: None,
            locations: None,
            version: 0,
            updated_at: None,
        }
    }
}

/// `PUT` body. A list absent or `null` permits any.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicyInput {
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub locations: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RoutingPolicyError {
    #[error("`providers` must list 1 to {MAX_PROVIDERS} provider families, or be null for any")]
    ProvidersCount,
    #[error("unknown provider family `{0}`")]
    UnknownProvider(String),
    #[error("`locations` must list 1 to {MAX_LOCATIONS} locations, or be null for any")]
    LocationsCount,
    #[error(
        "a location must be 1 to {MAX_LOCATION_CHARS} characters with no surrounding whitespace"
    )]
    InvalidLocation,
}

impl RoutingPolicyInput {
    /// The input checked, its lists sorted and without duplicates.
    pub fn normalized(self) -> Result<Self, RoutingPolicyError> {
        let providers = match self.providers {
            None => None,
            Some(list) => {
                let set: BTreeSet<String> = list.into_iter().collect();
                if set.is_empty() || set.len() > MAX_PROVIDERS {
                    return Err(RoutingPolicyError::ProvidersCount);
                }
                if let Some(unknown) = set
                    .iter()
                    .find(|family| ProviderFamily::from_str(family).is_err())
                {
                    return Err(RoutingPolicyError::UnknownProvider(unknown.clone()));
                }
                Some(set.into_iter().collect())
            }
        };
        let locations = match self.locations {
            None => None,
            Some(list) => {
                if list.iter().any(|location| {
                    location.is_empty()
                        || location.chars().count() > MAX_LOCATION_CHARS
                        || location.trim() != location
                        || location.chars().any(char::is_control)
                }) {
                    return Err(RoutingPolicyError::InvalidLocation);
                }
                let set: BTreeSet<String> = list.into_iter().collect();
                if set.is_empty() || set.len() > MAX_LOCATIONS {
                    return Err(RoutingPolicyError::LocationsCount);
                }
                Some(set.into_iter().collect())
            }
        };
        Ok(Self {
            providers,
            locations,
        })
    }
}

/// Whether a deployment of provider `family` at `location` is one the lists
/// permit.
pub fn permits(
    providers: Option<&[String]>,
    locations: Option<&[String]>,
    family: &str,
    location: Option<&str>,
) -> bool {
    providers.is_none_or(|providers| providers.iter().any(|allowed| allowed == family))
        && locations.is_none_or(|locations| {
            location.is_some_and(|location| locations.iter().any(|allowed| allowed == location))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(providers: Option<&[&str]>, locations: Option<&[&str]>) -> RoutingPolicyInput {
        let owned = |list: &[&str]| list.iter().map(|value| (*value).to_owned()).collect();
        RoutingPolicyInput {
            providers: providers.map(owned),
            locations: locations.map(owned),
        }
    }

    #[test]
    fn lists_are_checked_sorted_and_deduplicated() {
        let normalized = input(
            Some(&["openai", "bedrock", "openai"]),
            Some(&["us-east-1", "eastus2", "us-east-1"]),
        )
        .normalized()
        .unwrap();
        assert_eq!(
            normalized,
            input(
                Some(&["bedrock", "openai"]),
                Some(&["eastus2", "us-east-1"])
            )
        );
        assert_eq!(
            input(None, None).normalized().unwrap(),
            input(None, None),
            "null permits any"
        );
        assert_eq!(
            input(Some(&[]), None).normalized(),
            Err(RoutingPolicyError::ProvidersCount)
        );
        assert_eq!(
            input(Some(&["openrouter"]), None).normalized(),
            Err(RoutingPolicyError::UnknownProvider("openrouter".into()))
        );
        assert_eq!(
            input(None, Some(&[])).normalized(),
            Err(RoutingPolicyError::LocationsCount)
        );
        for bad in ["", " us-east-1", "eu\n"] {
            assert_eq!(
                input(None, Some(&[bad])).normalized(),
                Err(RoutingPolicyError::InvalidLocation),
                "{bad:?}"
            );
        }
        let many: Vec<String> = (0..=MAX_LOCATIONS).map(|n| format!("l{n}")).collect();
        assert_eq!(
            RoutingPolicyInput {
                providers: None,
                locations: Some(many),
            }
            .normalized(),
            Err(RoutingPolicyError::LocationsCount)
        );
    }

    #[test]
    fn a_deployment_is_permitted_only_by_both_lists() {
        let providers = ["bedrock".to_owned()];
        let locations = ["us-east-1".to_owned()];
        assert!(permits(None, None, "openai", None));
        assert!(permits(Some(&providers), None, "bedrock", None));
        assert!(!permits(
            Some(&providers),
            None,
            "openai",
            Some("us-east-1")
        ));
        assert!(permits(None, Some(&locations), "openai", Some("us-east-1")));
        assert!(!permits(
            None,
            Some(&locations),
            "openai",
            Some("eu-west-1")
        ));
        assert!(
            !permits(None, Some(&locations), "openai", None),
            "no location is not a permitted one"
        );
        assert!(permits(
            Some(&providers),
            Some(&locations),
            "bedrock",
            Some("us-east-1")
        ));
        assert!(!permits(
            Some(&providers),
            Some(&locations),
            "bedrock",
            Some("US-EAST-1")
        ));
    }
}
