//! Optimistic concurrency contracts used by admin mutation lanes.

use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ResourceVersion(u64);

use serde::{Deserialize, Serialize};

impl ResourceVersion {
    pub const INITIAL: Self = Self(1);
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    pub const fn next(self) -> Self {
        let next = self.0.saturating_add(1);
        Self(if next == 0 { 1 } else { next })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IfMatch {
    Exact(ResourceVersion),
    Any,
    /// `"v0"`: only if the resource has no version yet, so a client that saw
    /// it unversioned cannot overwrite a version created since.
    Absent,
}

impl IfMatch {
    pub fn parse(value: &str) -> Result<Self, VersionError> {
        if value.trim() == "*" {
            return Ok(Self::Any);
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .ok_or(VersionError::InvalidIfMatch)?;
        let value = value.strip_prefix("v").unwrap_or(value);
        let version = value
            .parse::<u64>()
            .map_err(|_| VersionError::InvalidIfMatch)?;
        if version == 0 {
            return Ok(Self::Absent);
        }
        ResourceVersion::new(version)
            .map(Self::Exact)
            .ok_or(VersionError::InvalidIfMatch)
    }

    pub const fn matches(self, current: ResourceVersion) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => expected.0 == current.0,
            Self::Absent => false,
        }
    }
}

impl fmt::Display for IfMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Exact(v) => write!(f, "\"{}\"", v.0),
            Self::Absent => f.write_str("\"0\""),
        }
    }
}

impl FromStr for IfMatch {
    type Err = VersionError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionConflict {
    pub expected: Option<ResourceVersion>,
    pub current: ResourceVersion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionError {
    InvalidIfMatch,
    Conflict,
}

impl VersionError {
    pub const fn problem_code(self) -> &'static str {
        match self {
            Self::InvalidIfMatch => "if_match_invalid",
            Self::Conflict => "version_conflict",
        }
    }
}

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidIfMatch => "invalid If-Match version",
            Self::Conflict => "resource version conflict",
        })
    }
}
impl std::error::Error for VersionError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn if_match_is_strict_and_version_conflict_is_explicit() {
        let current = ResourceVersion::new(4).unwrap();
        assert!(IfMatch::parse("\"4\"").unwrap().matches(current));
        assert!(IfMatch::parse("*").unwrap().matches(current));
        assert!(!IfMatch::parse("\"3\"").unwrap().matches(current));
        assert!(IfMatch::parse("4").is_err());
        // "v0" asks for a resource with no version yet: it matches none.
        assert_eq!(IfMatch::parse("\"v0\"").unwrap(), IfMatch::Absent);
        assert!(!IfMatch::Absent.matches(ResourceVersion::INITIAL));
    }
}
