// End users of an app (D-028): a service account's settings and plans, its
// end users' plans, and their credit grants and reversals.
//
// Reads need `budget_read`. Settings and plans need `budget_write`; assigning
// plans and granting or reversing credits need `end_user_write`, so an app's
// backend can do both with an agent token without being able to change
// budgets. Writes run through the admin mutation coordinator, so each is
// idempotent, versioned and audited in the same transaction as the change.
// A grant is also idempotent by its reference, whichever token sends it.

use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::application::admin_identities::MutationOutcome;
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    end_user::{
        self, CreditGrantInput, CreditGrantView, CreditReversalInput, CreditReversalView,
        EndUserInput, EndUserInputError, EndUserSettingsInput, EndUserSettingsView, EndUserView,
        PlanInput, PlanPeriod, PlanSpec, PlanView, parse_end_user_id, valid_plan_name,
    },
    identity::{OrganizationId, ServiceAccountId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{
        AdminMutation, AdminMutationCoordinator, AdminMutationRequest, AdminStore, AdminStoreError,
        MutationStart,
    },
    end_users::{EndUserStore, EndUserWriteError, Written},
};

#[cfg(test)]
const BASE: &str = "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}";
pub const ROUTE_SETTINGS: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-user-settings";
pub const ROUTE_PLANS: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-user-plans";
pub const ROUTE_PLAN: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-user-plans/{plan}";
pub const ROUTE_END_USERS: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-users";
pub const ROUTE_END_USER: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-users/{end_user}";
pub const ROUTE_GRANTS: &str = "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-users/{end_user}/credit-grants";
pub const ROUTE_REVERSALS: &str = "/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}/end-users/{end_user}/credit-grants/{grant_id}/reversals";

const COLLECTION: &str = "end_users";
const ORDER_ID: &str = "external_id";

#[derive(Debug)]
pub enum EndUserAdminError {
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    /// A body field the request got wrong: the problem code and detail.
    Invalid(&'static str, String),
    Pagination(PaginationError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    SettingsMissing,
    CurrencyLocked,
    PlanInUse,
    CreditReferenceConflict,
    ReversalReferenceConflict,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for EndUserAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

impl From<EndUserWriteError> for EndUserAdminError {
    fn from(error: EndUserWriteError) -> Self {
        match error {
            EndUserWriteError::NotFound => Self::NotFound,
            EndUserWriteError::SettingsMissing => Self::SettingsMissing,
            EndUserWriteError::UnknownPlan => Self::Invalid(
                "unknown_end_user_plan",
                "no plan of the service account has that name".into(),
            ),
            EndUserWriteError::Expired => Self::Invalid(
                "invalid_credit_grant",
                EndUserInputError::Expiry.to_string(),
            ),
            EndUserWriteError::CurrencyLocked => Self::CurrencyLocked,
            EndUserWriteError::PlanInUse => Self::PlanInUse,
            EndUserWriteError::CreditReferenceConflict => Self::CreditReferenceConflict,
            EndUserWriteError::ReversalReferenceConflict => Self::ReversalReferenceConflict,
            EndUserWriteError::Store(error) => Self::from(error),
        }
    }
}

/// A write's result: what to return, whether it created the resource, and
/// whether an earlier request had already made it (and was replayed).
#[derive(Clone, Debug)]
pub struct EndUserWrite<T> {
    pub item: T,
    pub created: bool,
    /// The `ETag` version, for versioned resources.
    pub version: Option<u64>,
}

/// Where a view's version goes, for resources the `ETag` versions.
trait Versioned {
    fn set_version(&mut self, version: u64) -> Option<u64>;
}

impl Versioned for EndUserSettingsView {
    fn set_version(&mut self, version: u64) -> Option<u64> {
        self.version = version;
        Some(version)
    }
}

impl Versioned for PlanView {
    fn set_version(&mut self, version: u64) -> Option<u64> {
        self.version = version;
        Some(version)
    }
}

impl Versioned for EndUserView {
    fn set_version(&mut self, version: u64) -> Option<u64> {
        self.version = version;
        Some(version)
    }
}

impl Versioned for CreditGrantView {
    fn set_version(&mut self, _: u64) -> Option<u64> {
        None
    }
}

impl Versioned for CreditReversalView {
    fn set_version(&mut self, _: u64) -> Option<u64> {
        None
    }
}

#[derive(Clone)]
pub struct EndUserAdminService {
    admin: AdminStore,
    coordinator: AdminMutationCoordinator,
    store: EndUserStore,
    codec: Option<SignedCursorCodec>,
}

/// Who asks, about which service account.
#[derive(Clone, Copy, Debug)]
pub struct Subject {
    pub actor: UserAccountId,
    pub organization_id: OrganizationId,
    pub service_account_id: ServiceAccountId,
}

/// The headers of a write.
#[derive(Clone, Debug)]
pub struct WriteContext {
    pub if_match: IfMatch,
    /// `None` where the route derives one (grants and reversals).
    pub idempotency_key: Option<AdminIdempotencyKey>,
    pub request_id: Uuid,
}

impl EndUserAdminService {
    pub fn new(admin: AdminStore, store: EndUserStore, codec: Option<SignedCursorCodec>) -> Self {
        Self {
            coordinator: admin.coordinator(),
            admin,
            store,
            codec,
        }
    }

    async fn authorize(
        &self,
        subject: Subject,
        permission: AdminPermission,
    ) -> Result<(), EndUserAdminError> {
        let context = self
            .admin
            .authorization_context(subject.actor, subject.organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(
            permission,
            AdminScope::Organization(subject.organization_id),
        ) {
            Ok(())
        } else {
            Err(EndUserAdminError::Forbidden)
        }
    }

    /// A read on a connection, after `budget_read` and the service account
    /// are checked.
    async fn reader(
        &self,
        subject: Subject,
    ) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>, EndUserAdminError> {
        self.authorize(subject, AdminPermission::BudgetRead).await?;
        let mut conn = self.store.connection().await?;
        if !EndUserStore::service_account_exists(
            &mut conn,
            subject.organization_id.0,
            subject.service_account_id.0,
        )
        .await?
        {
            return Err(EndUserAdminError::NotFound);
        }
        Ok(conn)
    }

    pub async fn settings(
        &self,
        subject: Subject,
    ) -> Result<EndUserSettingsView, EndUserAdminError> {
        let mut conn = self.reader(subject).await?;
        EndUserStore::settings(
            &mut conn,
            subject.organization_id.0,
            subject.service_account_id.0,
        )
        .await?
        .ok_or(EndUserAdminError::NotFound)
    }

    pub async fn plans(&self, subject: Subject) -> Result<Vec<PlanView>, EndUserAdminError> {
        let mut conn = self.reader(subject).await?;
        Ok(EndUserStore::plans(
            &mut conn,
            subject.organization_id.0,
            subject.service_account_id.0,
            chrono::Utc::now(),
        )
        .await?)
    }

    pub async fn end_user(
        &self,
        subject: Subject,
        external_id: &str,
    ) -> Result<EndUserView, EndUserAdminError> {
        let external_id = end_user_path_id(external_id)?;
        let mut conn = self.reader(subject).await?;
        EndUserStore::end_user(
            &mut conn,
            subject.organization_id.0,
            subject.service_account_id.0,
            &external_id,
            chrono::Utc::now(),
        )
        .await?
        .ok_or(EndUserAdminError::NotFound)
    }

    pub async fn grants(
        &self,
        subject: Subject,
        external_id: &str,
    ) -> Result<Vec<CreditGrantView>, EndUserAdminError> {
        let external_id = end_user_path_id(external_id)?;
        let mut conn = self.reader(subject).await?;
        let now = chrono::Utc::now();
        let (organization, account) = (subject.organization_id.0, subject.service_account_id.0);
        if EndUserStore::end_user(&mut conn, organization, account, &external_id, now)
            .await?
            .is_none()
        {
            return Err(EndUserAdminError::NotFound);
        }
        Ok(EndUserStore::grants(&mut conn, organization, account, &external_id, now).await?)
    }

    /// The service account's end users by ID, paged.
    pub async fn end_users(
        &self,
        subject: Subject,
        page: &PageRequest,
    ) -> Result<Page<EndUserView>, EndUserAdminError> {
        drop(self.reader(subject).await?);
        let scope = AdminScope::Organization(subject.organization_id);
        let codec = self
            .codec
            .as_ref()
            .ok_or(EndUserAdminError::AuthenticationUnavailable)?;
        let query = subject.service_account_id.0.to_string();
        let after = match page.page_token.as_deref() {
            Some(token) => Some(
                codec
                    .decode_and_validate(token, subject.actor, scope, COLLECTION, &query, ORDER_ID)
                    .map_err(EndUserAdminError::Pagination)?
                    .last_keyset,
            ),
            None => None,
        };
        let (items, has_more) = self
            .store
            .end_users(
                subject.organization_id.0,
                subject.service_account_id.0,
                page.page_size,
                after.as_deref(),
                chrono::Utc::now(),
            )
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => {
                let claims = CursorClaims::new(
                    subject.actor,
                    scope,
                    COLLECTION,
                    &query,
                    ORDER_ID,
                    last.id.clone(),
                    None,
                )
                .map_err(EndUserAdminError::Pagination)?;
                Some(
                    codec
                        .encode(&claims)
                        .map_err(EndUserAdminError::Pagination)?,
                )
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn put_settings(
        &self,
        subject: Subject,
        input: EndUserSettingsInput,
        context: WriteContext,
    ) -> Result<MutationOutcome<EndUserWrite<EndUserSettingsView>>, EndUserAdminError> {
        let input = input
            .validate()
            .map_err(invalid("invalid_end_user_settings"))?;
        let target_id = end_user::setting_id(subject.service_account_id.0);
        let request = self.request(
            subject,
            &context,
            Mutation {
                permission: AdminPermission::BudgetWrite,
                route: ROUTE_SETTINGS,
                method: "PUT",
                operation: "end_user_setting.set",
                action: "end_user_setting.set",
                target: AuditTargetResource::EndUserSetting,
                target_id,
            },
            serde_json::to_value(&input).unwrap_or_default(),
        )?;
        let mut mutation = match self.begin_or_replay(request).await? {
            Begun::Replay(outcome) => return Ok(outcome),
            Begun::New(mutation) => mutation,
        };
        let written = EndUserStore::put_settings(
            mutation.transaction(),
            subject.organization_id.0,
            subject.service_account_id.0,
            &input,
        )
        .await?;
        finish(mutation, target_id, written).await
    }

    pub async fn put_plan(
        &self,
        subject: Subject,
        name: &str,
        input: PlanInput,
        context: WriteContext,
    ) -> Result<MutationOutcome<EndUserWrite<PlanView>>, EndUserAdminError> {
        if !valid_plan_name(name) {
            return Err(invalid("invalid_end_user_plan")(
                EndUserInputError::PlanName,
            ));
        }
        let plan = input.validate().map_err(invalid("invalid_end_user_plan"))?;
        let target_id = end_user::plan_id(subject.service_account_id.0, name);
        let request = self.request(
            subject,
            &context,
            Mutation {
                permission: AdminPermission::BudgetWrite,
                route: ROUTE_PLAN,
                method: "PUT",
                operation: "end_user_plan.set",
                action: "end_user_plan.set",
                target: AuditTargetResource::EndUserPlan,
                target_id,
            },
            plan_body(name, &plan),
        )?;
        let mut mutation = match self.begin_or_replay(request).await? {
            Begun::Replay(outcome) => return Ok(outcome),
            Begun::New(mutation) => mutation,
        };
        let written = EndUserStore::put_plan(
            mutation.transaction(),
            subject.organization_id.0,
            subject.service_account_id.0,
            name,
            &plan,
            chrono::Utc::now(),
        )
        .await?;
        finish(mutation, target_id, written).await
    }

    pub async fn delete_plan(
        &self,
        subject: Subject,
        name: &str,
        context: WriteContext,
    ) -> Result<MutationOutcome<()>, EndUserAdminError> {
        let target_id = end_user::plan_id(subject.service_account_id.0, name);
        let request = self.request(
            subject,
            &context,
            Mutation {
                permission: AdminPermission::BudgetWrite,
                route: ROUTE_PLAN,
                method: "DELETE",
                operation: "end_user_plan.delete",
                action: "end_user_plan.deleted",
                target: AuditTargetResource::EndUserPlan,
                target_id,
            },
            serde_json::json!({ "name": name }),
        )?;
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(_) => Ok(MutationOutcome {
                item: (),
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                EndUserStore::delete_plan(
                    mutation.transaction(),
                    subject.organization_id.0,
                    subject.service_account_id.0,
                    name,
                )
                .await?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        previous(next).map(|version| snapshot(Some(version))),
                        None,
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: (),
                    replayed: false,
                })
            }
        }
    }

    pub async fn put_end_user(
        &self,
        subject: Subject,
        external_id: &str,
        input: EndUserInput,
        context: WriteContext,
    ) -> Result<MutationOutcome<EndUserWrite<EndUserView>>, EndUserAdminError> {
        let external_id = end_user_path_id(external_id)?;
        let target_id = end_user::end_user_id(subject.service_account_id.0, &external_id);
        let request = self.request(
            subject,
            &context,
            Mutation {
                permission: AdminPermission::EndUserWrite,
                route: ROUTE_END_USER,
                method: "PUT",
                operation: "end_user.set",
                action: "end_user.set",
                target: AuditTargetResource::EndUser,
                target_id,
            },
            serde_json::json!({ "end_user": external_id, "plan": input.plan }),
        )?;
        let mut mutation = match self.begin_or_replay(request).await? {
            Begun::Replay(outcome) => return Ok(outcome),
            Begun::New(mutation) => mutation,
        };
        let written = EndUserStore::put_end_user(
            mutation.transaction(),
            subject.organization_id.0,
            subject.service_account_id.0,
            &external_id,
            input.plan.as_deref(),
            chrono::Utc::now(),
        )
        .await?;
        finish(mutation, target_id, written).await
    }

    pub async fn grant(
        &self,
        subject: Subject,
        external_id: &str,
        input: CreditGrantInput,
        context: WriteContext,
    ) -> Result<MutationOutcome<EndUserWrite<CreditGrantView>>, EndUserAdminError> {
        let external_id = end_user_path_id(external_id)?;
        let now = chrono::Utc::now();
        let input = input.validate().map_err(invalid("invalid_credit_grant"))?;
        let target_id = end_user::grant_id(subject.service_account_id.0, &input.reference);
        let derived_key = context.idempotency_key.is_none();
        let context = WriteContext {
            idempotency_key: Some(match context.idempotency_key {
                Some(key) => key,
                None => derived(&format!("credit-grant:{target_id}"))?,
            }),
            ..context
        };
        let request = self.request(
            subject,
            &context,
            Mutation {
                permission: AdminPermission::EndUserWrite,
                route: ROUTE_GRANTS,
                method: "POST",
                operation: "end_user_credit_grant.create",
                action: "end_user_credit_grant.created",
                target: AuditTargetResource::EndUserCreditGrant,
                target_id,
            },
            serde_json::json!({
                "end_user": external_id,
                "reference": input.reference,
                "amount_minor": input.amount_minor,
                "expires_at": input.expires_at,
            }),
        )?;
        let begun = self
            .begin_or_replay(request)
            .await
            .map_err(|error| match error {
                // The key is the reference's: another body is another purchase
                // under a reference already used.
                EndUserAdminError::IdempotencyConflict if derived_key => {
                    EndUserAdminError::CreditReferenceConflict
                }
                other => other,
            })?;
        let mut mutation = match begun {
            Begun::Replay(outcome) => return Ok(outcome),
            Begun::New(mutation) => mutation,
        };
        let written = EndUserStore::grant(
            mutation.transaction(),
            subject.organization_id.0,
            subject.service_account_id.0,
            &external_id,
            &input,
            subject.actor.0,
            now,
        )
        .await?;
        finish(mutation, target_id, written).await
    }

    pub async fn reverse(
        &self,
        subject: Subject,
        external_id: &str,
        grant_id: Uuid,
        input: CreditReversalInput,
        context: WriteContext,
    ) -> Result<MutationOutcome<EndUserWrite<CreditReversalView>>, EndUserAdminError> {
        let external_id = end_user_path_id(external_id)?;
        let input = input
            .validate()
            .map_err(invalid("invalid_credit_reversal"))?;
        let derived_key = context.idempotency_key.is_none();
        let reversal_id = end_user::reversal_id(grant_id, &input.reference);
        let context = WriteContext {
            idempotency_key: Some(match context.idempotency_key {
                Some(key) => key,
                None => derived(&format!("credit-reversal:{reversal_id}"))?,
            }),
            ..context
        };
        let request = self.request(
            subject,
            &context,
            Mutation {
                permission: AdminPermission::EndUserWrite,
                route: ROUTE_REVERSALS,
                method: "POST",
                operation: "end_user_credit_reversal.create",
                action: "end_user_credit_grant.reversed",
                target: AuditTargetResource::EndUserCreditGrant,
                target_id: grant_id,
            },
            serde_json::json!({
                "end_user": external_id,
                "reference": input.reference,
                "amount_minor": input.amount_minor,
                "reason": input.reason,
            }),
        )?;
        let begun = self
            .begin_or_replay(request)
            .await
            .map_err(|error| match error {
                EndUserAdminError::IdempotencyConflict if derived_key => {
                    EndUserAdminError::ReversalReferenceConflict
                }
                other => other,
            })?;
        let mut mutation = match begun {
            Begun::Replay(outcome) => return Ok(outcome),
            Begun::New(mutation) => mutation,
        };
        let written = EndUserStore::reverse(
            mutation.transaction(),
            subject.organization_id.0,
            subject.service_account_id.0,
            &external_id,
            grant_id,
            &input,
            subject.actor.0,
            chrono::Utc::now(),
        )
        .await?;
        finish(mutation, grant_id, written).await
    }

    fn request(
        &self,
        subject: Subject,
        context: &WriteContext,
        mutation: Mutation,
        body: serde_json::Value,
    ) -> Result<AdminMutationRequest, EndUserAdminError> {
        let idempotency_key = context
            .idempotency_key
            .clone()
            .ok_or_else(|| EndUserAdminError::Store("an idempotency key is required".into()))?;
        let mut body = body;
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "service_account_id".into(),
                serde_json::json!(subject.service_account_id.0),
            );
        }
        Ok(AdminMutationRequest {
            actor_id: subject.actor,
            organization_id: subject.organization_id,
            scope: AdminScope::Organization(subject.organization_id),
            permission: mutation.permission,
            route: mutation.route.into(),
            method: mutation.method.into(),
            operation: AdminOperation::new(mutation.operation)
                .expect("operation constant is valid"),
            idempotency_key,
            target_id: mutation.target_id,
            parent_id: None,
            target: mutation.target,
            audit_action: AuditAction::new(mutation.action)
                .expect("audit action constant is valid"),
            if_match: context.if_match,
            request_id: context.request_id,
            body,
        })
    }

    async fn begin_or_replay<T: DeserializeOwned>(
        &self,
        request: AdminMutationRequest,
    ) -> Result<Begun<'_, T>, EndUserAdminError> {
        match self
            .coordinator
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // The recorded response, even if the resource has since changed.
            MutationStart::Replay(record) => {
                let metadata = record.response_metadata;
                let version = metadata.as_ref().and_then(|metadata| metadata.version);
                let snapshot = metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(EndUserAdminError::NotFound)?;
                let item = serde_json::from_value(snapshot).map_err(|_| {
                    EndUserAdminError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(Begun::Replay(MutationOutcome {
                    item: EndUserWrite {
                        item,
                        created: false,
                        version,
                    },
                    replayed: true,
                }))
            }
            MutationStart::New(mutation) => Ok(Begun::New(mutation)),
        }
    }
}

// Short-lived: matched at once by the caller.
#[allow(clippy::large_enum_variant)]
enum Begun<'a, T> {
    Replay(MutationOutcome<EndUserWrite<T>>),
    New(AdminMutation<'a>),
}

struct Mutation {
    permission: AdminPermission,
    route: &'static str,
    method: &'static str,
    operation: &'static str,
    action: &'static str,
    target: AuditTargetResource,
    target_id: Uuid,
}

/// Commit a write with its audit and its recorded response, or, where an
/// earlier request already made it, return that and record nothing.
async fn finish<T: Serialize + Versioned>(
    mutation: AdminMutation<'_>,
    resource_id: Uuid,
    written: Written<T>,
) -> Result<MutationOutcome<EndUserWrite<T>>, EndUserAdminError> {
    let (mut item, created) = match written {
        // Nothing changed: the mutation is dropped, which rolls it back and
        // releases the idempotency claim.
        Written::Existing(item) => {
            drop(mutation);
            return Ok(MutationOutcome {
                item: EndUserWrite {
                    item,
                    created: false,
                    version: None,
                },
                replayed: false,
            });
        }
        Written::Created(item) => (item, true),
        Written::Updated(item) => (item, false),
    };
    let next = mutation.next_version();
    let version = item.set_version(next.get());
    let response = serde_json::to_value(&item)
        .map_err(|_| EndUserAdminError::Store("response snapshot is not serializable".into()))?;
    mutation
        .commit(
            (!created).then(|| snapshot(previous(next))),
            Some(snapshot(Some(next.get()))),
            Some(IdempotencyResultMetadata {
                resource_id,
                version: Some(next.get()),
                secret_unavailable_on_replay: false,
                snapshot: Some(response),
            }),
        )
        .await
        .map_err(map_admin_error)?;
    Ok(MutationOutcome {
        item: EndUserWrite {
            item,
            created,
            version,
        },
        replayed: false,
    })
}

/// A plan write's recorded body. A monthly plan without limits records only
/// what such a write always did, and `max_concurrent` only when set, so a
/// retry across an upgrade replays.
fn plan_body(name: &str, plan: &PlanSpec) -> serde_json::Value {
    let mut body = serde_json::json!({ "name": name, "allowance_minor": plan.allowance_minor });
    if plan.period != PlanPeriod::Monthly || !plan.limits.is_empty() {
        body["period"] = plan.period.as_str().into();
        body["period_days"] = plan.period.days().into();
        body["requests_per_minute"] = plan.limits.requests_per_minute.into();
        body["tokens_per_minute"] = plan.limits.tokens_per_minute.into();
    }
    // Recorded only when set, so a write from before plans had it replays.
    if let Some(max_concurrent) = plan.limits.max_concurrent {
        body["max_concurrent"] = max_concurrent.into();
    }
    body
}

/// The `{end_user}` path segment, already percent-decoded by the router.
fn end_user_path_id(value: &str) -> Result<String, EndUserAdminError> {
    parse_end_user_id(value)
        .map_err(|error| EndUserAdminError::Invalid("end_user_invalid", error.to_string()))
}

fn invalid(code: &'static str) -> impl Fn(EndUserInputError) -> EndUserAdminError {
    move |error| EndUserAdminError::Invalid(code, error.to_string())
}

fn derived(value: &str) -> Result<AdminIdempotencyKey, EndUserAdminError> {
    AdminIdempotencyKey::new(value)
        .map_err(|_| EndUserAdminError::Store("derived idempotency key is invalid".into()))
}

fn snapshot(version: Option<u64>) -> AuditSnapshot {
    AuditSnapshot {
        enabled: Some(true),
        version,
        ..Default::default()
    }
}

fn previous(next: ResourceVersion) -> Option<u64> {
    next.get().checked_sub(1).filter(|version| *version > 0)
}

fn map_admin_error(error: AdminStoreError) -> EndUserAdminError {
    match error {
        AdminStoreError::Authorization(_) => EndUserAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => EndUserAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => EndUserAdminError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => EndUserAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => EndUserAdminError::IdempotencyInProgress,
        other => EndUserAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-end-users-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_SETTINGS,
            ROUTE_PLANS,
            ROUTE_PLAN,
            ROUTE_END_USERS,
            ROUTE_END_USER,
            ROUTE_GRANTS,
            ROUTE_REVERSALS,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
        assert!(routes.iter().all(|route| route.starts_with(BASE)));
    }
}
