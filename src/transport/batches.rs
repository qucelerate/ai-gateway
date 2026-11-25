//! The OpenAI-shaped files and batches endpoints (D-021): a client uploads
//! a JSONL input file, creates a batch from it, and reads the batch and its
//! output file back. Only the API key's own service account sees its files
//! and batches.

use axum::{
    Json,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    AppState,
    application::batches::{BatchError, BatchSpec},
    domain::{batch::MAX_FILE_BYTES, identity::ApiKeyScope},
    persistence::{
        AccountingStoreError,
        batches::{BatchFileRow, BatchRow, BatchStoreError},
    },
    transport::inference::{
        ProblemCode, REQUEST_ID_HEADER, accounting_error, authenticate, idempotency, problem,
        problem_with_detail, rate_limited, request_id,
    },
};

pub(crate) const FILES_PATH: &str = "/api/inference/v1/files";
pub(crate) const FILE_PATH: &str = "/api/inference/v1/files/{id}";
pub(crate) const FILE_CONTENT_PATH: &str = "/api/inference/v1/files/{id}/content";
pub(crate) const BATCHES_PATH: &str = "/api/inference/v1/batches";
pub(crate) const BATCH_PATH: &str = "/api/inference/v1/batches/{id}";
pub(crate) const BATCH_CANCEL_PATH: &str = "/api/inference/v1/batches/{id}/cancel";

/// Files and batches serve either inference capability: a batch's lines
/// are then checked against the capability their operation needs.
const SCOPES: [ApiKeyScope; 2] = [ApiKeyScope::Inference, ApiKeyScope::Embeddings];
const FILE_PREFIX: &str = "file-";
pub(crate) const BATCH_PREFIX: &str = crate::domain::batch::BATCH_ID_PREFIX;
const DEFAULT_PAGE: i64 = 20;
const MAX_PAGE: i64 = 100;

/// The files and batches routes, with the upload's own body limit.
pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route(
            FILES_PATH,
            post(upload_file).layer(DefaultBodyLimit::max(MAX_FILE_BYTES + 64 * 1024)),
        )
        .route(FILE_PATH, get(file))
        .route(FILE_CONTENT_PATH, get(file_content))
        .route(BATCHES_PATH, post(create_batch).get(list_batches))
        .route(BATCH_PATH, get(batch))
        .route(BATCH_CANCEL_PATH, post(cancel_batch))
}

pub(crate) fn file_id(id: Uuid) -> String {
    format!("{FILE_PREFIX}{id}")
}

pub(crate) fn batch_id(id: Uuid) -> String {
    format!("{BATCH_PREFIX}{id}")
}

pub(crate) fn parse_id(value: &str, prefix: &str) -> Option<Uuid> {
    value
        .strip_prefix(prefix)
        .and_then(|id| Uuid::parse_str(id).ok())
}

fn unix(at: DateTime<Utc>) -> i64 {
    at.timestamp()
}

fn file_object(file: &BatchFileRow) -> Value {
    json!({
        "id": file_id(file.id),
        "object": "file",
        "bytes": file.bytes,
        "created_at": unix(file.created_at),
        "filename": file.filename,
        "purpose": file.purpose,
    })
}

pub(crate) fn batch_object(batch: &BatchRow) -> Value {
    json!({
        "id": batch_id(batch.id),
        "object": "batch",
        "endpoint": batch.endpoint,
        "errors": batch.errors.as_ref().map(|data| json!({"object": "list", "data": data})),
        "input_file_id": file_id(batch.input_file_id),
        "completion_window": batch.completion_window,
        "status": batch.status,
        "output_file_id": batch.output_file_id.map(file_id),
        "error_file_id": batch.error_file_id.map(file_id),
        "created_at": unix(batch.created_at),
        "in_progress_at": batch.in_progress_at.map(unix),
        "expires_at": unix(batch.expires_at),
        "finalizing_at": batch.finalizing_at.map(unix),
        "completed_at": batch.completed_at.map(unix),
        "failed_at": batch.failed_at.map(unix),
        "expired_at": batch.expired_at.map(unix),
        "cancelling_at": batch.cancelling_at.map(unix),
        "cancelled_at": batch.cancelled_at.map(unix),
        "request_counts": {
            "total": batch.total_lines,
            "completed": batch.completed_lines,
            "failed": batch.failed_lines,
        },
        "metadata": batch.metadata,
    })
}

fn ok(body: Value, id: &str) -> Response {
    let mut response = Json(body).into_response();
    if let Ok(value) = HeaderValue::from_str(id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// A refused input file: the problem, plus OpenAI's `errors` list naming
/// each refused line.
fn validation_failed(errors: &[crate::domain::batch::LineError], path: &str, id: &str) -> Response {
    let code = ProblemCode::BatchValidationFailed;
    let mut response = problem(StatusCode::BAD_REQUEST, code, path, id);
    let body = json!({
        "type": format!("https://gateway.invalid/problems/{}", code.code()),
        "code": code.code(),
        "title": "Bad Request",
        "status": 400,
        "detail": code.detail(),
        "instance": path,
        "request_id": id,
        "errors": {
            "object": "list",
            "data": errors.iter().map(|error| json!({
                "code": error.code,
                "message": error.message,
                "line": error.line,
                "param": null,
            })).collect::<Vec<_>>(),
        },
    });
    let headers = std::mem::take(response.headers_mut());
    let extensions = std::mem::take(response.extensions_mut());
    let mut replaced = (StatusCode::BAD_REQUEST, Json(body)).into_response();
    *replaced.headers_mut() = headers;
    *replaced.extensions_mut() = extensions;
    replaced
}

fn batch_error(error: BatchError, path: &str, id: &str) -> Response {
    let unavailable = |code| problem(StatusCode::SERVICE_UNAVAILABLE, code, path, id);
    match error {
        BatchError::StoreUnavailable => unavailable(ProblemCode::BatchStoreUnavailable),
        BatchError::FileNotFound => {
            problem(StatusCode::NOT_FOUND, ProblemCode::FileNotFound, path, id)
        }
        BatchError::BatchNotFound => {
            problem(StatusCode::NOT_FOUND, ProblemCode::BatchNotFound, path, id)
        }
        BatchError::InvalidPurpose => problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::FilePurposeInvalid,
            path,
            id,
        ),
        BatchError::FileTooLarge => problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            ProblemCode::RequestTooLarge,
            path,
            id,
        ),
        BatchError::InvalidRequest(_) => problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::BatchRequestInvalid,
            path,
            id,
        ),
        BatchError::Invalid(errors) => validation_failed(&errors, path, id),
        BatchError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ProblemCode::IdempotencyConflict,
            path,
            id,
        ),
        BatchError::Store(BatchStoreError::Accounting(AccountingStoreError::RateLimited(
            breach,
        ))) => rate_limited(breach, path, id),
        BatchError::Store(BatchStoreError::Accounting(AccountingStoreError::QuotaExceeded)) => {
            problem(
                StatusCode::TOO_MANY_REQUESTS,
                ProblemCode::QuotaExceeded,
                path,
                id,
            )
        }
        BatchError::Store(BatchStoreError::Accounting(
            AccountingStoreError::ApiKeyBudgetExceeded,
        )) => problem_with_detail(
            StatusCode::TOO_MANY_REQUESTS,
            ProblemCode::QuotaExceeded,
            crate::transport::inference::API_KEY_BUDGET_DETAIL,
            path,
            id,
        ),
        // The end user's refusals, as chat and embeddings give them (D-028).
        BatchError::Store(BatchStoreError::Accounting(AccountingStoreError::EndUserRequired)) => {
            problem(
                StatusCode::BAD_REQUEST,
                ProblemCode::EndUserRequired,
                path,
                id,
            )
        }
        BatchError::Store(BatchStoreError::Accounting(
            AccountingStoreError::EndUserNotRegistered,
        )) => problem(
            StatusCode::FORBIDDEN,
            ProblemCode::EndUserNotRegistered,
            path,
            id,
        ),
        BatchError::Store(BatchStoreError::Accounting(
            AccountingStoreError::EndUserQuotaExceeded(refusal),
        )) => crate::transport::inference::end_user_quota_exceeded(&refusal, path, id),
        BatchError::Store(BatchStoreError::Accounting(
            AccountingStoreError::EndUserRateLimited(refusal),
        )) => crate::transport::inference::end_user_rate_limited(&refusal, path, id),
        BatchError::Store(BatchStoreError::Accounting(AccountingStoreError::NoBudget)) => {
            problem_with_detail(
                StatusCode::TOO_MANY_REQUESTS,
                ProblemCode::QuotaExceeded,
                crate::transport::inference::NO_BUDGET_DETAIL,
                path,
                id,
            )
        }
        BatchError::Store(_) | BatchError::Objects(_) => {
            unavailable(ProblemCode::InferenceUnavailable)
        }
        BatchError::Facade(error) => accounting_error(error, path, id),
    }
}

pub async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    let id = request_id(&headers);
    let path = FILES_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Ok(mut multipart) = multipart else {
        return problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ProblemCode::UnsupportedMediaType,
            path,
            &id,
        );
    };
    let mut purpose = None;
    let mut upload = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                let status = error.status();
                let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                    ProblemCode::RequestTooLarge
                } else {
                    ProblemCode::InvalidRequestBody
                };
                return problem(status, code, path, &id);
            }
        };
        match field.name() {
            Some("purpose") => match field.text().await {
                Ok(text) => purpose = Some(text),
                Err(_) => {
                    return problem(
                        StatusCode::BAD_REQUEST,
                        ProblemCode::InvalidRequestBody,
                        path,
                        &id,
                    );
                }
            },
            Some("file") => {
                let filename = field.file_name().unwrap_or("input.jsonl").to_owned();
                match field.bytes().await {
                    Ok(bytes) => upload = Some((filename, bytes.to_vec())),
                    Err(error) => {
                        let status = error.status();
                        let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                            ProblemCode::RequestTooLarge
                        } else {
                            ProblemCode::InvalidRequestBody
                        };
                        return problem(status, code, path, &id);
                    }
                }
            }
            _ => {}
        }
    }
    let (Some(purpose), Some((filename, bytes))) = (purpose, upload) else {
        return problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::InvalidRequestBody,
            path,
            &id,
        );
    };
    match state
        .batches()
        .upload(&identity, &purpose, &filename, bytes, Utc::now())
        .await
    {
        Ok(file) => ok(file_object(&file), &id),
        Err(error) => batch_error(error, path, &id),
    }
}

pub async fn file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    let id = request_id(&headers);
    let path = FILE_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Some(file) = parse_id(&file, FILE_PREFIX) else {
        return problem(StatusCode::NOT_FOUND, ProblemCode::FileNotFound, path, &id);
    };
    match state.batches().file(&identity, file).await {
        Ok(file) => ok(file_object(&file), &id),
        Err(error) => batch_error(error, path, &id),
    }
}

pub async fn file_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    let id = request_id(&headers);
    let path = FILE_CONTENT_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Some(file) = parse_id(&file, FILE_PREFIX) else {
        return problem(StatusCode::NOT_FOUND, ProblemCode::FileNotFound, path, &id);
    };
    match state.batches().file_content(&identity, file).await {
        Ok((_, bytes)) => {
            let mut response = bytes.into_response();
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/jsonl"));
            if let Ok(value) = HeaderValue::from_str(&id) {
                response.headers_mut().insert(REQUEST_ID_HEADER, value);
            }
            response
        }
        Err(error) => batch_error(error, path, &id),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateBatchBody {
    pub input_file_id: String,
    pub endpoint: String,
    pub completion_window: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

pub async fn create_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateBatchBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let id = request_id(&headers);
    let path = BATCHES_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    // A batch is for one end user, named on its creation (D-028).
    let identity = match crate::transport::inference::end_user(&headers, None, path, &id) {
        Ok(end_user) => crate::domain::identity::VerifiedIdentity {
            end_user,
            ..identity
        },
        Err(response) => return response,
    };
    let key = match idempotency(&headers, &id, path) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            return crate::transport::inference::body_rejection(rejection, path, &id);
        }
    };
    let Some(input_file_id) = parse_id(&body.input_file_id, FILE_PREFIX) else {
        return problem(StatusCode::NOT_FOUND, ProblemCode::FileNotFound, path, &id);
    };
    let spec = BatchSpec {
        input_file_id,
        endpoint: body.endpoint,
        completion_window: body.completion_window,
        metadata: body.metadata.unwrap_or_default(),
    };
    match state
        .batches()
        .create(&identity, &key, spec, Utc::now())
        .await
    {
        Ok(batch) => ok(batch_object(&batch), &id),
        Err(error) => batch_error(error, path, &id),
    }
}

pub async fn batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch): Path<String>,
) -> Response {
    let id = request_id(&headers);
    let path = BATCH_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Some(batch) = parse_id(&batch, BATCH_PREFIX) else {
        return problem(StatusCode::NOT_FOUND, ProblemCode::BatchNotFound, path, &id);
    };
    let batch = match state.batches().batch(&identity, batch).await {
        Ok(batch) => batch,
        Err(error) => return batch_error(error, path, &id),
    };
    let store = state.batches().store().clone();
    let (jobs, costs) = match tokio::try_join!(
        store.job_summaries(batch.organization_id, batch.id),
        store.costs(batch.organization_id, batch.id)
    ) {
        Ok(found) => found,
        Err(error) => return batch_error(BatchError::Store(error), path, &id),
    };
    let mut body = batch_object(&batch);
    body["gateway"] = gateway_extension(&jobs, &costs);
    ok(body, &id)
}

/// The gateway's own account of a batch (D-021): how each group of lines
/// runs and at which price, and what the batch has cost so far and still
/// holds, per currency.
pub(crate) fn gateway_extension(
    jobs: &[crate::persistence::batches::JobSummary],
    costs: &[crate::persistence::batches::BatchCost],
) -> Value {
    json!({
        "jobs": jobs.iter().map(|job| json!({
            "mode": job.mode,
            "price_source": crate::domain::batch::BatchMode::parse(&job.mode)
                .map(crate::domain::batch::BatchMode::price_source),
            "provider_family": job.family,
            "state": job.state,
            "provider_status": job.provider_status,
            "request_counts": {
                "total": job.lines,
                "completed": job.succeeded,
                "failed": job.failed + job.unknown,
            },
        })).collect::<Vec<_>>(),
        "cost": costs.iter().map(|cost| json!({
            "currency": cost.currency,
            "consumed_minor": cost.consumed_minor,
            "reserved_minor": cost.reserved_minor,
        })).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub after: Option<String>,
    pub limit: Option<i64>,
}

pub async fn list_batches(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let id = request_id(&headers);
    let path = BATCHES_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Ok(Query(query)) = query else {
        return problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::InvalidRequestBody,
            path,
            &id,
        );
    };
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return problem(
            StatusCode::BAD_REQUEST,
            ProblemCode::InvalidRequestBody,
            path,
            &id,
        );
    }
    let after = match query.after.as_deref() {
        None => None,
        Some(value) => match parse_id(value, BATCH_PREFIX) {
            Some(after) => Some(after),
            None => {
                return problem(StatusCode::NOT_FOUND, ProblemCode::BatchNotFound, path, &id);
            }
        },
    };
    // One more than the page shows whether another page follows.
    match state.batches().list(&identity, after, limit + 1).await {
        Ok(mut batches) => {
            let has_more = batches.len() as i64 > limit;
            batches.truncate(usize::try_from(limit).unwrap_or(0));
            ok(
                json!({
                    "object": "list",
                    "data": batches.iter().map(batch_object).collect::<Vec<_>>(),
                    "first_id": batches.first().map(|b| batch_id(b.id)),
                    "last_id": batches.last().map(|b| batch_id(b.id)),
                    "has_more": has_more,
                }),
                &id,
            )
        }
        Err(error) => batch_error(error, path, &id),
    }
}

pub async fn cancel_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch): Path<String>,
) -> Response {
    let id = request_id(&headers);
    let path = BATCH_CANCEL_PATH;
    let identity = match authenticate(&state, &headers, &SCOPES, &id, path).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Some(batch) = parse_id(&batch, BATCH_PREFIX) else {
        return problem(StatusCode::NOT_FOUND, ProblemCode::BatchNotFound, path, &id);
    };
    match state.batches().cancel(&identity, batch, Utc::now()).await {
        Ok(batch) => ok(batch_object(&batch), &id),
        Err(error) => batch_error(error, path, &id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_carry_openai_prefixes_and_parse_back() {
        let id = Uuid::new_v4();
        assert_eq!(parse_id(&file_id(id), FILE_PREFIX), Some(id));
        assert_eq!(parse_id(&batch_id(id), BATCH_PREFIX), Some(id));
        assert_eq!(parse_id(&file_id(id), BATCH_PREFIX), None);
        assert_eq!(parse_id("file-nope", FILE_PREFIX), None);
    }

    #[test]
    fn refused_files_list_every_line() {
        let response = validation_failed(
            &[crate::domain::batch::LineError::line(
                3,
                "model_unavailable",
                "model x has no batch route",
            )],
            BATCHES_PATH,
            "request",
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
    }
}
