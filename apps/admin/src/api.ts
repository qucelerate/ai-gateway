export type Organization = { id: string; slug: string; name: string };
export type Me = {
  user_id: string;
  display_name: string | null;
  email: string | null;
  photo_url: string | null;
  /** How this session signed in: an identity provider, or a local account's password. */
  sign_in_method?: 'oidc' | 'password';
  /** Administers the deployment, above its organizations. */
  deployment_admin?: boolean;
  /** Signed in through one organization's own provider: acts only there. */
  organization_session?: boolean;
  /** Signed-in people may create organizations. */
  self_serve_organizations?: boolean;
  /** When this session ends, Unix seconds. */
  session_expires_at?: number | null;
};
export type Context = { user_id: string; organization_id: string; team_id: string | null; role: string };
type OidcStartResponse = { authorization_url: string };

export type ApiKeyScope = 'Inference' | 'Embeddings' | 'CatalogRead' | 'AdminRead';
export type MembershipRole = 'Owner' | 'Admin' | 'Member' | 'Auditor' | 'ReadOnly';
export type AdminRole =
  | 'organization_owner'
  | 'organization_admin'
  | 'team_admin'
  | 'operator'
  | 'auditor'
  | 'read_only_member';

export type Page<T> = { items: T[]; next_page_token?: string | null };
export type PageParams = { page_size?: number; page_token?: string };

export type NotificationSeverity = 'info' | 'warning' | 'action';
export type NotificationKind =
  | 'price.adopted'
  | 'price.held'
  | 'price_source.failing'
  | 'provider_account.sync_failing'
  | 'budget.threshold'
  | 'api_key.expiring'
  | 'agent_token.expiring'
  | 'webhook.failing'
  | 'end_user.credits_low'
  | 'reconciliation.open';
export type Notification = {
  id: string;
  kind: NotificationKind | string;
  severity: NotificationSeverity;
  subject: string;
  title: string;
  body: string;
  /** A console path (`/admin/...`). */
  link: string | null;
  data: Record<string, unknown>;
  created_at: string;
  updated_at: string;
  /** When the condition ended or was acted on; null for an event. */
  resolved_at: string | null;
  /** Whether the signed-in member has read it. */
  read: boolean;
};
export type NotificationFilters = { kind?: string; severity?: NotificationSeverity | ''; read?: 'all' | 'unread' | 'read' };
export type UnreadCount = { unread: number };
/** Everything up to and including one notification (by creation), or chosen ones. */
export type MarkNotificationsRead = { through: string } | { ids: string[] };

export type AuditActor = {
  actor_type: string;
  user_id: string | null;
  service_account_id: string | null;
  /** The person's, agent's or service account's name, if known. */
  name?: string | null;
};
export type AuditEvent = {
  id: string;
  occurred_at: string;
  organization_id: string;
  team_id: string | null;
  actor: AuditActor;
  action: string;
  target_type: string;
  target_id: string;
  /** A team's or service account's name, while it exists. */
  target_name?: string | null;
  outcome: string;
  request_id: string | null;
};
/** `action`, `target_type`, `outcome` and `actor_type` take comma-separated
 * lists; `q` is a substring of the action or target type. */
export type AuditEventFilters = {
  actor?: string;
  action?: string;
  target_type?: string;
  target_id?: string;
  outcome?: string;
  actor_type?: string;
  q?: string;
  occurred_after?: string;
  occurred_before?: string;
  /** `true` leaves out views (`*.read`, `*.list`). */
  exclude_views?: 'true';
};
export type AuditDimension = 'action' | 'outcome' | 'target_type' | 'actor_type' | 'actor' | 'day';
export type AuditSummary = {
  group_by: AuditDimension[];
  groups: { values: Record<string, string | null>; count: number; first_occurred_at: string; last_occurred_at: string }[];
  total: number;
  truncated: boolean;
};

export type Team = { id: string; organization_id: string; name: string; version: number };
export type Membership = {
  organization_id: string;
  team_id: string | null;
  user_id: string;
  /** As the person's identity provider last gave it at sign-in; null until then. */
  display_name: string | null;
  email: string | null;
  role: string;
  enabled: boolean;
  version: number;
};
export type RoleBinding = {
  id: string;
  organization_id: string;
  user_id: string;
  /** As the person's identity provider last gave it at sign-in; null until then. */
  display_name: string | null;
  email: string | null;
  team_id: string | null;
  role: string;
  enabled: boolean;
  version: number;
};

export type ServiceAccount = {
  id: string;
  organization_id: string;
  team_id: string | null;
  owner_user_id: string;
  name: string;
  enabled: boolean;
  created_at: string;
  version: number;
  /** Lists only: its keys that can be used now (active, not expired). */
  active_keys?: number;
  /** Lists only: when any of its keys was last used. */
  last_used_at?: string | null;
};

export type ApiKey = {
  id: string;
  organization_id: string;
  service_account_id: string;
  successor_of: string | null;
  public_prefix: string;
  scopes: ApiKeyScope[];
  status: string;
  expires_at: string | null;
  revoked_at: string | null;
  disabled_at: string | null;
  last_used_at: string | null;
  created_at: string;
  version: number;
};

export type ApiKeySecret = {
  metadata: ApiKey;
  secret: string | null;
  replay: boolean;
  error?: 'secret_unavailable_on_replay' | null;
};

export type AccessMutationResponse = {
  status: 'created' | 'updated' | 'deleted' | 'replayed';
  resource_id: string;
  version?: number;
  replayed: boolean;
  secret_unavailable_on_replay?: boolean;
};

export type DeletionResponse = { deleted: true } | void;
// Deleting a service account that has made requests disables it instead.
export type ServiceAccountDeletion =
  | DeletionResponse
  | { deleted: false; disabled: true; reason: 'usage_history'; service_account: ServiceAccount };

// Reporting (openapi/admin-reporting-v1.json). Amounts are money units:
// 10⁻⁶ of the currency's smallest unit.
export type Confidence = 'exact' | 'estimated' | 'unknown';
export type UsageGroupBy = 'day' | 'team' | 'service_account' | 'model' | 'deployment' | 'provider_family' | 'location';
export type TokenTotals = { input: number; output: number; cached_input: number; reasoning: number; other: number };
export type UsageAggregate = {
  currency: string;
  confidence: Confidence;
  request_count: number;
  amount_minor: number;
  unpriced_request_count: number;
  incomplete_request_count: number;
  tokens: TokenTotals;
};
export type UsageReportRow = UsageAggregate & { group: string | null; values: Record<string, string | null> };
export type UsageReport = {
  from: string;
  to: string;
  /** The dimensions, comma-separated: `day`, or `team,model`. */
  group_by: string;
  totals: UsageAggregate[];
  items: UsageReportRow[];
  next_page_token: string | null;
};
/** A usage report query: one or two dimensions, comma-separated, and list
 * filters (see `listFilter`) that match any of their values. */
export type UsageQuery = {
  from: string;
  to: string;
  group_by: string;
  team_id?: string;
  service_account_id?: string;
  model?: string;
  deployment?: string;
  provider_family?: string;
  location?: string;
};
export type TraceSummary = {
  id: string;
  team_id: string | null;
  service_account_id: string | null;
  status: 'pending' | 'finalized';
  outcome: string | null;
  model: string;
  deployment: string;
  provider_family: string | null;
  location: string | null;
  currency: string;
  amount_minor: number | null;
  usage_confidence: Confidence | null;
  recorded_at: string;
  finalized_at: string | null;
  /** The request id the gateway returned to the caller. */
  correlation_id: string;
  /** Null for requests admitted before keys were recorded. */
  api_key_id: string | null;
  /** The app's own id of the end user the request was for (D-028). */
  end_user: string | null;
  alias: string | null;
  operation: string | null;
  /** Admission to settlement; null while pending or when closed by expiry or an operator. */
  duration_ms: number | null;
  error_category: string | null;
  content_state: ContentState | null;
  /** Which attempt of its request, from 1: a request that failed over to
   * another deployment of its route has one trace per attempt (D-023). */
  attempt?: number;
  /** The trace of the attempt this one replaced. */
  failover_of?: string | null;
  /** The batch the request was a line of (`batch_<uuid>`), and the line's `custom_id`. */
  batch_id?: string | null;
  custom_id?: string | null;
};
/** What a trace holds of its request's content; null when tracing was not on for it. */
export type ContentState = 'writing' | 'stored' | 'off' | 'unavailable' | 'failed' | 'expired';
export type TraceRouting = {
  release_id: string;
  route_id: string;
  deployment_id: string;
  price_id: string;
  price_source: string | null;
  price_override_id: string | null;
  chat_input_count_mode: string | null;
};
export type TraceDetail = TraceSummary & {
  provider_request_id: string | null;
  input_bound: number;
  output_cap: number | null;
  price_version: string;
  pricing_confidence: 'known' | 'conservative';
  reservation: { id: string; state: string; reserved_minor: number; expires_at: string };
  usage: {
    confidence: Confidence;
    completeness: 'complete' | 'partial' | 'unavailable';
    finality: string;
    tokens: { [K in keyof TokenTotals]: number | null };
    amount_minor: number | null;
    recorded_at: string;
  } | null;
  reconciliation: { id: string; reason: string; status: string } | null;
  /** Why the provider request failed; never the provider's message. */
  provider_error: TraceProviderError | null;
  /** Null without catalog provenance. */
  routing: TraceRouting | null;
};
export type TraceProviderError = { category: string; http_status: number | null; error_type: string | null };
/** Trace filters. List filters are comma-separated (see `listFilter`) and
 * match any of their values; `q` matches part of the model or deployment. */
export type TraceQuery = {
  from?: string;
  to?: string;
  team_id?: string;
  service_account_id?: string;
  api_key_id?: string;
  end_user?: string;
  model?: string;
  deployment?: string;
  provider_family?: string;
  location?: string;
  status?: string;
  outcome?: string;
  q?: string;
};
export type TraceDimension = 'day' | 'team' | 'service_account' | 'model' | 'deployment' | 'provider_family' | 'location' | 'status' | 'outcome';
export type TraceGroup = {
  values: Record<string, string | null>;
  count: number;
  unpriced_count: number;
  /** Summed per currency, never across currencies. */
  amounts: { currency: string; amount_minor: number }[];
  first_recorded_at: string;
  last_recorded_at: string;
};
export type TraceSummaryReport = { group_by: TraceDimension[]; total: number; truncated: boolean; groups: TraceGroup[] };

// The usage dashboard: a slice of requests over time and its requests.
export type UsageDimension =
  | 'team' | 'service_account' | 'api_key' | 'user' | 'end_user' | 'model' | 'alias' | 'deployment'
  | 'provider_family' | 'location' | 'operation' | 'status' | 'outcome' | 'error_category'
  /** The master record (one model across providers, D-033) the deployment was linked to when served. */
  | 'model_master';
export type Granularity = 'hour' | 'day' | 'week';
export type RankBy = 'requests' | 'tokens' | 'cost';
/** `expired`: never reached the provider; `unknown`: closed by expiry after a claim. */
export type RequestStatus = 'succeeded' | 'failed' | 'pending' | 'expired' | 'unknown' | 'failed-over';
/** A slice's filters: a window (`to` exclusive) and comma-separated lists (see `listFilter`). */
export type UsageSliceQuery = {
  from: string;
  to: string;
  team_id?: string;
  service_account_id?: string;
  api_key_id?: string;
  user_id?: string;
  /** End-user ids, as the app sent them. */
  end_user?: string;
  model?: string;
  alias?: string;
  deployment?: string;
  provider_family?: string;
  location?: string;
  operation?: string;
  status?: string;
  outcome?: string;
  error_category?: string;
  /** Batches (`batch_<uuid>`): only their lines. */
  batch_id?: string;
  /** Master records (a card's id or `org/<slug>`): only requests served by deployments linked to them then. */
  model_master?: string;
  currency?: string;
};
export type UsageSeriesQuery = UsageSliceQuery & { granularity?: Granularity; split_by?: UsageDimension; limit?: number; rank_by?: RankBy };
export type CurrencyAmount = { currency: string; amount_minor: number };
export type UsageMeasures = {
  requests: number;
  failed_requests: number;
  pending_requests: number;
  /** Reservations that expired before the provider was called. */
  expired_requests: number;
  /** Closed by expiry after a claim: the provider's result is unknown. */
  unknown_requests: number;
  tokens: TokenTotals;
  /** One per currency; never summed across currencies. */
  amounts: CurrencyAmount[];
  unpriced_requests: number;
  no_usage_requests: number;
  incomplete_requests: number;
  duration: { average_ms: number | null; p50_ms: number | null; p95_ms: number | null };
};
export type UsagePoint = UsageMeasures & { bucket: string };
export type UsageSeries = { value: string | null; label: string | null; other: boolean; totals: UsageMeasures; points: UsagePoint[] };
export type UsageSeriesReport = {
  from: string;
  to: string;
  granularity: Granularity;
  split_by: UsageDimension | null;
  rank_by: RankBy;
  buckets: string[];
  totals: UsageMeasures;
  previous: { from: string; to: string; totals: UsageMeasures };
  series: UsageSeries[];
  truncated: boolean;
};
export type UsageEvent = {
  /** The trace id. */
  id: string;
  correlation_id: string;
  recorded_at: string;
  finalized_at: string | null;
  duration_ms: number | null;
  team_id: string | null;
  service_account_id: string | null;
  api_key_id: string | null;
  user_id: string | null;
  labels: { team: string | null; service_account: string | null; api_key: string | null; user: string | null };
  model: string;
  alias: string | null;
  deployment: string;
  provider_family: string | null;
  location: string | null;
  operation: string | null;
  status: RequestStatus;
  outcome: string | null;
  error_category: string | null;
  tokens: { [K in keyof TokenTotals]: number | null };
  currency: string;
  amount_minor: number | null;
  usage_confidence: Confidence | null;
  content_state: ContentState | null;
  /** The batch the request was a line of (`batch_<uuid>`), and the line's `custom_id`. */
  batch_id?: string | null;
  custom_id?: string | null;
  /** The master record its deployment was linked to when it was served (D-033). */
  model_master?: string | null;
};

/** Values for a comma-separated list filter: a comma or backslash within a
 * value is escaped, so any label can be listed. */
export function listFilter(values: string[] | undefined): string | undefined {
  return values?.length ? values.map(value => value.replace(/\\/g, '\\\\').replace(/,/g, '\\,')).join(',') : undefined;
}
export type ReconciliationItem = {
  id: string;
  trace_id: string;
  reason: string;
  status: 'open' | 'resolved';
  reservation_state: string;
  model: string;
  deployment: string;
  currency: string;
  reserved_minor: number;
  created_at: string;
  resolved_at: string | null;
  resolution: 'settlement' | 'operator' | null;
  resolved_amount_minor: number | null;
};
export type ReconciliationStatus = 'open' | 'resolved' | 'all';

// Catalog (openapi/admin-catalog-v1.json). Prices are money units per token:
// 10⁻⁶ of the currency's smallest unit.
export type ProviderFamily = 'openai' | 'anthropic' | 'gemini' | 'bedrock' | 'azure-openai' | 'azure-ai-foundry' | 'openai-compatible';
export type CatalogOperation = 'chat' | 'chat_stream' | 'embeddings' | 'chat_batch' | 'embeddings_batch';
export type CatalogRelease = {
  id: string;
  version: string;
  digest: string;
  created_at: string;
  sealed_at: string | null;
  active: boolean;
  activated_at: string | null;
};
export type ProviderAccount = { id: string; release_id: string; adapter_instance_key: string; family: ProviderFamily; deployment_count: number };
export type CatalogPrice = {
  id: string;
  currency: string;
  input_minor_per_token: number;
  output_minor_per_token: number;
  cached_input_minor_per_token: number;
  reasoning_minor_per_token: number;
  other_minor_per_token: number;
  source: string;
  source_version: string;
  confidence: 'known' | 'conservative';
};
export type CatalogDeployment = {
  id: string;
  provider_account_id: string;
  adapter_instance_key: string;
  family: ProviderFamily;
  name: string;
  model: string;
  location: string | null;
  operations: { operation: CatalogOperation; capability: { id: string; max_output_tokens: number; tools?: boolean } | null; price: CatalogPrice | null }[];
};
export type CatalogRoute = {
  id: string;
  alias: string;
  operation: CatalogOperation;
  deployment_id: string;
  deployment_name: string;
  model: string;
  family: ProviderFamily;
  /** Share of its priority's traffic (D-023); 1 for a route of one deployment. */
  weight: number;
  /** Lower is preferred: requests go to the lowest priority with a healthy deployment. */
  priority: number;
};
/** A page of release-scoped records; `release_id` is null when no release is active. */
export type ReleasePage<T> = Page<T> & { release_id: string | null };
export type ProviderCredential = {
  adapter_instance_key: string;
  family: ProviderFamily;
  key_id: string;
  version: number;
  created_at: string;
  rotated_at: string;
  cloud_principal?: 'approved' | 'pending';
};
/** Write-only: sent once, never returned or kept. */
export type ProviderSecretInput = Partial<Record<
  'api_key' | 'access_key_id' | 'secret_access_key' | 'session_token' | 'role_arn' | 'tenant_id' | 'client_id' | 'endpoint' | 'resource_id' | 'service_account' | 'project_id' | 'location',
  string
>>;
export type CloudTrust = { aws_external_id: string; aws_role_session_name: string };
/** A release's whole catalog, as `POST .../catalog/releases` takes it. */
export type CatalogManifest = {
  release: { version: string };
  providers: { adapter_instance_key: string; family: ProviderFamily }[];
  deployments: {
    name: string;
    model: string;
    location: string | null;
    adapter_instance_key: string;
    capabilities: ManifestCapability[];
  }[];
  prices: (Omit<CatalogPrice, 'id'> & { deployment: string; operation: CatalogOperation })[];
  routes: ManifestRoute[];
};
/** One deployment an alias routes an operation to. `weight` and `priority`
 * are 1 when absent, and left out when 1 so a release keeps its digest. */
export type ManifestRoute = { alias: string; operation: CatalogOperation; deployment: string; weight?: number; priority?: number };
export type ManifestCapability = {
  operation: CatalogOperation;
  max_input_bytes: number;
  max_output_tokens: number;
  max_tokens_per_byte: number;
  tokenizer: 'configured';
  reservation_ttl_seconds: number;
  chat_input_count_mode: 'unsupported' | 'provider_api_exact';
  /** Reasoning tokens reserved beyond a request's output cap, for a model whose
   * reasoning the cap does not bound (Grok); absent or 0: within the cap. Chat only. */
  reasoning_allowance_tokens?: number;
  /** Whether the deployment takes tool definitions (D-032); absent: false. Chat only. */
  tools?: boolean;
};
export type DriftDimension = 'input' | 'output' | 'cached_input' | 'reasoning' | 'other';
/** How each billed price compares with what its provider publishes. */
export type PriceDrift = {
  release_id: string;
  prices: {
    deployment: string;
    operation: CatalogOperation;
    model: string;
    location: string | null;
    provider_family: ProviderFamily;
    currency: string;
    /** `override`: an effective price override is billed, not the release's price. */
    price_source: 'release' | 'override';
    comparisons: {
      source: string;
      status: 'matches' | 'differs' | 'no_observation' | 'currency_mismatch' | 'not_comparable' | 'unpublished';
      observed_item: string | null;
      dimensions: {
        dimension: DriftDimension;
        /** Money units per token billed. */
        catalog: number;
        /** Published money units per token, exact; a fraction where finer than one unit. */
        observed: string | null;
        status: 'matches' | 'differs' | 'unpublished' | 'not_reported';
      }[];
    }[];
  }[];
};
/** A price a price source publishes: decimal major units per `per_tokens`. */
export type PriceObservation = {
  source: string;
  item: string;
  model: string | null;
  region: string | null;
  currency: string;
  per_tokens: 1 | 1000 | 1000000;
  input: string | null;
  output: string | null;
  cached_input: string | null;
};

// Catalog discovery (openapi/admin-catalog-v1.json): what provider accounts
// list. A listing never changes a release, and reports no capabilities or
// prices.
export type CatalogSyncJobStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'unsupported';
export type CatalogSyncJob = {
  id: string;
  adapter_instance_key: string;
  provider_family: ProviderFamily;
  trigger: 'scheduled' | 'manual';
  status: CatalogSyncJobStatus;
  attempts: number;
  /** A fixed code, never provider text. */
  error_code: string | null;
  models_listed: number | null;
  models_added: number | null;
  models_missing: number | null;
  requested_by: string | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
};
export type DiscoveredModel = {
  adapter_instance_key: string;
  provider_family: ProviderFamily;
  model: string;
  display_name: string | null;
  first_seen_at: string;
  last_seen_at: string;
  /** Null while listed. */
  missing_since: string | null;
  /** Whether a deployment of the active release names it on this account. */
  deployed: boolean;
  /** What the listing says it supports; null when unknown. */
  operations: CatalogOperation[] | null;
  /** `unknown`: the listing cannot say whether the account's location
   * offers it (Google's catalog on Agent Platform). */
  availability: 'listed' | 'unknown' | 'profile_only' | 'endpoint_only';
  /** What the gateway has seen calling it on this account (D-018); null
   * when nothing yet. */
  evidence: ModelEvidence | null;
  /** The listed deployment's details where the listing is the account's
   * own deployments (Azure); null for listings of models. */
  deployment: DiscoveredDeployment | null;
};
/** A model the account may deploy (Azure's account model catalog), from
 * `GET .../catalog/deployable-models`. */
export type DeployableModel = {
  adapter_instance_key: string;
  provider_family: ProviderFamily;
  format: string;
  model: string;
  version: string;
  lifecycle_status: string | null;
  default_version: boolean;
  skus: string[];
  /** In the SKU's unit. */
  max_capacity: number | null;
  retires_at: string | null;
  operations: CatalogOperation[] | null;
  /** Listed deployments serving it; empty when it needs a deployment. */
  deployments: string[];
  listed_at: string;
};
/** Each null when not reported. `capacity` is in the SKU's unit. */
export type DiscoveredDeployment = {
  format: string | null;
  model: string | null;
  version: string | null;
  sku: string | null;
  capacity: number | null;
};
export type ModelEvidence = {
  state: 'verified' | 'refused' | 'entitled' | 'requires_action';
  /** `access_denied` (403) or `not_found` (404) when refused;
   * `permission_missing`, `agreement_required`, `not_entitled`, `quota_zero`, `policy_denied` or `enable_in_model_garden` when
   * action is required; else null. */
  reason: 'access_denied' | 'not_found' | 'permission_missing' | 'agreement_required' | 'not_entitled' | 'quota_zero' | 'policy_denied' | 'enable_in_model_garden' | null;
  source: 'traffic' | 'probe' | 'control_plane';
  deployment: string | null;
  provider_status: number | null;
  /** The provider's error type, never its message. */
  provider_code: string | null;
  observed_at: string;
  /** The last success, kept after a refusal. */
  verified_at: string | null;
};
export type DiscoveredModelFilters = {
  adapter_instance_key?: string;
  state?: 'listed' | 'missing';
  deployed?: boolean;
  /** Latest evidence states, any of them; `none` for no evidence yet. */
  evidence?: (ModelEvidence['state'] | 'none')[];
};
export type DeployableModelFilters = {
  adapter_instance_key?: string;
  deployed?: boolean;
  /** Compared without case (Azure: `GenerallyAvailable`, `Preview`, ...). */
  lifecycle_status?: string;
  operation?: 'chat' | 'chat_stream' | 'embeddings';
};
/** Per account of the active release; drift fields are null until a listing succeeded. */
export type DiscoveryDrift = {
  release_id: string;
  accounts: {
    adapter_instance_key: string;
    provider_family: ProviderFamily;
    last_job: CatalogSyncJob | null;
    last_synced_at: string | null;
    undeployed_models: number | null;
    unlisted_deployments: { deployment: string; model: string; missing_since: string | null }[] | null;
  }[];
};

// Pricing (openapi/admin-catalog-v1.json): price sources, mappings of
// deployments to their published items, and price overrides. Sources never
// carry their URLs.
export type PriceSourceErrorCode =
  | 'invalid_url' | 'destination_not_allowed' | 'dns_failed' | 'connection_failed' | 'timed_out'
  | 'http_status' | 'response_too_large' | 'request_failed' | 'invalid_price_sheet' | 'invalid_price_list';
export type PriceSource = {
  /** `{kind}:{provider_family}`. */
  source: string;
  kind: 'price-sheet' | 'azure-retail' | 'aws-price-list';
  provider_family: ProviderFamily;
  last_attempt_at: string | null;
  last_success_at: string | null;
  last_error_code: PriceSourceErrorCode | null;
  next_fetch_at: string;
  snapshot: { id: string; source_version: string | null; fetched_at: string; confirmed_at: string; observations: number; unmapped: number } | null;
};
export type PriceObservationFilters = { q?: string; source?: string; region?: string };
export type PriceMapping = {
  id: string;
  deployment: string;
  source: string;
  item: string;
  region: string | null;
  version: number;
  created_at: string;
  updated_at: string;
};
/** What an account's latest automatic offer run decided for a model. */
export type OfferStatus = 'offered' | 'needs_price' | 'unsupported' | 'withdrawn' | 'catalog_full' | 'refused' | 'requires_action' | 'unchecked';
export type OfferedModel = {
  model: string;
  /** The deployment it is, or would be, offered as: a price mapping may
   * name it before it is offered. */
  deployment: string;
  status: OfferStatus;
  price_source: string | null;
  price_item: string | null;
  updated_at: string;
};
/** Whether an organization's prices follow their price sources. */
export type PricePolicy = { auto_adopt: boolean; version: number; updated_at: string | null };
/** The provider families and deployment locations requests may be sent to
 * (D-023); `null` for any. */
export type RoutingPolicy = { providers: ProviderFamily[] | null; locations: string[] | null; version: number; updated_at: string | null };
export type PriceHoldReason = 'large_increase' | 'large_decrease' | 'to_zero' | 'sources_disagree';
/** A published price held for an admin instead of adopted automatically. */
export type PriceHold = {
  id: string;
  deployment: string;
  operation: CatalogOperation;
  model: string;
  currency: string;
  source: string;
  reason: PriceHoldReason;
  /** Money units per token: `observed` exact, `proposed` the whole units adopting it bills. */
  changes: { dimension: DriftDimension; catalog: number; observed: string; proposed: number }[];
  status: 'held' | 'adopted' | 'dismissed' | 'superseded';
  release_id: string;
  created_at: string;
  resolved_at: string | null;
  version: number;
};
/** One rate of a model agreement offer's rate card. */
export type AgreementRate = { dimension: string; price: string | null; unit: string | null; description: string | null };
/** An offer for a Bedrock model's AWS Marketplace agreement (never its token). */
export type AgreementOffer = {
  offer_id: string;
  /** The account's region's rates where the card names them, else the card's first 100. */
  rate_card: AgreementRate[];
  rate_card_region: string | null;
  rate_card_total: number;
  legal_url: string | null;
  refund_policy: string | null;
  agreement_duration: string | null;
};
export type AgreementStatus = 'pending' | 'available' | 'verified' | 'refused' | 'failed';
export type AgreementReason =
  | 'agreement_permission_missing'
  | 'access_request_required'
  | 'invoke_permission_missing'
  | 'not_found'
  | 'agreement_error'
  | 'agreement_not_available'
  | 'still_pending'
  | 'not_verified'
  | 'not_attempted'
  | 'acceptance_interrupted'
  | 'not_agreement_required'
  | 'provider_error';
/** An agreement accepted through the gateway, and where it stands. */
export type AgreementState = {
  status: AgreementStatus;
  reason: AgreementReason | null;
  detail: string | null;
  provider_code: string | null;
  offer_id: string;
  accepted_by: string | null;
  accepted_at: string;
  checked_at: string | null;
  verified_at: string | null;
  probe: boolean;
  probed_model: string | null;
  checking: boolean;
};
export type AgreementModel = {
  model: string;
  unblocks: string[];
  deployments: string[];
  agreement_required: boolean;
  offers: AgreementOffer[];
  offers_error: 'agreement_permission_missing' | 'provider_error' | null;
  agreement: AgreementState | null;
};
export type ModelAgreements = {
  adapter_instance_key: string;
  family: string;
  permission: 'granted' | 'missing' | 'unknown';
  models: AgreementModel[];
};
/** Each model named with the offer it was shown with: there is no "accept all". */
export type AcceptAgreementsInput = { models: { model: string; offer_id: string }[]; verify: boolean };
export type AgreementResult = {
  model: string;
  offer_id: string | null;
  outcome: 'accepted' | 'already_accepted' | 'unconfirmed' | 'failed';
  status: AgreementStatus;
  reason: AgreementReason | null;
  detail: string | null;
  provider_code: string | null;
};
export type AgreementResults = { adapter_instance_key: string; results: AgreementResult[] };

/** A provider account's opt-in to automatic model offers. */
export type AutoOffer = {
  adapter_instance_key: string;
  location: string | null;
  created_at: string;
  updated_at: string;
  applied_at: string | null;
  applied_release_id: string | null;
  error_code: string | null;
  version: number;
  models: OfferedModel[];
};
export type PriceOverrideStatus = 'scheduled' | 'active' | 'expired' | 'revoked';
/** Prices are money units per token, like the catalog's. */
export type PriceOverrideInput = {
  deployment: string;
  operation: CatalogOperation;
  input_minor_per_token: number;
  output_minor_per_token: number;
  cached_input_minor_per_token?: number;
  reasoning_minor_per_token?: number;
  other_minor_per_token?: number;
  currency: string;
  confidence: 'known' | 'conservative';
  reason: string;
  effective_from?: string | null;
  effective_until?: string | null;
};
export type PriceOverride = Required<Omit<PriceOverrideInput, 'effective_from' | 'effective_until'>> & {
  id: string;
  price_version: string;
  effective_from: string;
  effective_until: string | null;
  status: PriceOverrideStatus;
  created_by: string | null;
  created_at: string;
  revoked_at: string | null;
  revoked_by: string | null;
  revoke_reason: string | null;
  version: number;
};

// Policies (openapi/admin-rate-limits-v1.json, openapi/admin-data-v1.json).
/** The levels budgets have: the organization, a team, a service account, one API key. */
export type RateLimitScope = 'organization' | 'team' | 'service-account' | 'api-key';
export type RateLimitPolicy = {
  id: string;
  scope: RateLimitScope;
  team_id: string | null;
  service_account_id: string | null;
  api_key_id: string | null;
  requests_per_minute: number | null;
  tokens_per_minute: number | null;
  max_concurrent: number | null;
  version: number;
  created_at: string;
  updated_at: string;
};
export type RateLimitInput = { requests_per_minute: number | null; tokens_per_minute: number | null; max_concurrent: number | null };
export type RetentionPeriods = { trace_days: number | null; usage_days: number | null; audit_days: number | null };
export type RetentionPolicy = RetentionPeriods & {
  version: number;
  updated_at: string | null;
  last_purged_at: string | null;
  purged: { traces: number; reservations: number; audit_events: number };
};
export type TraceMode = 'off' | 'organization' | 'selected';
export type TracePolicyInput = {
  mode: TraceMode;
  team_ids: string[];
  service_account_ids: string[];
  content: boolean;
  content_retention_days: number;
};
export type TracePolicy = TracePolicyInput & { version: number; updated_at: string | null };
export type TraceContentState = 'stored' | 'pending' | 'off' | 'unavailable' | 'failed' | 'expired' | 'not_traced';
export type TracedMessage = { role: string; content: string; name?: string; tool_call_id?: string };
export type TracedChoice = {
  index: number;
  content: string;
  refusal?: string;
  tool_calls?: { id?: string; name?: string; arguments: string }[];
  finish_reason?: string;
};
export type TraceContent = {
  trace_id: string;
  state: TraceContentState;
  started_at: string | null;
  provider_started_at: string | null;
  first_event_at: string | null;
  finished_at: string | null;
  expired_at: string | null;
  request_bytes: number | null;
  response_bytes: number | null;
  truncated: { request: boolean; response: boolean };
  request: TracedChatRequest | TracedEmbeddingRequest | null;
  response: TracedChatResponse | TracedEmbeddingResponse | null;
};
export type TracedChatRequest = { messages: TracedMessage[]; temperature?: number; max_tokens?: number };
export type TracedChatResponse = { model: string; choices: TracedChoice[] };
/** An embeddings request's input texts. */
export type TracedEmbeddingRequest = { input: string[] };
/** Each vector's dimensions and its first values (`preview`); whole vectors are not kept. */
export type TracedEmbeddingResponse = { model: string; embeddings: { index: number; dimensions: number; preview: number[] }[] };
export type ExportDataset = 'usage' | 'traces' | 'audit-events';
export type ExportFormat = 'csv' | 'ndjson';
export type DataExport = {
  id: string;
  dataset: ExportDataset;
  format: ExportFormat;
  from: string;
  to: string;
  actor_user_id: string | null;
  status: 'streaming' | 'completed' | 'failed' | 'abandoned';
  rows: number;
  started_at: string;
  finished_at: string | null;
};

// Budgets per end user of an app (openapi/admin-end-users-v1.json, D-028).
export type EndUserRequirement = 'optional' | 'required';
export type EndUserSettings = {
  service_account_id: string;
  requirement: EndUserRequirement;
  currency: string;
  default_plan: string | null;
  version: number;
  created_at: string;
  updated_at: string;
};
export type EndUserSettingsInput = { requirement: EndUserRequirement; currency: string; default_plan: string | null };
/** How a plan's allowance repeats: the UTC month, the ISO week from Monday,
 * the UTC day, or `period_days` days. */
export type EndUserPlanPeriod = 'monthly' | 'weekly' | 'daily' | 'days';
export type EndUserPlan = {
  name: string;
  /** Per period. */
  allowance_minor: number;
  period: EndUserPlanPeriod;
  period_days: number | null;
  /** Per user; null is no limit. */
  requests_per_minute: number | null;
  tokens_per_minute: number | null;
  max_concurrent: number | null;
  currency: string;
  /** The end users the plan is in force for (assigned, or unassigned while it is the default). */
  users: number;
  /** What those users' requests settled in the plan's current period were charged. */
  spent_minor: number;
  period_start: string;
  period_end: string;
  version: number;
  created_at: string;
  updated_at: string;
};
/** A plan as PUT replaces it: what is left out is monthly and unlimited. */
export type EndUserPlanInput = {
  allowance_minor: number;
  period: EndUserPlanPeriod;
  period_days: number | null;
  requests_per_minute: number | null;
  tokens_per_minute: number | null;
  max_concurrent: number | null;
};
export type EndUser = {
  id: string;
  plan: string | null;
  effective_plan: string | null;
  currency: string | null;
  period_start: string;
  period_end: string;
  allowance_minor: number;
  allowance_spent_minor: number;
  allowance_reserved_minor: number;
  allowance_remaining_minor: number;
  credit_balance_minor: number;
  /** What its requests were charged this period of their plan (the UTC month
   * without one), from allowance or credits,
   * in `spent_currency`; null when, without end-user settings, they used
   * several currencies. */
  spent_minor: number | null;
  /** The app's currency, or without settings the one its requests used. */
  spent_currency: string | null;
  /** 0 for a user only requests have seen. */
  version: number;
  created_at: string;
  updated_at: string;
};
export type CreditGrant = {
  id: string;
  end_user: string;
  reference: string;
  amount_minor: number;
  currency: string;
  reversed_minor: number;
  spent_minor: number;
  reserved_minor: number;
  remaining_minor: number;
  expires_at: string | null;
  expired: boolean;
  granted_at: string;
};
export type CreditGrantInput = { reference: string; amount_minor: number; expires_at: string | null };
export type CreditReversal = {
  id: string;
  grant_id: string;
  reference: string;
  requested_minor: number;
  reversed_minor: number;
  reason: string;
  created_at: string;
};
export type CreditReversalInput = { reference: string; amount_minor: number; reason: string };

function endUserBase(organizationId: string, serviceAccountId: string): string {
  return `/organizations/${organizationId}/service-accounts/${encodeURIComponent(serviceAccountId)}`;
}

// Budgets (openapi/admin-budgets-v1.json): amounts in money units.
export type BudgetPeriod = 'monthly' | 'fixed';
export type BudgetInput = { currency: string; limit_minor: number; period: BudgetPeriod; starts_at?: string; ends_at?: string };
/** What a budget covers: the same levels as a rate limit. */
export type BudgetScope = RateLimitScope;
export type BudgetPolicy = {
  id: string;
  scope: BudgetScope;
  team_id: string | null;
  service_account_id: string | null;
  /** Set for an `api-key` budget; the key may have been revoked since. */
  api_key_id: string | null;
  currency: string;
  limit_minor: number;
  period: BudgetPeriod;
  starts_at: string | null;
  ends_at: string | null;
  current: { period_start: string; period_end: string; consumed_minor: number; reserved_minor: number } | null;
  version: number;
  created_at: string;
  updated_at: string;
};

// Usage export (openapi/admin-usage-export-v1.json): settled usage written
// to the organization's own storage through one of its cloud connections.
export type StorageKind = 's3' | 'gcs' | 'azure_blob';
export type UsageExportInput = {
  kind: StorageKind;
  /** The adapter instance key of the stored credential that writes. */
  connection: string;
  /** The bucket, or the Azure container. */
  bucket: string;
  /** The Azure storage account; `azure_blob` only. */
  account: string | null;
  /** The bucket's region; `s3` only. */
  region: string | null;
  prefix: string;
};
export type UsageExport = UsageExportInput & {
  id: string;
  enabled_at: string;
  verified_at: string;
  last_success_at: string | null;
  /** Writes have failed for an hour without a success since. */
  failing: boolean;
  failing_since: string | null;
  last_error: string | null;
  queued_records: number;
  oldest_queued_at: string | null;
  version: number;
  created_at: string;
  updated_at: string;
};

// Webhooks (openapi/admin-webhooks-v1.json). Signing secrets are returned
// only by create and rotate, once; never listed.
export const webhookEventTypes = ['inference.finalized', 'reconciliation.opened', 'reconciliation.updated', 'reconciliation.resolved', 'usage_export.failing', 'notification.created'] as const;
export type WebhookEventType = (typeof webhookEventTypes)[number];
export type WebhookEndpoint = {
  id: string;
  url: string;
  description: string | null;
  event_types: WebhookEventType[];
  enabled: boolean;
  created_at: string;
  updated_at: string;
  version: number;
};
export type WebhookEndpointInput = { url: string; description?: string | null; event_types: WebhookEventType[]; enabled: boolean };
/** Only the fields present change; an empty `description` clears it. */
export type WebhookEndpointUpdate = Partial<{ url: string; description: string; event_types: WebhookEventType[]; enabled: boolean }>;
/** `secret` is null on an idempotent replay, with `error` saying why. */
export type WebhookSecretResponse = {
  endpoint: WebhookEndpoint;
  secret: string | null;
  replay: boolean;
  error: 'secret_unavailable_on_replay' | null;
};
export type WebhookDelivery = {
  id: string;
  event_id: string;
  event_type: WebhookEventType;
  status: 'pending' | 'delivered' | 'parked';
  attempts: number;
  next_attempt_at: string | null;
  last_attempt_at: string | null;
  last_status_code: number | null;
  last_error: string | null;
  delivered_at: string | null;
  created_at: string;
};
// The organization's own sign-in provider (openapi/admin-identity-provider-v1.json).
// The client secret is write-only: it is sent on save and never returned.
export type IdentityProviderStatus =
  | 'not_configured'
  | 'pending'
  | 'active'
  | 'active_with_pending_change'
  | 'disabled';
export type IdentityProviderConfiguration = {
  configuration_version: number;
  issuer: string;
  client_id: string;
  client_secret_set: boolean;
  extra_scopes: string[];
  created_at: string;
  created_by: string | null;
  tested_at: string | null;
};
export type IdentityProvider = {
  status: IdentityProviderStatus;
  enabled: boolean;
  active: IdentityProviderConfiguration | null;
  pending: IdentityProviderConfiguration | null;
  last_tested_at: string | null;
  last_tested_by: string | null;
  redirect_uri: string | null;
  /** The ETag's version; 0 before the first save. */
  version: number;
};
export type IdentityProviderInput = {
  issuer: string;
  client_id: string;
  /** Left out to keep the stored secret (same issuer and client only). */
  client_secret?: string;
  extra_scopes: string[];
};
// Invitations (openapi/admin-invitations-v1.json): a single-use link, shown once.
export type InvitationRole = 'owner' | 'admin' | 'member' | 'auditor' | 'read-only';
export type InvitationStatus = 'pending' | 'accepted' | 'revoked' | 'expired';
export type Invitation = {
  id: string;
  role: InvitationRole;
  status: InvitationStatus;
  created_at: string;
  expires_at: string;
  created_by: string | null;
  accepted_at: string | null;
  accepted_by: string | null;
  revoked_at: string | null;
};
/** `grantable_roles` is what the caller may invite in: empty without `invitation_write`. */
export type InvitationPage = Page<Invitation> & { grantable_roles: InvitationRole[] };
/** An agent's organization role: the invitation roles. */
export type AgentRole = InvitationRole;
export type AgentTokenStatus = 'active' | 'expired' | 'revoked';
export type AgentToken = {
  id: string;
  name: string;
  /** The token's first 12 characters (`gwa_…`), to recognise it by. */
  public_prefix: string;
  status: AgentTokenStatus;
  created_at: string;
  created_by: string | null;
  expires_at: string | null;
  revoked_at: string | null;
  last_used_at: string | null;
  /** The one service account it is limited to (its end users only); null
   * for the agent's whole organization. */
  service_account_id: string | null;
};
/** `token` is shown once; null on a replay. */
export type CreatedAgentToken = AgentToken & { token: string | null; replay: boolean };
export type Agent = {
  id: string;
  name: string;
  description: string | null;
  role: AgentRole;
  enabled: boolean;
  created_at: string;
  created_by: string | null;
  disabled_at: string | null;
  tokens: AgentToken[];
};
/** Not paged. `grantable_roles` is what the caller may give an agent: empty without `agent_write`. */
export type AgentList = { items: Agent[]; grantable_roles: AgentRole[] };
export type AgentInput = { name: string; role: AgentRole; description: string | null };

export type InvitationLinkResponse = {
  invitation: Invitation;
  /** Only in the response that created it; null on a replay. */
  link: string | null;
  replay: boolean;
  error: 'secret_unavailable_on_replay' | null;
};
// Configuration versions (openapi/admin-config-versions-v1.json): what
// routing every API-serving instance serves, and the pin that holds them on
// an earlier version.
export type ConfigVersion = {
  id: string;
  sequence: number;
  /** Hex SHA-256 of the version's document. */
  digest: string;
  release_id: string | null;
  routes: number;
  overrides: number;
  served: boolean;
  pinned: boolean;
  newest: boolean;
  created_at: string;
};
export type ConfigPin = {
  pinned_version_id: string | null;
  served_version_id: string | null;
  newest_version_id: string | null;
  /** The ETag's version; 0 before the first pin. */
  version: number;
};
export type ConfigInstance = {
  instance_id: string;
  profile: 'all-in-one' | 'api' | 'data-plane';
  region: string | null;
  build: string;
  applied_sequence: number | null;
  serving_version_id: string | null;
  loaded_at: string | null;
  started_at: string;
  last_seen_at: string;
  /** Unseen for five minutes. */
  stale: boolean;
};

/** The policy path of a scope (`rate-limits` or `budgets`): the organization, or one team, service account or (budgets only) API key. */
function scopePath(organizationId: string, collection: 'rate-limits' | 'budgets', scope: BudgetScope, targetId: string | null): string {
  const base = `/organizations/${organizationId}/${collection}`;
  if (scope === 'organization') return `${base}/organization`;
  const segment = { team: 'teams', 'service-account': 'service-accounts', 'api-key': 'api-keys' }[scope];
  return `${base}/${segment}/${targetId}`;
}

/** The team, service account or API key a budget names; null for the organization's. */
export function budgetTarget(budget: Pick<BudgetPolicy, 'team_id' | 'service_account_id' | 'api_key_id'>): string | null {
  return budget.team_id ?? budget.service_account_id ?? budget.api_key_id ?? null;
}

// Batches (openapi/admin-batches-v1.json, D-021).
export type BatchStatus = 'validating' | 'in_progress' | 'finalizing' | 'completed' | 'failed' | 'expired' | 'cancelling' | 'cancelled';
export type BatchCounts = { total: number; completed: number; failed: number };
export type AdminBatch = {
  id: string;
  object: 'batch';
  endpoint: string;
  status: BatchStatus | string;
  input_file_id: string;
  output_file_id: string | null;
  error_file_id: string | null;
  created_at: number;
  expires_at: number;
  completed_at?: number | null;
  cancelled_at?: number | null;
  request_counts: BatchCounts;
  metadata: Record<string, string>;
  service_account_id: string;
  team_id: string | null;
};
export type BatchJob = {
  /** `native` (the provider's batch API) or `emulated` (line by line). */
  mode: 'native' | 'emulated' | string;
  /** `batch` or `on_demand`. */
  price_source: 'batch' | 'on_demand' | string | null;
  provider_family: string;
  deployment: string;
  model: string;
  state: string;
  provider_status: string | null;
  provider_batch_id: string | null;
  error_code: string | null;
  request_counts: BatchCounts & { pending: number; unknown: number };
  submitted_at: string | null;
  finished_at: string | null;
};
export type BatchCost = { currency: string; consumed_minor: number; reserved_minor: number };
export type AdminBatchDetail = AdminBatch & { gateway: { jobs: BatchJob[]; cost: BatchCost[] } };
export type BatchSettings = { emulated_allowed: boolean; emulated_concurrency: number; version: number; updated_at: string | null };
export type BatchSettingsInput = { emulated_allowed: boolean; emulated_concurrency?: number };
export type BatchStaging = {
  connection: string;
  /** `s3` (Bedrock) or `gcs` (Gemini on Agent Platform). */
  kind: 's3' | 'gcs' | string;
  bucket: string;
  prefix: string;
  region: string | null;
  role_arn: string | null;
  version: number;
  created_at: string;
  updated_at: string;
};
export type BatchStagingInput = { bucket: string; prefix?: string; region?: string | null; role_arn?: string | null };

/** Where the browser downloads a batch's output or errors (JSONL). */
export function batchResultsUrl(organizationId: string, batchId: string, which: 'output' | 'errors'): string {
  return `${base}/organizations/${organizationId}/batches/${encodeURIComponent(batchId)}/${which}`;
}

/** Where the browser downloads an export; the request itself is the export. */
export function exportUrl(organizationId: string, dataset: ExportDataset, format: ExportFormat, from: string, to: string): string {
  return `${base}${withQuery(`/organizations/${organizationId}/exports/${dataset}`, { format, from, to })}`;
}

// Cloud onboarding (openapi/admin-onboarding-v1.json): nothing here is
// secret, and verification echoes no secret.
export type OnboardingSummary = {
  aws: { principal_arn: string; external_id: string; role_name: string } | null;
  azure: { client_id: string } | null;
  google: { service_account: string } | null;
  /** Always offered; `private_endpoints` are the origins the operator allows
   * over plain HTTP or at private addresses. */
  openai_compatible?: { private_endpoints: string[] } | null;
};
export type AwsInstructions = {
  role_arn: string;
  external_id: string;
  launch_url: string | null;
  template_url: string | null;
  /** Whether the role may also accept models' AWS Marketplace agreements. */
  accept_model_agreements: boolean;
  /** The staging bucket and service role the role may run batch jobs with. */
  batch_bucket?: string | null;
  batch_service_role_arn?: string | null;
  /** The IAM actions the role is granted. */
  actions: string[];
  cli: string;
  terraform: string;
};
export type AzureInstructions = { consent_url: string; client_id: string; role: string; batch?: boolean; cli: string; terraform: string };
export type GoogleInstructions = { service_account: string; project_id: string; batch_bucket?: string | null; gcloud: string; cloud_shell_url: string; terraform: string };
/** What the optional batch sections are given: AWS needs the staging bucket
 * and the batch service role, Google the bucket. */
export type AwsBatchSetup = { bucket: string; serviceRoleArn: string };
export type Verification = {
  status: 'connected' | 'awaiting_approval' | 'not_configured' | 'failed';
  reason?: 'authentication' | 'authorization' | 'unavailable' | 'timeout' | 'unsupported' | 'provider';
  message: string;
  models?: { model: string; display_name: string | null }[];
  model_count?: number;
  region?: string;
};

export type ApiProblem = {
  type: string;
  code: string;
  title: string;
  status: number;
  detail: string;
  instance: string;
  request_id: string;
};

const base = '/api/admin/v1';

export class ApiError extends Error {
  constructor(public status: number, public code: string) {
    super(code);
  }
}

export class ApiProblemError extends Error {
  constructor(
    public status: number,
    public problem: ApiProblem,
  ) {
    super(problem.code);
  }
}

export type NormalizedError = { status: number; code: string };

export function normalizeApiError(caught: unknown): NormalizedError {
  if (caught instanceof ApiProblemError) {
    return { status: caught.status, code: caught.problem.code };
  }
  return caught instanceof ApiError
    ? { status: caught.status, code: caught.code }
    : { status: 0, code: 'network_error' };
}

export function isContextDenied(error: NormalizedError) {
  return error.status === 403 && error.code === 'organization_or_team_context_denied';
}

function csrfToken() {
  return document.cookie
    .split('; ')
    .find(c => c.startsWith('__Host-gateway_csrf='))
    ?.split('=')
    .slice(1)
    .join('=');
}

type AdminRequestInit = RequestInit & { idempotencyKey?: string; ifMatch?: string };

export type AdminResponseMeta = { etag?: string; requestId?: string };

let sessionEndedListener: (() => void) | null = null;

/** Calls `listener` when an admin request finds the session gone (401
 * `authentication_required`), so the console can go to the sign-in page.
 * Returns the unsubscribe. */
export function onSessionEnded(listener: () => void): () => void {
  sessionEndedListener = listener;
  return () => {
    if (sessionEndedListener === listener) sessionEndedListener = null;
  };
}

function noticeEndedSession(status: number, code: string) {
  if (status === 401 && code === 'authentication_required') sessionEndedListener?.();
}

export async function adminRequest<T>(
  path: string,
  init: AdminRequestInit = {},
): Promise<{ data: T; meta: AdminResponseMeta }> {
  const headers = new Headers(init.headers);
  headers.set('Accept', 'application/json');
  if (init.body) headers.set('Content-Type', 'application/json');
  const token = csrfToken();
  if (token) headers.set('X-CSRF-Token', token);
  if (init.idempotencyKey) headers.set('Idempotency-Key', init.idempotencyKey);
  if (init.ifMatch) headers.set('If-Match', init.ifMatch);

  const response = await fetch(`${base}${path}`, { ...init, headers, credentials: 'same-origin' });

  if (!response.ok) {
    let problem: ApiProblem | null = null;
    try {
      const body = await response.clone().json();
      if (body && typeof body.code === 'string') problem = body as ApiProblem;
    } catch {
      /* non-JSON or missing code */
    }
    if (problem) {
      noticeEndedSession(response.status, problem.code);
      throw new ApiProblemError(response.status, problem);
    }
    let code = `http_${response.status}`;
    try {
      code = (await response.json()).error ?? code;
    } catch {
      /* ignore */
    }
    noticeEndedSession(response.status, code);
    throw new ApiError(response.status, code);
  }

  const meta: AdminResponseMeta = {};
  const etag = response.headers.get('etag');
  if (etag) meta.etag = etag;
  const requestId = response.headers.get('x-request-id');
  if (requestId) meta.requestId = requestId;

  const data = response.status === 204 ? (undefined as T) : await response.json();
  return { data, meta };
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  headers.set('Accept', 'application/json');
  if (init.body) headers.set('Content-Type', 'application/json');
  const token = csrfToken();
  if (token) headers.set('X-CSRF-Token', token);
  const response = await fetch(`${base}${path}`, { ...init, headers, credentials: 'same-origin' });
  if (!response.ok) {
    let code = `http_${response.status}`;
    try {
      code = (await response.json()).error ?? code;
    } catch {
      /* ignore */
    }
    noticeEndedSession(response.status, code);
    throw new ApiError(response.status, code);
  }
  return response.status === 204 ? (undefined as T) : response.json();
}

export function newIdempotencyKey(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

function etag(version: number): string {
  return `"v${version}"`;
}

async function fetchCollection<T>(path: string, params?: PageParams, filters: Record<string, string> = {}): Promise<Page<T>> {
  const query = new URLSearchParams(filters);
  if (params?.page_size) query.set('page_size', String(params.page_size));
  if (params?.page_token) query.set('page_token', params.page_token);
  const suffix = query.toString() ? `?${query.toString()}` : '';
  const { data } = await adminRequest<Page<T>>(`${path}${suffix}`);
  return data;
}

/** Every item of a paged list, following `next_page_token` to the end. */
export async function allPages<T, P extends { items: T[]; next_page_token?: string | null }>(fetch: (token?: string) => Promise<P>): Promise<{ items: T[]; first: P }> {
  const first = await fetch();
  const items = [...first.items];
  let token = first.next_page_token ?? undefined;
  while (token) {
    const page = await fetch(token);
    items.push(...page.items);
    token = page.next_page_token ?? undefined;
  }
  return { items, first };
}

function withQuery(path: string, values: Record<string, string | number | undefined>): string {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined && value !== '') query.set(key, String(value));
  }
  return query.toString() ? `${path}?${query.toString()}` : path;
}

async function fetchAuditEvents(
  organizationId: string,
  params?: PageParams & AuditEventFilters,
): Promise<Page<AuditEvent>> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value !== undefined && value !== '') query.set(key, String(value));
  }
  const suffix = query.toString() ? `?${query.toString()}` : '';
  const { data } = await adminRequest<Page<AuditEvent>>(
    `/organizations/${organizationId}/audit-events${suffix}`,
  );
  return data;
}

// Model cards (openapi/admin-model-cards-v1.json): one catalog model's card
// from the active release, availability evidence, usage and the audit log.
export type CardPrice = {
  currency: string;
  input_minor_per_token: number;
  output_minor_per_token: number;
  cached_input_minor_per_token: number;
  reasoning_minor_per_token: number;
  other_minor_per_token: number;
  billed_by: 'release' | 'override';
  source: string | null;
  source_version: string | null;
  confidence: 'known' | 'conservative';
};
/** One operation of a deployment: its limits, whether it takes tools, and
 * the price billed now (null when the release prices no such operation, so
 * no route uses it). */
export type CardOperation = { operation: string; max_input_bytes: number; max_output_tokens: number; tools: boolean; price: CardPrice | null };
export type CardAvailability = { state: string; reason: string | null; source: string; observed_at: string; verified_at: string | null };
export type CardDeployment = {
  deployment: string;
  adapter_instance_key: string;
  provider_family: string;
  location: string | null;
  routes: { alias: string; operation: string }[];
  operations: CardOperation[];
  availability: CardAvailability | null;
  /** What the provider's listing of the account's deployments says (Azure); null elsewhere. */
  provider_listing: CardListing | null;
};
export type CardListing = {
  format: string | null;
  model: string | null;
  version: string | null;
  sku: string | null;
  lifecycle_status: string | null;
  retires_at: string | null;
  listed_at: string;
  /** The content filter policy the deployment applies (Azure's `raiPolicyName`). */
  content_filter: string | null;
  /** Its settings, when the account's policy list could be read. */
  content_filter_policy: ContentFilterPolicy | null;
};
export type ContentFilterPolicy = {
  name: string;
  base_policy?: string;
  mode?: string;
  system_policy?: boolean;
  filters: { name: string; source?: string; enabled?: boolean; blocking?: boolean; severity_threshold?: string }[];
};
export type CardNote = {
  note: string;
  version: number;
  updated_at: string;
  updated_by: { actor_type: 'user' | 'service_account' | 'system'; id: string | null; name: string | null } | null;
};
export type ModelCardSummary = {
  model: string;
  provider_families: string[];
  card: { id: string; name: string; vendor: string; origin: CardFileOrigin } | null;
  context_window_tokens: CardFact | null;
  max_output_tokens: CardFact | null;
  /** The provider's published retirement commitment, from the card file. */
  retirement: CardFact | null;
  /** The earliest retirement date a provider listing gives a deployment of the model. */
  retires_at: string | null;
};
export type RequestStats = {
  days: number;
  from: string;
  to: string;
  request_count: number;
  failed_count: number;
  errors: { key: string; count: number }[];
  timed_since: string | null;
  timed_request_count: number;
  latency_p50_ms: number | null;
  latency_p95_ms: number | null;
  first_event_p50_ms: number | null;
  first_event_p95_ms: number | null;
  finish_reasons: { reason: string; count: number; safety: boolean }[];
};
export type GovernanceEntry = {
  release_id: string;
  release_version: string;
  created_at: string;
  actor: { actor_type: 'user' | 'service_account' | 'system'; id: string | null; name: string | null } | null;
  added: string[];
  removed: string[];
  changed: string[];
};
/** One public fact as its source states it. */
export type CardFact = {
  value: string | number | boolean | string[];
  source_url: string;
  retrieved_at: string;
  /** `provider`: self-reported. `independent`: an evaluator or hub. */
  reported_by: 'provider' | 'independent';
  title?: string;
};
export type CardEvaluation = {
  name: string;
  score: string | number;
  setting?: string;
  evaluator: string;
  source_url: string;
  retrieved_at: string;
  reported_by: 'provider' | 'independent';
};
/** `imported`: from a Hugging Face model card, by this organization. */
export type CardFileOrigin = 'builtin' | 'operator' | 'imported';
// Organization evaluations (openapi/admin-model-cards-v1.json).
export type EvaluationMatch = 'exact' | 'contains';
export type EvaluationItem = { id: string; expected: string; match: EvaluationMatch };
export type EvaluationSet = {
  id: string;
  name: string;
  description?: string;
  items: EvaluationItem[];
  created_at: string;
  created_by: CardNote['updated_by'];
  /** The `If-Match` a delete names. */
  version: number;
};
export type EvaluationSetInput = { name: string; description?: string; items: EvaluationItem[] };
export type EvaluationScore = { model: string; items: number; answered: number; correct: number };
export type EvaluationRun = {
  id: string;
  set_id: string;
  batch_id: string;
  created_at: string;
  created_by: CardNote['updated_by'];
  /** Output lines whose custom_id named no item of the set. */
  unmatched_lines: number;
  results: EvaluationScore[];
};
export type CardImport = { model: string; repository: string; provider_repository: boolean; version: number; imported_at: string; imported_by: CardNote['updated_by'] };
export type PublicDataHandling = {
  provider_family: string;
  name: string;
  origin: CardFileOrigin;
  applies_to?: string;
  /** The family's service these terms cover (`gemini-api`, `agent-platform`, `anthropic`). */
  service?: string;
  /** The model's deployments these terms cover. */
  deployments: string[];
  facts: Record<string, CardFact>;
};
export type PublicCard = {
  status: 'on_file' | 'not_on_file';
  card?: {
    id: string;
    name: string;
    vendor: string;
    origin: CardFileOrigin;
    matched_deployments: string[];
    overview: Record<string, CardFact>;
    evaluations: CardEvaluation[];
    safety: Record<string, CardFact>;
  };
  data_handling: PublicDataHandling[];
};
export type ModelCard = {
  schema: 'qucelerate.model-card.v1';
  model: string;
  generated_at: string;
  release_id: string;
  overview: {
    display_name: string | null;
    provider_families: string[];
    operations: string[];
    locations: string[];
    max_input_bytes: number | null;
    max_output_tokens: number | null;
  };
  public_card: PublicCard;
  /** Absent without `tracing_read`. */
  gateway_data_handling?: { trace_mode: 'off' | 'organization' | 'selected'; trace_content: boolean; trace_content_retention_days: number };
  deployments: CardDeployment[];
  /** Absent without `usage_read`. */
  usage?: { windows: { days: number; from: string; to: string; totals: UsageAggregate[] }[]; requests: RequestStats };
  /** Absent without `audit_read`. */
  governance?: { first_release: GovernanceEntry | null; history: GovernanceEntry[]; truncated: boolean };
  /** The organization's own evaluation sets that scored the model: latest run each. */
  organization_evaluations: { set_id: string; set_name: string; run_id: string; batch_id: string; scored_at: string; items: number; answered: number; correct: number }[];
  /** The organization's own note on the model; null when none is set. */
  organization_note: CardNote | null;
  omitted: { section: 'usage' | 'governance' | 'gateway_data_handling'; requires: 'usage_read' | 'audit_read' | 'tracing_read' }[];
};


// Master model records (openapi/admin-model-cards-v1.json, D-033): one
// record per model, whoever serves it. Links are catalog metadata: they
// never route; a route across providers reaches routing through a release.
export type Modality = 'chat' | 'embeddings';
/** How a deployment's master was decided: the organization's link (which
 * may be to none), else the card listing its family and model, else none. */
export type ResolvedBy = 'link' | 'card' | 'none';
/** A link in use, or cleared (`cleared`: the card decides; `version` is the one to set it again at). */
export type MasterLinkRef = { master: string | null; version: number; cleared: boolean };
export type MasterDeployment = {
  deployment: string;
  model: string;
  provider_family: string;
  /** The provider account. */
  adapter_instance_key: string;
  location: string | null;
  master: string | null;
  resolved_by: ResolvedBy;
  /** The card listing its family and model, whether or not a link overrides it. */
  card: string | null;
  link: MasterLinkRef | null;
  operations: CardOperation[];
  availability: CardAvailability | null;
  routes: { alias: string; operation: string }[];
};
export type MasterRecord = {
  /** A card's id (`anthropic/claude-sonnet-4-6`) or `org/<slug>`. */
  id: string;
  name: string;
  kind: 'card' | 'organization';
  vendor: string | null;
  origin: CardFileOrigin | null;
  modality: Modality;
  /** An organization record's version; null for a card. */
  version: number | null;
  deployments: MasterDeployment[];
  /** Linked deployments the record's own aliases (routing two or more of
   * its deployments, or its default alias) leave out; computed, never
   * applied on its own. Absent from older gateways. */
  suggestions?: RouteSuggestion[];
};
/** The alias routes other deployments of the record for the operation, but not this one. */
export type RouteSuggestion = { alias: string; operation: ProposalOperation; deployment: string };
export type MasterLink = {
  provider_family: string;
  model: string;
  master: string | null;
  version: number;
  updated_at: string;
  updated_by: CardNote['updated_by'];
  /** Cleared: kept, and set again at `version`, not `"v0"`. */
  cleared: boolean;
};
/** `links` includes cleared ones; `deleted_records` the organization's deleted records, each created again at its `version`. */
export type MasterList = { release_id: string | null; items: MasterRecord[]; unlinked: MasterDeployment[]; links: MasterLink[]; deleted_records: OrganizationMaster[] };
export type OrganizationMaster = {
  id: string;
  name: string;
  modality: Modality;
  version: number;
  created_at: string;
  updated_at: string;
  updated_by: CardNote['updated_by'];
  /** Deleted (only in the list's `deleted_records`). */
  deleted: boolean;
};
export type ProposalOperation = 'chat' | 'embeddings' | 'chat_batch' | 'embeddings_batch';
export type ProposedRoute = { operation: string; deployment: string; weight: number; priority: number };
export type RouteExclusionReason =
  | 'no_price'
  | 'provider_not_permitted'
  | 'location_not_permitted'
  | 'availability_refused'
  | 'availability_requires_action'
  | 'no_tools'
  | 'mixed_tool_call_ids'
  | 'currency'
  | 'too_many_deployments';
export type RouteExclusion = { operation: string; deployment: string; reason: RouteExclusionReason; detail: string };
export type RouteWeight = { weight: number; priority: number };
export type RouteChange = { operation: string; deployment: string; change: 'added' | 'removed' | 'changed' | 'unchanged'; before: RouteWeight | null; after: RouteWeight | null };
export type RouteProposal = {
  master: string;
  release_id: string;
  alias: string;
  /** Whether the active release routes the alias already, for any operation. */
  alias_exists: boolean;
  operations: string[];
  tools: boolean;
  routes: ProposedRoute[];
  excluded: RouteExclusion[];
  /** The alias's routes of these operations in the active release. */
  existing: ProposedRoute[];
  changes: RouteChange[];
  /** Why publishing the proposal as computed would be refused; null when it would publish. */
  problem: string | null;
};

export const api = {
  /** A page of the organization's batches, newest first. */
  listBatches: async (organizationId: string, status: BatchStatus | '', params?: PageParams) =>
    (await adminRequest<Page<AdminBatch>>(withQuery(`/organizations/${organizationId}/batches`, { status, ...params }))).data,
  getBatch: async (organizationId: string, batchId: string) =>
    (await adminRequest<AdminBatchDetail>(`/organizations/${organizationId}/batches/${encodeURIComponent(batchId)}`)).data,
  /** Retrying must reuse `idempotencyKey`, so a lost response replays. */
  cancelBatch: async (organizationId: string, batchId: string, idempotencyKey: string) =>
    (await adminRequest<AdminBatchDetail>(`/organizations/${organizationId}/batches/${encodeURIComponent(batchId)}/cancel`, {
      method: 'POST',
      idempotencyKey,
    })).data,
  batchSettings: async (organizationId: string) =>
    (await adminRequest<BatchSettings>(`/organizations/${organizationId}/batch-settings`)).data,
  setBatchSettings: (organizationId: string, input: BatchSettingsInput, version: number, idempotencyKey: string) =>
    adminRequest<BatchSettings>(`/organizations/${organizationId}/batch-settings`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: version > 0 ? etag(version) : '*',
    }),
  listBatchStaging: async (organizationId: string) =>
    (await adminRequest<{ data: BatchStaging[] }>(`/organizations/${organizationId}/batch-staging`)).data.data,
  /** `*` for a connection without staging; an existing one needs its version. */
  setBatchStaging: (organizationId: string, connection: string, input: BatchStagingInput, version: number | null, idempotencyKey: string) =>
    adminRequest<BatchStaging>(`/organizations/${organizationId}/batch-staging/${encodeURIComponent(connection)}`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: version === null ? '*' : etag(version),
    }),
  deleteBatchStaging: (organizationId: string, staging: BatchStaging, idempotencyKey: string) =>
    adminRequest<void>(`/organizations/${organizationId}/batch-staging/${encodeURIComponent(staging.connection)}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(staging.version),
    }),
  /** The member's notifications, newest first. */
  listNotifications: async (organizationId: string, params?: PageParams & NotificationFilters) => {
    const { data } = await adminRequest<Page<Notification>>(withQuery(`/organizations/${organizationId}/notifications`, { ...params }));
    return data;
  },
  /** Unread notifications that are not resolved. */
  unreadNotifications: async (organizationId: string) => {
    const { data } = await adminRequest<UnreadCount>(`/organizations/${organizationId}/notifications/unread-count`);
    return data;
  },
  /** Marks notifications read for the signed-in member; the unread count after. */
  markNotificationsRead: async (organizationId: string, target: MarkNotificationsRead) => {
    const { data } = await adminRequest<UnreadCount>(`/organizations/${organizationId}/notifications/read`, { method: 'POST', body: JSON.stringify(target) });
    return data;
  },
  me: () => request<Me>('/me'),
  organizations: () => request<Organization[]>('/organizations'),
  context: (organizationId: string, teamId?: string) =>
    request<Context>('/context', {
      method: 'POST',
      body: JSON.stringify({ organization_id: organizationId, team_id: teamId ?? null }),
    }),
  logout: () => request<void>('/logout', { method: 'POST' }),

  listAuditEvents: (organizationId: string, params?: PageParams & AuditEventFilters) =>
    fetchAuditEvents(organizationId, params),
  /** Matching events counted by one or two dimensions, largest groups first. */
  summarizeAuditEvents: async (organizationId: string, groupBy: AuditDimension[], filters: AuditEventFilters) => {
    const query = new URLSearchParams({ group_by: groupBy.join(',') });
    for (const [key, value] of Object.entries(filters)) if (value !== undefined && value !== '') query.set(key, String(value));
    const { data } = await adminRequest<AuditSummary>(`/organizations/${organizationId}/audit-events/summary?${query}`);
    return data;
  },

  listReleases: async (organizationId: string, params?: PageParams) =>
    (await adminRequest<Page<CatalogRelease> & { activation_version: number }>(withQuery(`/organizations/${organizationId}/catalog/releases`, { ...params }))).data,
  /** `activationVersion` is the one listed with the releases the person saw
   * (`"v0"` before any activation); if another activation happened since,
   * this fails with version_conflict. */
  activateRelease: (organizationId: string, releaseId: string, activationVersion: number, idempotencyKey: string) =>
    adminRequest<unknown>(`/organizations/${organizationId}/catalog/releases/${releaseId}/activate`, {
      method: 'POST',
      idempotencyKey,
      ifMatch: etag(activationVersion),
    }),
  /** A release's catalog, the base for a release that changes it. Lists
   * are sorted by their keys. */
  releaseManifest: async (organizationId: string, releaseId: string) =>
    (await adminRequest<CatalogManifest>(`/organizations/${organizationId}/catalog/releases/${releaseId}/manifest`)).data,
  /** Stores a sealed, inactive release; activate it to use it. */
  createRelease: async (organizationId: string, manifest: CatalogManifest, idempotencyKey: string) =>
    (await adminRequest<CatalogRelease>(`/organizations/${organizationId}/catalog/releases`, {
      method: 'POST',
      body: JSON.stringify(manifest),
      idempotencyKey,
    })).data,
  /** Each billed price of the active release against what its provider publishes. */
  priceDrift: async (organizationId: string) =>
    (await adminRequest<PriceDrift>(`/organizations/${organizationId}/catalog/price-drift`)).data,
  /** Prices the gateway's price sources publish, items or models containing `q`. */
  priceObservations: async (organizationId: string, q: string, params?: PageParams) =>
    (await adminRequest<Page<PriceObservation>>(withQuery(`/organizations/${organizationId}/catalog/price-observations`, { q, ...params }))).data,
  /** Accounts of `releaseId`, else of the active release. */
  listProviderAccounts: async (organizationId: string, params?: PageParams, releaseId?: string) =>
    (await adminRequest<ReleasePage<ProviderAccount>>(withQuery(`/organizations/${organizationId}/catalog/provider-accounts`, { release_id: releaseId, ...params }))).data,
  /** The card of the catalog model `model` (its identifier, as deployments name it). */
  modelCard: async (organizationId: string, model: string) =>
    (await adminRequest<ModelCard>(`/organizations/${organizationId}/catalog/models/${encodeURIComponent(model)}/card`)).data,
  /** Card facts of every model the active release serves (the Models table's columns). */
  modelCards: async (organizationId: string) =>
    (await adminRequest<{ release_id: string | null; items: ModelCardSummary[] }>(`/organizations/${organizationId}/catalog/model-cards`)).data,
  /** Every master record with its linked deployments in the active release, and the unlinked ones. */
  modelMasters: async (organizationId: string) =>
    (await adminRequest<MasterList>(`/organizations/${organizationId}/catalog/model-masters`)).data,
  /** Creates (`version` null: `"v0"`; a deleted record at its version) or changes the organization record `id` (`org/<slug>`). */
  putOrganizationMaster: async (organizationId: string, id: string, input: { name: string; modality?: Modality }, version: number | null, idempotencyKey: string) =>
    (await adminRequest<OrganizationMaster>(`/organizations/${organizationId}/catalog/model-masters/${encodeURIComponent(id)}`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: etag(version ?? 0),
    })).data,
  /** Deletes an organization record softly; refused (`master_in_use`) while links name it. */
  deleteOrganizationMaster: async (organizationId: string, id: string, version: number, idempotencyKey: string) => {
    await adminRequest<void>(`/organizations/${organizationId}/catalog/model-masters/${encodeURIComponent(id)}`, { method: 'DELETE', idempotencyKey, ifMatch: etag(version) });
  },
  /** Links `family`'s `model` to `master` (null: to none). `ifMatch` from `linkIfMatch`. */
  setMasterLink: async (organizationId: string, family: string, model: string, master: string | null, ifMatch: string, idempotencyKey: string) =>
    (await adminRequest<MasterLink>(`/organizations/${organizationId}/catalog/model-master-links/${encodeURIComponent(family)}/${encodeURIComponent(model)}`, {
      method: 'PUT',
      body: JSON.stringify({ master }),
      idempotencyKey,
      ifMatch,
    })).data,
  /** Clears the link, so the card that matches the model decides again. */
  clearMasterLink: async (organizationId: string, family: string, model: string, version: number, idempotencyKey: string) => {
    await adminRequest<void>(`/organizations/${organizationId}/catalog/model-master-links/${encodeURIComponent(family)}/${encodeURIComponent(model)}`, { method: 'DELETE', idempotencyKey, ifMatch: etag(version) });
  },
  /** A route across providers for `master`, computed against the active release. Read-only. */
  routeProposal: async (organizationId: string, master: string, operations: ProposalOperation[], tools: boolean, alias?: string) =>
    (await adminRequest<RouteProposal>(withQuery(`/organizations/${organizationId}/catalog/model-masters/${encodeURIComponent(master)}/route-proposal`, {
      operations: operations.join(','),
      tools: tools ? 'true' : undefined,
      alias,
    }))).data,
  /** Sets the organization's note on `model`; an empty note clears it. `current` null: none ever set,
   * so the write only succeeds while none is (`"v0"`), and two first notes cannot overwrite each other. */
  setModelNote: async (organizationId: string, model: string, note: string, current: CardNote | null, idempotencyKey: string) =>
    (await adminRequest<CardNote>(`/organizations/${organizationId}/catalog/models/${encodeURIComponent(model)}/card/note`, {
      method: 'PUT',
      body: JSON.stringify({ note }),
      idempotencyKey,
      ifMatch: current ? etag(current.version) : '"v0"',
    })).data,
  /** The `If-Match` for changing `model`'s imported card: its version, or `*` when none is imported. */
  modelCardImportMatch: async (organizationId: string, model: string) => {
    const path = `/organizations/${organizationId}/catalog/models/${encodeURIComponent(model)}/card/import`;
    return adminRequest<CardImport>(path).then(({ data }) => etag(data.version), (error: { status?: number }) => {
      if (error.status === 404) return '*';
      throw error;
    });
  },
  /** Imports `repository`'s Hugging Face model card as `model`'s card. */
  /** `ifMatch` from `modelCardImportMatch`, read once per attempt: a retry
   * sends the same key and the same `If-Match`, so it replays. */
  importModelCard: async (organizationId: string, model: string, repository: string, providerRepository: boolean, idempotencyKey: string, ifMatch: string) =>
    (await adminRequest<CardImport>(`/organizations/${organizationId}/catalog/models/${encodeURIComponent(model)}/card/import`, {
      method: 'PUT',
      body: JSON.stringify({ repository, provider_repository: providerRepository }),
      idempotencyKey,
      ifMatch,
    })).data,
  /** Removes `model`'s imported card (kept in history); `ifMatch` as for `importModelCard`. */
  removeModelCardImport: async (organizationId: string, model: string, idempotencyKey: string, ifMatch: string) => {
    await adminRequest<void>(`/organizations/${organizationId}/catalog/models/${encodeURIComponent(model)}/card/import`, { method: 'DELETE', idempotencyKey, ifMatch });
  },
  /** The organization's evaluation sets, newest first (at most 200). */
  listEvaluationSets: async (organizationId: string) =>
    (await adminRequest<{ items: EvaluationSet[] }>(`/organizations/${organizationId}/catalog/evaluation-sets`)).data.items,
  /** Retrying must reuse `idempotencyKey`, so a lost response replays. */
  createEvaluationSet: async (organizationId: string, input: EvaluationSetInput, idempotencyKey: string) =>
    (await adminRequest<EvaluationSet>(`/organizations/${organizationId}/catalog/evaluation-sets`, {
      method: 'POST',
      body: JSON.stringify(input),
      idempotencyKey,
    })).data,
  /** Deletes the set softly: hidden, with its runs and scores kept. */
  deleteEvaluationSet: async (organizationId: string, set: EvaluationSet, idempotencyKey: string) => {
    await adminRequest<void>(`/organizations/${organizationId}/catalog/evaluation-sets/${encodeURIComponent(set.id)}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(set.version),
    });
  },
  /** The set's runs, newest first, with each model's score. */
  listEvaluationRuns: async (organizationId: string, setId: string) =>
    (await adminRequest<{ items: EvaluationRun[] }>(`/organizations/${organizationId}/catalog/evaluation-sets/${encodeURIComponent(setId)}/runs`)).data.items,
  /** Scores the set from a completed chat batch of the organization. */
  createEvaluationRun: async (organizationId: string, setId: string, batchId: string, idempotencyKey: string) =>
    (await adminRequest<EvaluationRun>(`/organizations/${organizationId}/catalog/evaluation-sets/${encodeURIComponent(setId)}/runs`, {
      method: 'POST',
      body: JSON.stringify({ batch_id: batchId }),
      idempotencyKey,
    })).data,
  listDeployments: async (organizationId: string, params?: PageParams) =>
    (await adminRequest<ReleasePage<CatalogDeployment>>(withQuery(`/organizations/${organizationId}/catalog/deployments`, { ...params }))).data,
  /** Routes of `releaseId`, so they match deployments read from that release. */
  listRoutes: async (organizationId: string, releaseId: string, params?: PageParams) =>
    (await adminRequest<ReleasePage<CatalogRoute>>(withQuery(`/organizations/${organizationId}/catalog/routes`, { release_id: releaseId, ...params }))).data,
  listCredentials: (organizationId: string, params?: PageParams) =>
    fetchCollection<ProviderCredential>(`/organizations/${organizationId}/catalog/credentials`, params),
  /** `version` is the stored credential's, guarding a rotation; null for a new one. */
  setCredential: (organizationId: string, key: string, secret: ProviderSecretInput, version: number | null, idempotencyKey: string) =>
    adminRequest<ProviderCredential>(`/organizations/${organizationId}/catalog/credentials/${encodeURIComponent(key)}`, {
      method: 'PUT',
      body: JSON.stringify(secret),
      idempotencyKey,
      // `*` to create: a removed policy or credential keeps its version
      // history, so "v0" (none yet) would refuse adding it back.
      ifMatch: version === null ? '*' : etag(version),
    }),
  deleteCredential: (organizationId: string, key: string, version: number, idempotencyKey: string) =>
    adminRequest<void>(`/organizations/${organizationId}/catalog/credentials/${encodeURIComponent(key)}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(version),
    }),
  onboarding: async (organizationId: string) =>
    (await adminRequest<OnboardingSummary>(`/organizations/${organizationId}/onboarding`)).data,
  awsOnboarding: async (organizationId: string, accountId: string, region: string, acceptAgreements = false, batch?: AwsBatchSetup) =>
    (await adminRequest<AwsInstructions>(withQuery(`/organizations/${organizationId}/onboarding/aws`, {
      account_id: accountId,
      region,
      accept_agreements: acceptAgreements ? 'true' : undefined,
      batch_bucket: batch?.bucket,
      batch_role_arn: batch?.serviceRoleArn,
    }))).data,
  azureOnboarding: async (organizationId: string, family: ProviderFamily, resourceId?: string, batch = false) =>
    (await adminRequest<AzureInstructions>(withQuery(`/organizations/${organizationId}/onboarding/azure`, { resource_id: resourceId, family, batch: batch ? 'true' : undefined }))).data,
  googleOnboarding: async (organizationId: string, projectId: string, batchBucket?: string) =>
    (await adminRequest<GoogleInstructions>(withQuery(`/organizations/${organizationId}/onboarding/google`, { project_id: projectId, batch_bucket: batchBucket }))).data,
  /** Signs in as inference would and checks access; stores nothing. */
  verifyCredential: async (organizationId: string, adapterInstanceKey: string, family: ProviderFamily, credential: ProviderSecretInput) =>
    (await adminRequest<Verification>(`/organizations/${organizationId}/onboarding/verify`, {
      method: 'POST',
      body: JSON.stringify({ adapter_instance_key: adapterInstanceKey, family, credential }),
    })).data,
  cloudTrust: async (organizationId: string) =>
    (await adminRequest<CloudTrust>(`/organizations/${organizationId}/catalog/cloud-trust`)).data,

  // Catalog discovery: sync jobs list provider accounts' models.
  listSyncJobs: async (organizationId: string, adapterInstanceKey?: string, params?: PageParams) =>
    (await adminRequest<Page<CatalogSyncJob>>(withQuery(`/organizations/${organizationId}/catalog/sync-jobs`, { adapter_instance_key: adapterInstanceKey, ...params }))).data,
  /** Queues a listing of one account of the active release. */
  startSyncJob: (organizationId: string, adapterInstanceKey: string, idempotencyKey: string) =>
    adminRequest<CatalogSyncJob>(`/organizations/${organizationId}/catalog/sync-jobs`, {
      method: 'POST',
      body: JSON.stringify({ adapter_instance_key: adapterInstanceKey }),
      idempotencyKey,
    }),
  getSyncJob: async (organizationId: string, jobId: string) =>
    (await adminRequest<CatalogSyncJob>(`/organizations/${organizationId}/catalog/sync-jobs/${jobId}`)).data,
  listDiscoveredModels: async (organizationId: string, filters: DiscoveredModelFilters, params?: PageParams) =>
    (await adminRequest<Page<DiscoveredModel>>(withQuery(`/organizations/${organizationId}/catalog/discovered-models`, {
      adapter_instance_key: filters.adapter_instance_key,
      state: filters.state,
      deployed: filters.deployed === undefined ? undefined : String(filters.deployed),
      evidence: filters.evidence?.length ? filters.evidence.join(',') : undefined,
      ...params,
    }))).data,
  listDeployableModels: async (organizationId: string, filters: DeployableModelFilters, params?: PageParams) =>
    (await adminRequest<Page<DeployableModel>>(withQuery(`/organizations/${organizationId}/catalog/deployable-models`, {
      adapter_instance_key: filters.adapter_instance_key,
      deployed: filters.deployed === undefined ? undefined : String(filters.deployed),
      lifecycle_status: filters.lifecycle_status,
      operation: filters.operation,
      ...params,
    }))).data,
  discoveryDrift: async (organizationId: string) =>
    (await adminRequest<DiscoveryDrift>(`/organizations/${organizationId}/catalog/discovery-drift`)).data,

  // Pricing: sources, mappings and overrides.
  listPriceSources: async (organizationId: string) =>
    (await adminRequest<{ items: PriceSource[] }>(`/organizations/${organizationId}/catalog/price-sources`)).data.items,
  /** Makes a gateway-wide source due now; the worker fetches it within a minute. */
  fetchPriceSource: async (organizationId: string, source: string, idempotencyKey: string) =>
    (await adminRequest<PriceSource>(`/organizations/${organizationId}/catalog/price-sources/${encodeURIComponent(source)}/fetch`, {
      method: 'POST',
      idempotencyKey,
    })).data,
  pricePolicy: async (organizationId: string) =>
    (await adminRequest<PricePolicy>(`/organizations/${organizationId}/catalog/price-policy`)).data,
  setPricePolicy: async (organizationId: string, autoAdopt: boolean, current: PricePolicy, idempotencyKey: string) =>
    (await adminRequest<PricePolicy>(`/organizations/${organizationId}/catalog/price-policy`, {
      method: 'PUT',
      body: JSON.stringify({ auto_adopt: autoAdopt }),
      idempotencyKey,
      ifMatch: current.version === 0 ? '*' : etag(current.version),
    })).data,
  routingPolicy: async (organizationId: string) =>
    (await adminRequest<RoutingPolicy>(`/organizations/${organizationId}/catalog/routing-policy`)).data,
  /** Replaces the policy; `null` for a list permits any. */
  setRoutingPolicy: async (
    organizationId: string,
    next: { providers: ProviderFamily[] | null; locations: string[] | null },
    current: RoutingPolicy,
    idempotencyKey: string,
  ) =>
    (await adminRequest<RoutingPolicy>(`/organizations/${organizationId}/catalog/routing-policy`, {
      method: 'PUT',
      body: JSON.stringify(next),
      idempotencyKey,
      ifMatch: current.version === 0 ? '*' : etag(current.version),
    })).data,
  /** Open holds; `all` includes resolved ones. */
  priceHolds: async (organizationId: string, all = false) =>
    (await adminRequest<{ items: PriceHold[] }>(`/organizations/${organizationId}/catalog/price-holds${all ? '?all=true' : ''}`)).data.items,
  /** Leaves the price as it is; the same published price is not held again. */
  dismissPriceHold: async (organizationId: string, hold: PriceHold, idempotencyKey: string) =>
    (await adminRequest<PriceHold>(`/organizations/${organizationId}/catalog/price-holds/${hold.id}/dismiss`, {
      method: 'POST',
      idempotencyKey,
      ifMatch: etag(hold.version),
    })).data,
  searchPriceObservations: async (organizationId: string, filters: PriceObservationFilters, params?: PageParams) =>
    (await adminRequest<Page<PriceObservation>>(withQuery(`/organizations/${organizationId}/catalog/price-observations`, { ...filters, ...params }))).data,
  listPriceMappings: (organizationId: string, params?: PageParams) =>
    fetchCollection<PriceMapping>(`/organizations/${organizationId}/catalog/price-mappings`, params),
  /** `current` is the mapping being replaced, guarding it; null maps anew with `*`. */
  setPriceMapping: (organizationId: string, deployment: string, source: string, input: { item: string; region?: string | null }, current: PriceMapping | null, idempotencyKey: string) =>
    adminRequest<PriceMapping>(`/organizations/${organizationId}/catalog/price-mappings/${encodeURIComponent(deployment)}/${encodeURIComponent(source)}`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: current ? etag(current.version) : '*',
    }),
  deletePriceMapping: (organizationId: string, mapping: PriceMapping, idempotencyKey: string) =>
    adminRequest<DeletionResponse>(`/organizations/${organizationId}/catalog/price-mappings/${encodeURIComponent(mapping.deployment)}/${encodeURIComponent(mapping.source)}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(mapping.version),
    }),
  /** The account's opt-in, or null when it is not opted in. */
  getAutoOffer: async (organizationId: string, adapterInstanceKey: string): Promise<AutoOffer | null> => {
    try {
      return (await adminRequest<AutoOffer>(`/organizations/${organizationId}/catalog/auto-offers/${encodeURIComponent(adapterInstanceKey)}`)).data;
    } catch (caught) {
      if ((caught as { problem?: ApiProblem }).problem?.status === 404) return null;
      throw caught;
    }
  },
  /** A Bedrock account's models that need an AWS Marketplace agreement, with their live offers. */
  modelAgreements: async (organizationId: string, adapterInstanceKey: string) =>
    (await adminRequest<ModelAgreements>(`/organizations/${organizationId}/catalog/agreements/${encodeURIComponent(adapterInstanceKey)}`)).data,
  /** Creates AWS Marketplace subscriptions, billed per use. Retrying must reuse `idempotencyKey`. */
  acceptModelAgreements: async (organizationId: string, adapterInstanceKey: string, input: AcceptAgreementsInput, idempotencyKey: string) =>
    (await adminRequest<AgreementResults>(`/organizations/${organizationId}/catalog/agreements/${encodeURIComponent(adapterInstanceKey)}/accept`, {
      method: 'POST',
      body: JSON.stringify(input),
      idempotencyKey,
    })).data,
  setAutoOffer: (organizationId: string, adapterInstanceKey: string, input: { location: string | null }, current: AutoOffer | null, idempotencyKey: string) =>
    adminRequest<AutoOffer>(`/organizations/${organizationId}/catalog/auto-offers/${encodeURIComponent(adapterInstanceKey)}`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: current ? etag(current.version) : '*',
    }),
  deleteAutoOffer: (organizationId: string, offer: AutoOffer, idempotencyKey: string) =>
    adminRequest<DeletionResponse>(`/organizations/${organizationId}/catalog/auto-offers/${encodeURIComponent(offer.adapter_instance_key)}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(offer.version),
    }),
  listPriceOverrides: (organizationId: string, params?: PageParams) =>
    fetchCollection<PriceOverride>(`/organizations/${organizationId}/catalog/price-overrides`, params),
  createPriceOverride: (organizationId: string, input: PriceOverrideInput, idempotencyKey: string) =>
    adminRequest<PriceOverride>(`/organizations/${organizationId}/catalog/price-overrides`, {
      method: 'POST',
      body: JSON.stringify(input),
      idempotencyKey,
    }),
  revokePriceOverride: (organizationId: string, override: PriceOverride, reason: string, idempotencyKey: string) =>
    adminRequest<PriceOverride>(`/organizations/${organizationId}/catalog/price-overrides/${override.id}/revoke`, {
      method: 'POST',
      body: JSON.stringify({ reason }),
      idempotencyKey,
      ifMatch: etag(override.version),
    }),

  listRateLimits: (organizationId: string, params?: PageParams) =>
    fetchCollection<RateLimitPolicy>(`/organizations/${organizationId}/rate-limits`, params),
  /** `version` guards replacing an existing policy; null creates one. */
  setRateLimit: (organizationId: string, scope: RateLimitScope, targetId: string | null, limits: RateLimitInput, version: number | null, idempotencyKey: string) =>
    adminRequest<RateLimitPolicy>(scopePath(organizationId, 'rate-limits', scope, targetId), {
      method: 'PUT',
      body: JSON.stringify(limits),
      idempotencyKey,
      // `*` to create: a removed policy or credential keeps its version
      // history, so "v0" (none yet) would refuse adding it back.
      ifMatch: version === null ? '*' : etag(version),
    }),
  deleteRateLimit: (organizationId: string, policy: RateLimitPolicy, idempotencyKey: string) =>
    adminRequest<void>(scopePath(organizationId, 'rate-limits', policy.scope, budgetTarget(policy)), {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(policy.version),
    }),
  getEndUserSettings: async (organizationId: string, serviceAccountId: string) =>
    (await adminRequest<EndUserSettings>(`${endUserBase(organizationId, serviceAccountId)}/end-user-settings`)).data,
  /** `version` guards replacing the settings; null creates them. */
  setEndUserSettings: (organizationId: string, serviceAccountId: string, input: EndUserSettingsInput, version: number | null, idempotencyKey: string) =>
    adminRequest<EndUserSettings>(`${endUserBase(organizationId, serviceAccountId)}/end-user-settings`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: version === null ? '*' : etag(version),
    }),
  listEndUserPlans: async (organizationId: string, serviceAccountId: string) =>
    (await adminRequest<{ data: EndUserPlan[] }>(`${endUserBase(organizationId, serviceAccountId)}/end-user-plans`)).data.data,
  /** `*` to create: a removed plan keeps its version history. */
  setEndUserPlan: (organizationId: string, serviceAccountId: string, name: string, input: EndUserPlanInput, version: number | null, idempotencyKey: string) =>
    adminRequest<EndUserPlan>(`${endUserBase(organizationId, serviceAccountId)}/end-user-plans/${encodeURIComponent(name)}`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: version === null ? '*' : etag(version),
    }),
  deleteEndUserPlan: (organizationId: string, serviceAccountId: string, plan: EndUserPlan, idempotencyKey: string) =>
    adminRequest<void>(`${endUserBase(organizationId, serviceAccountId)}/end-user-plans/${encodeURIComponent(plan.name)}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(plan.version),
    }),
  /** End users by ID; the API names the page `data`. */
  listEndUsers: async (organizationId: string, serviceAccountId: string, params?: PageParams): Promise<Page<EndUser>> => {
    const query = new URLSearchParams();
    if (params?.page_size) query.set('page_size', String(params.page_size));
    if (params?.page_token) query.set('page_token', params.page_token);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    const { data } = await adminRequest<{ data: EndUser[]; next_page_token?: string | null }>(
      `${endUserBase(organizationId, serviceAccountId)}/end-users${suffix}`,
    );
    return { items: data.data, next_page_token: data.next_page_token };
  },
  /** `plan` null puts the user on the default plan. */
  assignEndUserPlan: (organizationId: string, serviceAccountId: string, user: EndUser, plan: string | null, idempotencyKey: string) =>
    adminRequest<EndUser>(`${endUserBase(organizationId, serviceAccountId)}/end-users/${encodeURIComponent(user.id)}`, {
      method: 'PUT',
      body: JSON.stringify({ plan }),
      idempotencyKey,
      ifMatch: etag(user.version),
    }),
  /** Names a user requests have not created yet, on a plan (null: the default), so they can be given a plan or credits before their first request. */
  addEndUser: (organizationId: string, serviceAccountId: string, endUser: string, plan: string | null, idempotencyKey: string) =>
    adminRequest<EndUser>(`${endUserBase(organizationId, serviceAccountId)}/end-users/${encodeURIComponent(endUser)}`, {
      method: 'PUT',
      body: JSON.stringify({ plan }),
      idempotencyKey,
      ifMatch: '*',
    }),
  listCreditGrants: async (organizationId: string, serviceAccountId: string, endUser: string) =>
    (await adminRequest<{ data: CreditGrant[] }>(`${endUserBase(organizationId, serviceAccountId)}/end-users/${encodeURIComponent(endUser)}/credit-grants`)).data.data,
  /** Once per reference: the same reference again returns the grant already made. */
  grantCredits: (organizationId: string, serviceAccountId: string, endUser: string, input: CreditGrantInput) =>
    adminRequest<CreditGrant>(`${endUserBase(organizationId, serviceAccountId)}/end-users/${encodeURIComponent(endUser)}/credit-grants`, {
      method: 'POST',
      body: JSON.stringify(input),
    }),
  reverseCredits: (organizationId: string, serviceAccountId: string, endUser: string, grantId: string, input: CreditReversalInput) =>
    adminRequest<CreditReversal>(
      `${endUserBase(organizationId, serviceAccountId)}/end-users/${encodeURIComponent(endUser)}/credit-grants/${grantId}/reversals`,
      { method: 'POST', body: JSON.stringify(input) },
    ),
  listBudgets: (organizationId: string, params?: PageParams) =>
    fetchCollection<BudgetPolicy>(`/organizations/${organizationId}/budgets`, params),
  /** `version` guards replacing an existing budget; null creates one. */
  setBudget: (organizationId: string, scope: BudgetScope, targetId: string | null, budget: BudgetInput, version: number | null, idempotencyKey: string) =>
    adminRequest<BudgetPolicy>(scopePath(organizationId, 'budgets', scope, targetId), {
      method: 'PUT',
      body: JSON.stringify(budget),
      idempotencyKey,
      // `*` to create: a removed budget keeps its version history.
      ifMatch: version === null ? '*' : etag(version),
    }),
  deleteBudget: (organizationId: string, budget: BudgetPolicy, idempotencyKey: string) =>
    adminRequest<void>(scopePath(organizationId, 'budgets', budget.scope, budgetTarget(budget)), {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(budget.version),
    }),
  listAgents: async (organizationId: string) =>
    (await adminRequest<AgentList>(`/organizations/${organizationId}/agents`)).data,
  createAgent: (organizationId: string, input: AgentInput, idempotencyKey: string) =>
    adminRequest<Agent>(`/organizations/${organizationId}/agents`, {
      method: 'POST',
      body: JSON.stringify(input),
      idempotencyKey,
    }),
  /** Disables the agent and revokes its tokens; it cannot be enabled again. */
  disableAgent: (organizationId: string, agentId: string, idempotencyKey: string) =>
    adminRequest<Agent>(`/organizations/${organizationId}/agents/${agentId}`, {
      method: 'DELETE',
      idempotencyKey,
    }),
  /** `expiresInDays` null: the token never expires; `serviceAccountId`
   * limits it to that service account's end users. */
  createAgentToken: (organizationId: string, agentId: string, name: string, expiresInDays: number | null, idempotencyKey: string, serviceAccountId: string | null = null) =>
    adminRequest<CreatedAgentToken>(`/organizations/${organizationId}/agents/${agentId}/tokens`, {
      method: 'POST',
      body: JSON.stringify({ name, expires_in_days: expiresInDays, ...(serviceAccountId ? { service_account_id: serviceAccountId } : {}) }),
      idempotencyKey,
    }),
  revokeAgentToken: (organizationId: string, agentId: string, tokenId: string, idempotencyKey: string) =>
    adminRequest<AgentToken>(`/organizations/${organizationId}/agents/${agentId}/tokens/${tokenId}`, {
      method: 'DELETE',
      idempotencyKey,
    }),
  listInvitations: async (organizationId: string, params?: PageParams) =>
    (await adminRequest<InvitationPage>(withQuery(`/organizations/${organizationId}/invitations`, { ...params }))).data,
  createInvitation: (organizationId: string, role: InvitationRole, validHours: number, idempotencyKey: string) =>
    adminRequest<InvitationLinkResponse>(`/organizations/${organizationId}/invitations`, {
      method: 'POST',
      body: JSON.stringify({ role, valid_hours: validHours }),
      idempotencyKey,
    }),
  identityProvider: async (organizationId: string) =>
    (await adminRequest<IdentityProvider>(`/organizations/${organizationId}/identity-provider`)).data,
  /** `*` creates; an existing provider needs its exact version. */
  saveIdentityProvider: (organizationId: string, input: IdentityProviderInput, version: number, idempotencyKey: string) =>
    adminRequest<IdentityProvider>(`/organizations/${organizationId}/identity-provider`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: version > 0 ? etag(version) : '*',
    }),
  disableIdentityProvider: (organizationId: string, version: number, idempotencyKey: string) =>
    adminRequest<IdentityProvider>(`/organizations/${organizationId}/identity-provider`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(version),
    }),
  /** Starts the owner's test sign-in; the browser goes to the returned URL. */
  testIdentityProvider: async (organizationId: string) =>
    (await adminRequest<OidcStartResponse>(`/organizations/${organizationId}/identity-provider/test`, {
      method: 'POST',
    })).data,
  revokeInvitation: (organizationId: string, invitationId: string, idempotencyKey: string) =>
    adminRequest<Invitation>(`/organizations/${organizationId}/invitations/${invitationId}/revoke`, {
      method: 'POST',
      idempotencyKey,
    }),
  listConfigVersions: (organizationId: string, params?: PageParams) =>
    fetchCollection<ConfigVersion>(`/organizations/${organizationId}/config-versions`, params),
  configPin: async (organizationId: string) =>
    (await adminRequest<ConfigPin>(`/organizations/${organizationId}/config-versions/pin`)).data,
  /** `version` is the pin's, as last read: a pin changed since fails with
   * version_conflict. */
  pinConfigVersion: (organizationId: string, versionId: string, version: number, idempotencyKey: string) =>
    adminRequest<ConfigPin>(`/organizations/${organizationId}/config-versions/pin`, {
      method: 'PUT',
      body: JSON.stringify({ version_id: versionId }),
      idempotencyKey,
      ifMatch: etag(version),
    }),
  unpinConfigVersion: (organizationId: string, version: number, idempotencyKey: string) =>
    adminRequest<ConfigPin>(`/organizations/${organizationId}/config-versions/pin`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(version),
    }),
  listConfigInstances: async (organizationId: string) =>
    (await adminRequest<Page<ConfigInstance>>(`/organizations/${organizationId}/config-instances`)).data,
  retentionPolicy: async (organizationId: string) =>
    (await adminRequest<RetentionPolicy>(`/organizations/${organizationId}/retention-policy`)).data,
  setRetentionPolicy: (organizationId: string, periods: RetentionPeriods, version: number, idempotencyKey: string) =>
    adminRequest<RetentionPolicy>(`/organizations/${organizationId}/retention-policy`, {
      method: 'PUT',
      body: JSON.stringify(periods),
      idempotencyKey,
      ifMatch: etag(version),
    }),
  tracePolicy: async (organizationId: string) =>
    (await adminRequest<TracePolicy>(`/organizations/${organizationId}/trace-policy`)).data,
  /** `version` 0 is a policy never set. */
  setTracePolicy: (organizationId: string, policy: TracePolicyInput, version: number, idempotencyKey: string) =>
    adminRequest<TracePolicy>(`/organizations/${organizationId}/trace-policy`, {
      method: 'PUT',
      body: JSON.stringify(policy),
      idempotencyKey,
      ifMatch: version === 0 ? '*' : etag(version),
    }),
  /** A trace's content; every read is audited. */
  getTraceContent: async (organizationId: string, traceId: string) =>
    (await adminRequest<TraceContent>(`/organizations/${organizationId}/traces/${encodeURIComponent(traceId)}/content`)).data,
  /** The organization's usage export, or null when none is set. */
  getUsageExport: async (organizationId: string): Promise<UsageExport | null> => {
    try {
      return (await adminRequest<UsageExport>(`/organizations/${organizationId}/usage-export`)).data;
    } catch (caught) {
      if ((caught as { status?: number }).status === 404) return null;
      throw caught;
    }
  },
  setUsageExport: (organizationId: string, input: UsageExportInput, version: number | null, idempotencyKey: string) =>
    adminRequest<UsageExport>(`/organizations/${organizationId}/usage-export`, {
      method: 'PUT',
      body: JSON.stringify(input),
      idempotencyKey,
      ifMatch: version === null ? '*' : etag(version),
    }),
  deleteUsageExport: (organizationId: string, current: UsageExport, idempotencyKey: string) =>
    adminRequest<void>(`/organizations/${organizationId}/usage-export`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(current.version),
    }),
  listExports: (organizationId: string, params?: PageParams) =>
    fetchCollection<DataExport>(`/organizations/${organizationId}/exports`, params),

  // Webhooks. Create and rotate return the signing secret once; a replay of
  // either returns `secret: null`.
  listWebhookEndpoints: (organizationId: string, params?: PageParams) =>
    fetchCollection<WebhookEndpoint>(`/organizations/${organizationId}/webhook-endpoints`, params),
  createWebhookEndpoint: (organizationId: string, input: WebhookEndpointInput, idempotencyKey: string) =>
    adminRequest<WebhookSecretResponse>(`/organizations/${organizationId}/webhook-endpoints`, {
      method: 'POST',
      body: JSON.stringify(input),
      idempotencyKey,
    }),
  getWebhookEndpoint: async (organizationId: string, endpointId: string) =>
    (await adminRequest<WebhookEndpoint>(`/organizations/${organizationId}/webhook-endpoints/${endpointId}`)).data,
  updateWebhookEndpoint: (organizationId: string, endpoint: WebhookEndpoint, update: WebhookEndpointUpdate, idempotencyKey: string) =>
    adminRequest<WebhookEndpoint>(`/organizations/${organizationId}/webhook-endpoints/${endpoint.id}`, {
      method: 'PATCH',
      body: JSON.stringify(update),
      idempotencyKey,
      ifMatch: etag(endpoint.version),
    }),
  deleteWebhookEndpoint: (organizationId: string, endpoint: WebhookEndpoint, idempotencyKey: string) =>
    adminRequest<DeletionResponse>(`/organizations/${organizationId}/webhook-endpoints/${endpoint.id}`, {
      method: 'DELETE',
      idempotencyKey,
      ifMatch: etag(endpoint.version),
    }),
  rotateWebhookSecret: (organizationId: string, endpoint: WebhookEndpoint, idempotencyKey: string) =>
    adminRequest<WebhookSecretResponse>(`/organizations/${organizationId}/webhook-endpoints/${endpoint.id}/rotate-secret`, {
      method: 'POST',
      idempotencyKey,
      ifMatch: etag(endpoint.version),
    }),
  listWebhookDeliveries: (organizationId: string, endpointId: string, params?: PageParams) =>
    fetchCollection<WebhookDelivery>(`/organizations/${organizationId}/webhook-endpoints/${endpointId}/deliveries`, params),
  /** Only a parked delivery; `If-Match` is not used. */
  redeliverWebhookDelivery: (organizationId: string, endpointId: string, deliveryId: string, idempotencyKey: string) =>
    adminRequest<WebhookDelivery>(`/organizations/${organizationId}/webhook-endpoints/${endpointId}/deliveries/${deliveryId}/redeliver`, {
      method: 'POST',
      idempotencyKey,
    }),

  usageReport: async (organizationId: string, query: UsageQuery, params?: PageParams) =>
    (await adminRequest<UsageReport>(withQuery(`/organizations/${organizationId}/usage`, { ...query, ...params }))).data,
  /** A slice charted over time, with its totals and the previous window's. */
  usageSeries: async (organizationId: string, query: UsageSeriesQuery) =>
    (await adminRequest<UsageSeriesReport>(withQuery(`/organizations/${organizationId}/usage/series`, query))).data,
  /** A slice's requests, newest first. Needs trace read. */
  usageEvents: async (organizationId: string, query: UsageSliceQuery, params?: PageParams) =>
    (await adminRequest<Page<UsageEvent>>(withQuery(`/organizations/${organizationId}/usage/events`, { ...query, ...params }))).data,
  listTraces: async (organizationId: string, query: TraceQuery, params?: PageParams) =>
    (await adminRequest<Page<TraceSummary>>(withQuery(`/organizations/${organizationId}/traces`, { ...query, ...params }))).data,
  /** Matching traces counted by one or two dimensions, largest groups first. */
  summarizeTraces: async (organizationId: string, groupBy: TraceDimension[], query: TraceQuery) =>
    (await adminRequest<TraceSummaryReport>(withQuery(`/organizations/${organizationId}/traces/summary`, { group_by: groupBy.join(','), ...query }))).data,
  getTrace: async (organizationId: string, traceId: string) =>
    (await adminRequest<TraceDetail>(`/organizations/${organizationId}/traces/${traceId}`)).data,
  listReconciliationItems: async (organizationId: string, status: ReconciliationStatus, params?: PageParams) =>
    (await adminRequest<Page<ReconciliationItem>>(withQuery(`/organizations/${organizationId}/reconciliation-items`, { status, ...params }))).data,
  /** Retrying the same resolution must reuse `idempotencyKey`, so a lost
   * response replays as success instead of failing as not open. */
  resolveReconciliationItem: (organizationId: string, itemId: string, amountMinor: number, idempotencyKey: string) =>
    adminRequest<ReconciliationItem>(`/organizations/${organizationId}/reconciliation-items/${itemId}/resolve`, {
      method: 'POST',
      body: JSON.stringify({ amount_minor: amountMinor }),
      idempotencyKey,
      ifMatch: '*',
    }),

  listTeams: (organizationId: string, params?: PageParams) =>
    fetchCollection<Team>(`/organizations/${organizationId}/teams`, params),
  createTeam: (organizationId: string, name: string) =>
    adminRequest<AccessMutationResponse>(`/organizations/${organizationId}/teams`, {
      method: 'POST',
      body: JSON.stringify({ name }),
      idempotencyKey: newIdempotencyKey(),
    }),
  updateTeam: (organizationId: string, teamId: string, name: string, version: number) =>
    adminRequest<AccessMutationResponse>(`/organizations/${organizationId}/teams/${teamId}`, {
      method: 'PATCH',
      body: JSON.stringify({ name }),
      idempotencyKey: newIdempotencyKey(),
      ifMatch: etag(version),
    }),
  deleteTeam: (organizationId: string, teamId: string, version: number) =>
    adminRequest<DeletionResponse>(`/organizations/${organizationId}/teams/${teamId}`, {
      method: 'DELETE',
      idempotencyKey: newIdempotencyKey(),
      ifMatch: etag(version),
    }),

  listOrgMembers: (organizationId: string, params?: PageParams) =>
    fetchCollection<Membership>(`/organizations/${organizationId}/members`, params),
  createOrgMember: (organizationId: string, userId: string, role: MembershipRole) =>
    adminRequest<AccessMutationResponse>(`/organizations/${organizationId}/members`, {
      method: 'POST',
      body: JSON.stringify({ user_id: userId, role }),
      idempotencyKey: newIdempotencyKey(),
    }),
  updateOrgMember: (
    organizationId: string,
    userId: string,
    patch: { role?: MembershipRole; enabled?: boolean },
    version: number,
  ) =>
    adminRequest<AccessMutationResponse>(`/organizations/${organizationId}/members/${userId}`, {
      method: 'PATCH',
      body: JSON.stringify(patch),
      idempotencyKey: newIdempotencyKey(),
      ifMatch: etag(version),
    }),
  deleteOrgMember: (organizationId: string, userId: string, version: number) =>
    adminRequest<DeletionResponse>(`/organizations/${organizationId}/members/${userId}`, {
      method: 'DELETE',
      idempotencyKey: newIdempotencyKey(),
      ifMatch: etag(version),
    }),

  listTeamMembers: (organizationId: string, teamId: string, params?: PageParams) =>
    fetchCollection<Membership>(`/organizations/${organizationId}/teams/${teamId}/members`, params),
  createTeamMember: (organizationId: string, teamId: string, userId: string, role: MembershipRole) =>
    adminRequest<AccessMutationResponse>(
      `/organizations/${organizationId}/teams/${teamId}/members`,
      {
        method: 'POST',
        body: JSON.stringify({ user_id: userId, role }),
        idempotencyKey: newIdempotencyKey(),
      },
    ),
  updateTeamMember: (
    organizationId: string,
    teamId: string,
    userId: string,
    patch: { role?: MembershipRole; enabled?: boolean },
    version: number,
  ) =>
    adminRequest<AccessMutationResponse>(
      `/organizations/${organizationId}/teams/${teamId}/members/${userId}`,
      {
        method: 'PATCH',
        body: JSON.stringify(patch),
        idempotencyKey: newIdempotencyKey(),
        ifMatch: etag(version),
      },
    ),
  deleteTeamMember: (organizationId: string, teamId: string, userId: string, version: number) =>
    adminRequest<DeletionResponse>(
      `/organizations/${organizationId}/teams/${teamId}/members/${userId}`,
      {
        method: 'DELETE',
        idempotencyKey: newIdempotencyKey(),
        ifMatch: etag(version),
      },
    ),

  listRoleBindings: (organizationId: string, params?: PageParams) =>
    fetchCollection<RoleBinding>(`/organizations/${organizationId}/role-bindings`, params),
  createRoleBinding: (organizationId: string, userId: string, role: AdminRole) =>
    adminRequest<AccessMutationResponse>(`/organizations/${organizationId}/role-bindings`, {
      method: 'POST',
      body: JSON.stringify({ user_id: userId, role }),
      idempotencyKey: newIdempotencyKey(),
    }),
  updateRoleBinding: (organizationId: string, bindingId: string, role: AdminRole, version: number) =>
    adminRequest<AccessMutationResponse>(
      `/organizations/${organizationId}/role-bindings/${bindingId}`,
      {
        method: 'PATCH',
        body: JSON.stringify({ role }),
        idempotencyKey: newIdempotencyKey(),
        ifMatch: etag(version),
      },
    ),
  deleteRoleBinding: (organizationId: string, bindingId: string, version: number) =>
    adminRequest<DeletionResponse>(`/organizations/${organizationId}/role-bindings/${bindingId}`, {
      method: 'DELETE',
      idempotencyKey: newIdempotencyKey(),
      ifMatch: etag(version),
    }),

  /** `enabled` lists only enabled (or only disabled) accounts. */
  listServiceAccounts: (organizationId: string, params?: PageParams, filter: { enabled?: boolean } = {}) =>
    fetchCollection<ServiceAccount>(`/organizations/${organizationId}/service-accounts`, params, filter.enabled === undefined ? {} : { enabled: String(filter.enabled) }),
  getServiceAccount: async (organizationId: string, serviceAccountId: string) =>
    (await adminRequest<ServiceAccount>(`/organizations/${organizationId}/service-accounts/${serviceAccountId}`)).data,
  createServiceAccount: (organizationId: string, name: string) =>
    adminRequest<ServiceAccount>(`/organizations/${organizationId}/service-accounts`, {
      method: 'POST',
      body: JSON.stringify({ name }),
      idempotencyKey: newIdempotencyKey(),
    }),
  updateServiceAccount: (organizationId: string, accountId: string, name: string, version: number) =>
    adminRequest<ServiceAccount>(`/organizations/${organizationId}/service-accounts/${accountId}`, {
      method: 'PATCH',
      body: JSON.stringify({ name }),
      idempotencyKey: newIdempotencyKey(),
      ifMatch: etag(version),
    }),
  deactivateServiceAccount: (organizationId: string, accountId: string, version: number) =>
    adminRequest<ServiceAccount>(
      `/organizations/${organizationId}/service-accounts/${accountId}/deactivate`,
      {
        method: 'POST',
        idempotencyKey: newIdempotencyKey(),
        ifMatch: etag(version),
      },
    ),
  deleteServiceAccount: (organizationId: string, accountId: string, version: number) =>
    adminRequest<ServiceAccountDeletion>(
      `/organizations/${organizationId}/service-accounts/${accountId}`,
      {
        method: 'DELETE',
        idempotencyKey: newIdempotencyKey(),
        ifMatch: etag(version),
      },
    ),

  /** `usable` lists only the keys that can be used now (active, not expired). */
  listApiKeys: (organizationId: string, accountId: string, params?: PageParams, filter: { usable?: boolean } = {}) =>
    fetchCollection<ApiKey>(
      `/organizations/${organizationId}/service-accounts/${accountId}/keys`,
      params,
      filter.usable ? { usable: 'true' } : {},
    ),
  createApiKey: (
    organizationId: string,
    accountId: string,
    body: { scopes: ApiKeyScope[]; expires_at?: string | null },
  ) =>
    adminRequest<ApiKeySecret>(
      `/organizations/${organizationId}/service-accounts/${accountId}/keys`,
      {
        method: 'POST',
        body: JSON.stringify(body),
        idempotencyKey: newIdempotencyKey(),
      },
    ),
  rotateApiKey: (organizationId: string, accountId: string, keyId: string) =>
    adminRequest<ApiKeySecret>(
      `/organizations/${organizationId}/service-accounts/${accountId}/keys/${keyId}/rotate`,
      {
        method: 'POST',
        idempotencyKey: newIdempotencyKey(),
      },
    ),
  revokeApiKey: (organizationId: string, accountId: string, keyId: string, version: number) =>
    adminRequest<ApiKey>(
      `/organizations/${organizationId}/service-accounts/${accountId}/keys/${keyId}/revoke`,
      {
        method: 'POST',
        idempotencyKey: newIdempotencyKey(),
        ifMatch: etag(version),
      },
    ),
};

/**
 * Starts a sign-in. With `organization` (its slug), through that
 * organization's own identity provider when it has one; the gateway uses its
 * own provider otherwise.
 */
export async function beginSignIn(
  navigate: (url: string) => void = url => window.location.assign(url),
  fetcher: typeof fetch = fetch,
  organization?: string,
) {
  const slug = organization?.trim();
  const query = slug ? `?organization=${encodeURIComponent(slug)}` : '';
  const response = await fetcher(`${base}/oidc/start${query}`, {
    headers: { Accept: 'application/json' },
    credentials: 'same-origin',
  });
  if (!response.ok) {
    throw new ApiError(response.status, `http_${response.status}`);
  }
  const { authorization_url: authorizationUrl } = (await response.json()) as OidcStartResponse;
  if (!authorizationUrl) {
    throw new ApiError(502, 'authorization_url_missing');
  }
  navigate(authorizationUrl);
}
