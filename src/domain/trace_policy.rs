//! An organization's tracing policy (D-017).
//!
//! Tracing is off until an owner or admin turns it on, for the whole
//! organization or for chosen teams and service accounts. A traced request
//! keeps what was sent and what came back beside its request record; the
//! record itself never carries content and is kept whatever this policy
//! says. Content is stored by default once tracing is on, and an
//! organization that wants traces without it turns content off. Stored
//! content has its own retention period, 30 days unless set, and never
//! outlives its request record.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::identity::{ServiceAccountId, TeamId};

pub const DEFAULT_CONTENT_DAYS: u32 = 30;
pub const MAX_CONTENT_DAYS: u32 = 3650;
/// Teams, and service accounts, one policy may name.
pub const MAX_SELECTED: usize = 200;

/// Which requests are traced.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceMode {
    #[default]
    Off,
    /// Every request of the organization.
    Organization,
    /// Requests of the listed teams' service accounts and of the listed
    /// service accounts.
    Selected,
}

impl TraceMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Organization => "organization",
            Self::Selected => "selected",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "organization" => Some(Self::Organization),
            "selected" => Some(Self::Selected),
            _ => None,
        }
    }
}

/// The policy as set: a write replaces the whole policy. `content` and
/// `content_retention_days` left out take their defaults (on, 30 days).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TracePolicyInput {
    pub mode: TraceMode,
    #[serde(default)]
    pub team_ids: Vec<TeamId>,
    #[serde(default)]
    pub service_account_ids: Vec<ServiceAccountId>,
    #[serde(default = "content_default")]
    pub content: bool,
    #[serde(default = "content_days_default")]
    pub content_retention_days: u32,
}

const fn content_default() -> bool {
    true
}

const fn content_days_default() -> u32 {
    DEFAULT_CONTENT_DAYS
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TracePolicyError {
    #[error("mode selected needs at least one team or service account")]
    NothingSelected,
    #[error("team_ids and service_account_ids are only for mode selected")]
    SelectionWithoutSelected,
    #[error("at most 200 team_ids and 200 service_account_ids")]
    TooManySelected,
    #[error("content_retention_days must be between 1 and 3650")]
    ContentDays,
    #[error("a listed team does not exist in this organization")]
    UnknownTeam,
    #[error("a listed service account does not exist in this organization")]
    UnknownServiceAccount,
}

impl TracePolicyInput {
    /// Checks the input and returns it with each list sorted and without
    /// repeats, so the same selection is always stored the same way.
    pub fn validate(mut self) -> Result<Self, TracePolicyError> {
        let selected = !self.team_ids.is_empty() || !self.service_account_ids.is_empty();
        match self.mode {
            TraceMode::Selected if !selected => return Err(TracePolicyError::NothingSelected),
            TraceMode::Off | TraceMode::Organization if selected => {
                return Err(TracePolicyError::SelectionWithoutSelected);
            }
            _ => {}
        }
        if !(1..=MAX_CONTENT_DAYS).contains(&self.content_retention_days) {
            return Err(TracePolicyError::ContentDays);
        }
        // Counted as sent, repeats included, as the contract states.
        if self.team_ids.len() > MAX_SELECTED || self.service_account_ids.len() > MAX_SELECTED {
            return Err(TracePolicyError::TooManySelected);
        }
        self.team_ids.sort_by_key(|id| id.0);
        self.team_ids.dedup();
        self.service_account_ids.sort_by_key(|id| id.0);
        self.service_account_ids.dedup();
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TracePolicyView {
    pub mode: TraceMode,
    pub team_ids: Vec<Uuid>,
    pub service_account_ids: Vec<Uuid>,
    pub content: bool,
    pub content_retention_days: u32,
    /// 0 until the policy is first set; then the `ETag` version.
    pub version: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

impl TracePolicyView {
    /// An organization that never set a policy traces nothing.
    pub fn unset() -> Self {
        Self {
            mode: TraceMode::Off,
            team_ids: Vec::new(),
            service_account_ids: Vec::new(),
            content: true,
            content_retention_days: DEFAULT_CONTENT_DAYS,
            version: 0,
            updated_at: None,
        }
    }

    /// Whether a request made by `service_account` of `team` is traced.
    /// A request with neither (an organization-wide key) is traced only
    /// when the whole organization is.
    pub fn traces(&self, team: Option<Uuid>, service_account: Option<Uuid>) -> bool {
        match self.mode {
            TraceMode::Off => false,
            TraceMode::Organization => true,
            TraceMode::Selected => {
                team.is_some_and(|team| self.team_ids.contains(&team))
                    || service_account
                        .is_some_and(|account| self.service_account_ids.contains(&account))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(mode: TraceMode) -> TracePolicyInput {
        TracePolicyInput {
            mode,
            team_ids: Vec::new(),
            service_account_ids: Vec::new(),
            content: true,
            content_retention_days: DEFAULT_CONTENT_DAYS,
        }
    }

    #[test]
    fn defaults_store_content_for_thirty_days() {
        let parsed: TracePolicyInput =
            serde_json::from_value(serde_json::json!({"mode": "organization"})).unwrap();
        assert_eq!(parsed, input(TraceMode::Organization));
        let unset = TracePolicyView::unset();
        assert_eq!(unset.mode, TraceMode::Off);
        assert!(unset.content);
        assert_eq!(unset.content_retention_days, 30);
    }

    #[test]
    fn selected_needs_a_selection_and_only_selected_takes_one() {
        assert_eq!(
            input(TraceMode::Selected).validate(),
            Err(TracePolicyError::NothingSelected)
        );
        let team = TeamId(Uuid::new_v4());
        for mode in [TraceMode::Off, TraceMode::Organization] {
            let mut with_team = input(mode);
            with_team.team_ids = vec![team];
            assert_eq!(
                with_team.validate(),
                Err(TracePolicyError::SelectionWithoutSelected)
            );
        }
        let mut selected = input(TraceMode::Selected);
        selected.team_ids = vec![team, team];
        assert_eq!(selected.validate().unwrap().team_ids, [team]);
    }

    #[test]
    fn content_days_and_list_sizes_are_bounded() {
        for days in [0, MAX_CONTENT_DAYS + 1] {
            let mut policy = input(TraceMode::Organization);
            policy.content_retention_days = days;
            assert_eq!(policy.validate(), Err(TracePolicyError::ContentDays));
        }
        let mut policy = input(TraceMode::Selected);
        policy.service_account_ids = (0..=MAX_SELECTED)
            .map(|_| ServiceAccountId(Uuid::new_v4()))
            .collect();
        assert_eq!(policy.validate(), Err(TracePolicyError::TooManySelected));
        // Repeats count: the limit is on the list as sent.
        let mut repeated = input(TraceMode::Selected);
        repeated.team_ids = vec![TeamId(Uuid::new_v4()); MAX_SELECTED + 1];
        assert_eq!(
            repeated.clone().validate(),
            Err(TracePolicyError::TooManySelected)
        );
        repeated.team_ids.truncate(MAX_SELECTED);
        assert_eq!(repeated.validate().unwrap().team_ids.len(), 1);
    }

    #[test]
    fn selected_traces_listed_teams_and_service_accounts_only() {
        let (team, account) = (Uuid::new_v4(), Uuid::new_v4());
        let mut policy = TracePolicyView::unset();
        assert!(!policy.traces(Some(team), Some(account)));
        policy.mode = TraceMode::Organization;
        assert!(policy.traces(None, None));
        policy.mode = TraceMode::Selected;
        policy.team_ids = vec![team];
        assert!(policy.traces(Some(team), Some(Uuid::new_v4())));
        assert!(!policy.traces(Some(Uuid::new_v4()), Some(account)));
        assert!(!policy.traces(None, None));
        policy.service_account_ids = vec![account];
        assert!(policy.traces(None, Some(account)));
    }
}
