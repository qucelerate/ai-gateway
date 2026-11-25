// Who an admin API request acts as: a person's browser session (the
// `__Host-gateway_session` and browser-binding cookies) or an agent's token
// (`Authorization: Bearer gwa_…`). Both resolve to a user account, so every
// lane's permission checks and audit trail are the same for either. Any
// bearer credential is taken as an agent token: one that is malformed or
// fails is refused, never retried as a session, so a header a client got
// wrong cannot fall back to cookies it also carries.

use axum::http::{HeaderMap, header::AUTHORIZATION};

use crate::{
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
    },
    persistence::auth::AuthStore,
    transport::admin_identities::{BROWSER_COOKIE, SESSION_COOKIE, cookie},
};

/// The bearer credential the request presents, if any: the agent token it
/// is taken as (well-formed or not). A non-UTF-8 `Authorization` header
/// counts as an empty credential, which matches no token.
pub(crate) fn agent_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?;
    let Ok(value) = value.to_str() else {
        return Some("");
    };
    let value = value.trim();
    let scheme = value.split(char::is_whitespace).next()?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| value[scheme.len()..].trim())
}

/// The administrator a request acts as, or `None` when it is not
/// authenticated.
pub(crate) async fn admin_actor(
    auth: &AuthStore,
    crypto: Option<&AuthCrypto>,
    headers: &HeaderMap,
) -> Result<Option<UserAccountId>, sqlx::Error> {
    Ok(scoped_admin_actor(auth, crypto, headers)
        .await?
        .map(|(actor, _)| actor))
}

/// The administrator a request acts as and, for an agent or a session from
/// an organization's own identity provider, the one organization it may act
/// in.
pub(crate) async fn scoped_admin_actor(
    auth: &AuthStore,
    crypto: Option<&AuthCrypto>,
    headers: &HeaderMap,
) -> Result<Option<(UserAccountId, Option<OrganizationId>)>, sqlx::Error> {
    if let Some(token) = agent_token(headers) {
        return Ok(auth
            .find_agent_token(token)
            .await?
            .map(|agent| (agent.agent_id, Some(agent.organization_id))));
    }
    let (Some(crypto), Some(token), Some(browser)) = (
        crypto,
        cookie(headers, SESSION_COOKIE),
        cookie(headers, BROWSER_COOKIE),
    ) else {
        return Ok(None);
    };
    auth.find_scoped_session(&crypto.digest(&token), &crypto.digest(&browser))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn every_bearer_credential_is_taken_as_an_agent_token() {
        let headers = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            headers
        };
        assert_eq!(agent_token(&headers("Bearer gwa_abc")), Some("gwa_abc"));
        assert_eq!(agent_token(&headers("bearer gwa_abc")), Some("gwa_abc"));
        // Odd spacing or another credential is still a bearer attempt, so it
        // never falls back to session cookies.
        assert_eq!(agent_token(&headers("Bearer  gwa_abc ")), Some("gwa_abc"));
        assert_eq!(agent_token(&headers("Bearer\tgwa_abc")), Some("gwa_abc"));
        assert_eq!(agent_token(&headers("Bearer gw_0123")), Some("gw_0123"));
        assert_eq!(agent_token(&headers("Bearer")), Some(""));
        assert_eq!(agent_token(&headers("Basic gwa_abc")), None);
        assert_eq!(agent_token(&headers("gwa_abc")), None);
        assert_eq!(agent_token(&HeaderMap::new()), None);
    }
}
