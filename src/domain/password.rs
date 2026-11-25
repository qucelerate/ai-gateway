//! Local accounts: people who sign in with an email address and a password
//! the gateway keeps, instead of through an OpenID Connect provider. They
//! let a new installation be tried without registering it anywhere.
//!
//! A local account is still a user keyed on `(issuer, subject)`: the issuer
//! is [`LOCAL_ISSUER`], which no OIDC provider can have (it is not an https
//! URL), and the subject is the user's own id. Passwords are hashed with
//! Argon2id; only the PHC string is stored.

use std::{collections::HashSet, sync::OnceLock, time::Duration};

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};

/// The issuer of every local account.
pub const LOCAL_ISSUER: &str = "urn:gateway:local";

/// Shortest accepted password, in characters.
pub const MIN_PASSWORD_CHARS: usize = 12;
/// Longest accepted password, in characters; bounds hashing work.
pub const MAX_PASSWORD_CHARS: usize = 256;

/// Failed sign-ins an account may have before each further one locks it.
pub const FREE_FAILURES: u32 = 5;
/// The longest an account is locked after repeated failures.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Why a new password is refused. The admin UI shows each as a sentence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasswordRejection {
    TooShort,
    TooLong,
    /// One of the most common passwords, or one with only digits or
    /// symbols added around it, or digits alone.
    TooCommon,
    /// Contains the account's email address or its name part.
    ContainsEmail,
    /// Fewer than four different characters.
    TooRepetitive,
}

impl PasswordRejection {
    pub const fn code(self) -> &'static str {
        match self {
            Self::TooShort => "password_too_short",
            Self::TooLong => "password_too_long",
            Self::TooCommon => "password_too_common",
            Self::ContainsEmail => "password_contains_email",
            Self::TooRepetitive => "password_too_repetitive",
        }
    }

    pub const fn detail(self) -> &'static str {
        match self {
            Self::TooShort => "the password must have at least 12 characters",
            Self::TooLong => "the password must have at most 256 characters",
            Self::TooCommon => "the password is too common; choose one that is harder to guess",
            Self::ContainsEmail => "the password must not contain your email address",
            Self::TooRepetitive => "the password must use at least four different characters",
        }
    }
}

/// An email address as accounts are keyed on: trimmed and lowercased.
/// `None` for anything not shaped like `name@domain.tld`.
pub fn normalize_email(value: &str) -> Option<String> {
    let email = value.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    let valid = (3..=254).contains(&email.len())
        && !local.is_empty()
        && local.len() <= 64
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && email
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && c != '<' && c != '>');
    valid.then_some(email)
}

/// The domain of a normalized email address.
pub fn email_domain(email: &str) -> Option<&str> {
    email.rsplit_once('@').map(|(_, domain)| domain)
}

fn common_passwords() -> &'static HashSet<&'static str> {
    static LIST: OnceLock<HashSet<&'static str>> = OnceLock::new();
    LIST.get_or_init(|| {
        include_str!("common_passwords.txt")
            .lines()
            .filter(|line| !line.is_empty())
            .collect()
    })
}

/// Whether `password` may be set for the account `email`: long enough, not
/// one of the most common passwords (also with digits or symbols added at
/// either end, such as `Password2024!`), not built on the email address,
/// and not one character over and over.
pub fn check_password(password: &str, email: &str) -> Result<(), PasswordRejection> {
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(PasswordRejection::TooShort);
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err(PasswordRejection::TooLong);
    }
    let distinct: HashSet<char> = password.chars().collect();
    if distinct.len() < 4 {
        return Err(PasswordRejection::TooRepetitive);
    }
    let lower = password.to_lowercase();
    let email = email.to_lowercase();
    let name = email.split('@').next().unwrap_or_default();
    if (!email.is_empty() && lower.contains(&email)) || (name.len() >= 4 && lower.contains(name)) {
        return Err(PasswordRejection::ContainsEmail);
    }
    // Digits alone are guessed in that order.
    if password.chars().all(|c| c.is_ascii_digit()) {
        return Err(PasswordRejection::TooCommon);
    }
    let core = lower.trim_matches(|c: char| !c.is_alphabetic());
    let list = common_passwords();
    if list.contains(lower.as_str()) || (core.chars().count() >= 4 && list.contains(core)) {
        return Err(PasswordRejection::TooCommon);
    }
    Ok(())
}

/// OWASP's Argon2id baseline: 19 MiB, two passes, one lane.
fn hasher() -> Argon2<'static> {
    let params = Params::new(19 * 1024, 2, 1, None).expect("valid Argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// The PHC string of `password` under a fresh salt. CPU-heavy: call it off
/// the async executor (`spawn_blocking`).
pub fn hash_password(password: &str) -> Result<String, PasswordHashError> {
    let salt = SaltString::generate(&mut OsRng);
    hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| PasswordHashError)
}

/// Whether `password` matches the stored PHC string. A malformed hash
/// matches nothing. CPU-heavy, like [`hash_password`].
pub fn verify_password(stored: &str, password: &str) -> bool {
    let Ok(hash) = PasswordHash::new(stored) else {
        return false;
    };
    hasher().verify_password(password.as_bytes(), &hash).is_ok()
}

/// Does the work of a verification for an unknown email address, so the
/// answer takes as long as for a known one.
pub fn verify_nothing(password: &str) {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let dummy = DUMMY
        .get_or_init(|| hash_password("gateway-unknown-account-placeholder").unwrap_or_default());
    let _ = verify_password(dummy, password);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the password could not be hashed")]
pub struct PasswordHashError;

/// How long an account is locked after its `failures`-th failed sign-in in
/// a row: not at all for the first [`FREE_FAILURES`], then 30 seconds,
/// doubling with each further failure up to [`MAX_LOCKOUT`].
pub fn lockout_after(failures: u32) -> Option<Duration> {
    let over = failures.checked_sub(FREE_FAILURES)?;
    let seconds = 30_u64.saturating_mul(1_u64 << over.min(10));
    Some(Duration::from_secs(seconds).min(MAX_LOCKOUT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_normalized_or_refused() {
        assert_eq!(
            normalize_email("  Ada@Example.COM "),
            Some("ada@example.com".into())
        );
        for bad in [
            "",
            "ada",
            "ada@",
            "@example.com",
            "ada@example",
            "ada@.example.com",
            "ada@example..com",
            "a da@example.com",
            "ada@b@example.com",
            "<ada@example.com>",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad}");
        }
        assert_eq!(email_domain("ada@example.com"), Some("example.com"));
    }

    #[test]
    fn weak_passwords_are_refused_with_their_reason() {
        let email = "ada.lovelace@example.com";
        assert_eq!(
            check_password("short1!", email),
            Err(PasswordRejection::TooShort)
        );
        assert_eq!(
            check_password(&"x".repeat(257), email),
            Err(PasswordRejection::TooLong)
        );
        assert_eq!(
            check_password("aaaaaaaaaaaaaaaa", email),
            Err(PasswordRejection::TooRepetitive)
        );
        assert_eq!(
            check_password("ababababcdcdcd", email),
            Ok(()),
            "four distinct characters are enough"
        );
        assert_eq!(
            check_password("Password2024!!", email),
            Err(PasswordRejection::TooCommon)
        );
        assert_eq!(
            check_password("1234567890123", email),
            Err(PasswordRejection::TooCommon)
        );
        assert_eq!(
            check_password("!!Ada.Lovelace!!9", email),
            Err(PasswordRejection::ContainsEmail)
        );
        assert_eq!(
            check_password("correct horse battery staple", email),
            Ok(())
        );
        // Characters, not bytes: twelve accented letters are enough.
        assert_eq!(check_password("éàüöçñßøåæœł", email), Ok(()));
    }

    #[test]
    fn hashes_verify_only_their_password() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "correct horse battery stapler"));
        assert!(!verify_password("not a hash", "anything"));
        // Each hash has its own salt.
        assert_ne!(hash, hash_password("correct horse battery staple").unwrap());
    }

    #[test]
    fn lockouts_grow_after_the_free_failures() {
        assert_eq!(lockout_after(0), None);
        assert_eq!(lockout_after(4), None);
        assert_eq!(lockout_after(5), Some(Duration::from_secs(30)));
        assert_eq!(lockout_after(6), Some(Duration::from_secs(60)));
        assert_eq!(lockout_after(9), Some(Duration::from_secs(480)));
        assert_eq!(lockout_after(10), Some(MAX_LOCKOUT));
        assert_eq!(lockout_after(u32::MAX), Some(MAX_LOCKOUT));
    }
}
