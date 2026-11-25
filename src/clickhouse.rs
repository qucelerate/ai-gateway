//! ClickHouse over its HTTP interface, for the optional analytics sink
//! (D-030; `docs/operations.md`, *Analytics in ClickHouse*).
//!
//! Plain `reqwest` rather than a ClickHouse crate: the gateway already
//! builds HTTP clients with it (rustls, timeouts, no redirects), the sink
//! needs only JSON rows in and out, and reads send PostgreSQL's freshest
//! rows as *external tables* (multipart bodies), which the crates do not
//! expose. No new dependency is added.
//!
//! Values never go into SQL text: every query takes them as typed
//! parameters (`{name:Type}`), sent beside the query, and rows travel as
//! JSON. Errors carry the HTTP status and ClickHouse's numeric exception
//! code only: a response body can quote the rows it refused.

use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Below the sink queue's lease, so a worker whose write hangs gives up
/// before another worker may claim the same rows.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const USER_AGENT: &str = concat!("gateway/", env!("CARGO_PKG_VERSION"), " analytics-sink");
pub const DEFAULT_DATABASE: &str = "gateway";
pub const DEFAULT_USER: &str = "default";

/// `GATEWAY_CLICKHOUSE_*`. The password comes from the environment like
/// every other secret (a Kubernetes Secret in the chart) and is sent in a
/// header, never in the URL.
#[derive(Clone)]
pub struct ClickHouseConfig {
    /// `http://` or `https://` (TLS) origin of the HTTP interface.
    pub url: Url,
    pub database: String,
    pub user: String,
    pub(crate) password: Option<Zeroizing<String>>,
    /// `GATEWAY_CLICKHOUSE_CA_FILE`: PEM roots trusted besides the public
    /// ones, for a private CA.
    pub(crate) ca_pem: Option<Vec<u8>>,
}

impl fmt::Debug for ClickHouseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouseConfig")
            .field("url", &self.url.as_str())
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("ca_file", &self.ca_pem.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// Why `GATEWAY_CLICKHOUSE_*` was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigProblem {
    Url,
    Database,
    User,
    CaFile,
    /// A password over `http://` to a host other than this one's loopback,
    /// without `GATEWAY_CLICKHOUSE_ALLOW_HTTP_PASSWORD=true`.
    PlaintextPassword,
}

impl ClickHouseConfig {
    /// `None` when `GATEWAY_CLICKHOUSE_URL` is unset or empty: the sink is
    /// off and nothing reads or writes ClickHouse.
    pub fn from_lookup(
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigProblem> {
        let set = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let Some(url) = set("GATEWAY_CLICKHOUSE_URL") else {
            return Ok(None);
        };
        let url = Url::parse(&url).map_err(|_| ConfigProblem::Url)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(ConfigProblem::Url);
        }
        let database =
            set("GATEWAY_CLICKHOUSE_DATABASE").unwrap_or_else(|| DEFAULT_DATABASE.into());
        if !is_identifier(&database) {
            return Err(ConfigProblem::Database);
        }
        let user = set("GATEWAY_CLICKHOUSE_USER").unwrap_or_else(|| DEFAULT_USER.into());
        if user.len() > 128 || user.chars().any(|c| c.is_control()) {
            return Err(ConfigProblem::User);
        }
        let password = lookup("GATEWAY_CLICKHOUSE_PASSWORD")
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new);
        // ClickHouse's HTTP interface takes the password in a header, so
        // over plain HTTP anyone on the path reads it. Loopback is fine (a
        // sidecar or a test); elsewhere use `https://`, or say the network
        // is already encrypted (a service mesh's mTLS) to allow it.
        if password.is_some() && url.scheme() == "http" && !is_loopback(&url) {
            let allowed = match set("GATEWAY_CLICKHOUSE_ALLOW_HTTP_PASSWORD").as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err(ConfigProblem::PlaintextPassword),
            };
            if !allowed {
                return Err(ConfigProblem::PlaintextPassword);
            }
        }
        let ca_pem = match set("GATEWAY_CLICKHOUSE_CA_FILE") {
            Some(path) => {
                let pem = std::fs::read(&path).map_err(|_| ConfigProblem::CaFile)?;
                reqwest::Certificate::from_pem_bundle(&pem).map_err(|_| ConfigProblem::CaFile)?;
                Some(pem)
            }
            None => None,
        };
        Ok(Some(Self {
            url,
            database,
            user,
            password,
            ca_pem,
        }))
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// A database name the gateway puts in DDL: letters, digits and `_`.
fn is_identifier(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !value.as_bytes()[0].is_ascii_digit()
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ClickHouseError {
    #[error("clickhouse could not be reached")]
    Transport,
    #[error("clickhouse timed out")]
    Timeout,
    #[error("clickhouse refused the request (HTTP {status}, code {code})")]
    Server { status: u16, code: u32 },
    #[error("clickhouse response could not be read")]
    Decode,
    #[error("clickhouse client could not be built")]
    Client,
}

impl ClickHouseError {
    fn from_reqwest(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Transport
        }
    }
}

/// A typed query parameter, already in ClickHouse's text form.
#[derive(Clone, Debug)]
pub struct Param {
    name: &'static str,
    value: String,
}

impl Param {
    pub fn uuid(name: &'static str, value: Uuid) -> Self {
        Self {
            name,
            value: value.to_string(),
        }
    }

    pub fn string(name: &'static str, value: &str) -> Self {
        Self {
            name,
            value: escape_param(value),
        }
    }

    pub fn int(name: &'static str, value: i64) -> Self {
        Self {
            name,
            value: value.to_string(),
        }
    }

    /// A `DateTime64(6, 'UTC')`.
    pub fn time(name: &'static str, value: DateTime<Utc>) -> Self {
        Self {
            name,
            value: format_time(value),
        }
    }

    /// An `Array(String)`.
    pub fn strings<S: AsRef<str>>(name: &'static str, values: &[S]) -> Self {
        let items: Vec<String> = values
            .iter()
            .map(|value| format!("'{}'", escape_quoted(value.as_ref())))
            .collect();
        Self {
            name,
            value: format!("[{}]", items.join(",")),
        }
    }

    /// An `Array(UUID)`.
    pub fn uuids(name: &'static str, values: &[Uuid]) -> Self {
        let items: Vec<String> = values.iter().map(|value| format!("'{value}'")).collect();
        Self {
            name,
            value: format!("[{}]", items.join(",")),
        }
    }
}

/// ClickHouse's text form of a `DateTime64(6)` in UTC.
pub fn format_time(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

/// A `String` parameter is read in ClickHouse's escaped format.
fn escape_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out
}

/// A string inside a quoted literal (array parameters).
fn escape_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out
}

/// Rows sent with a query as a temporary table (`name`), in JSONEachRow.
pub struct ExternalTable {
    pub name: &'static str,
    /// `column Type, ...`
    pub structure: &'static str,
    pub rows: Vec<u8>,
}

impl ExternalTable {
    pub fn new<T: Serialize>(
        name: &'static str,
        structure: &'static str,
        rows: impl IntoIterator<Item = T>,
    ) -> Result<Self, ClickHouseError> {
        Ok(Self {
            name,
            structure,
            rows: json_lines(rows)?,
        })
    }
}

fn json_lines<T: Serialize>(rows: impl IntoIterator<Item = T>) -> Result<Vec<u8>, ClickHouseError> {
    let mut body = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut body, &row).map_err(|_| ClickHouseError::Decode)?;
        body.push(b'\n');
    }
    Ok(body)
}

/// A ClickHouse database over HTTP. Cheap to clone.
#[derive(Clone)]
pub struct ClickHouse {
    client: reqwest::Client,
    url: Url,
    database: Arc<str>,
    user: Arc<str>,
    password: Option<Arc<Zeroizing<String>>>,
}

impl fmt::Debug for ClickHouse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClickHouse")
            .field("url", &self.url.as_str())
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

/// Settings sent with every query.
const SETTINGS: &[(&str, &str)] = &[
    // Numbers as JSON numbers, not strings.
    ("output_format_json_quote_64bit_integers", "0"),
    // A request's versions share a partition (the sort key starts with
    // its creation time), so FINAL never merges across partitions.
    ("do_not_merge_across_partitions_select_final", "1"),
    // Errors arrive as an HTTP status, never after a 200.
    ("wait_end_of_query", "1"),
    // Reads use FINAL, and these two optimizations filter rows before
    // FINAL picks each request's newest version, so a range or a flag
    // could match an old version whose newest is deleted or no longer
    // matches (reproduced on 25.8 with skip indexes in their inexact
    // mode). Pinned here so a server or profile default cannot turn them
    // on; the sort key (`created_at` bounds) does the pruning instead.
    ("use_skip_indexes_if_final", "0"),
    ("optimize_move_to_prewhere_if_final", "0"),
];

impl ClickHouse {
    pub fn new(config: &ClickHouseConfig) -> Result<Self, ClickHouseError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = config.ca_pem.as_ref() {
            for certificate in
                reqwest::Certificate::from_pem_bundle(pem).map_err(|_| ClickHouseError::Client)?
            {
                builder = builder.add_root_certificate(certificate);
            }
        }
        Ok(Self {
            client: builder.build().map_err(|_| ClickHouseError::Client)?,
            url: config.url.clone(),
            database: Arc::from(config.database.as_str()),
            user: Arc::from(config.user.as_str()),
            password: config.password.clone().map(Arc::new),
        })
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    fn request_url(&self, query: Option<&str>, params: &[Param], database: bool) -> Url {
        let mut url = self.url.clone();
        url.set_path("/");
        {
            let mut pairs = url.query_pairs_mut();
            if database {
                pairs.append_pair("database", &self.database);
            }
            for (name, value) in SETTINGS {
                pairs.append_pair(name, value);
            }
            if let Some(query) = query {
                pairs.append_pair("query", query);
            }
            for param in params {
                pairs.append_pair(&format!("param_{}", param.name), &param.value);
            }
        }
        url
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>, ClickHouseError> {
        let mut request = request.header("X-ClickHouse-User", &*self.user);
        if let Some(password) = self.password.as_ref() {
            request = request.header("X-ClickHouse-Key", password.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|error| ClickHouseError::from_reqwest(&error))?;
        let status = response.status();
        let code = response
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        let body = response
            .bytes()
            .await
            .map_err(|error| ClickHouseError::from_reqwest(&error))?;
        if !status.is_success() || code.is_some() {
            return Err(ClickHouseError::Server {
                status: status.as_u16(),
                code: code.unwrap_or(0),
            });
        }
        Ok(body.to_vec())
    }

    /// Runs a statement that returns nothing (DDL, `TRUNCATE`).
    pub async fn execute(&self, sql: &str, params: &[Param]) -> Result<(), ClickHouseError> {
        self.execute_in(sql, params, true).await
    }

    async fn execute_in(
        &self,
        sql: &str,
        params: &[Param],
        database: bool,
    ) -> Result<(), ClickHouseError> {
        let url = self.request_url(None, params, database);
        self.send(self.client.post(url).body(sql.to_owned()))
            .await
            .map(|_| ())
    }

    /// Rows of `sql` (which must not name a format), decoded from
    /// JSONEachRow. With external tables the query travels in the URL and
    /// the tables in a multipart body.
    pub async fn query<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[Param],
        tables: Vec<ExternalTable>,
    ) -> Result<Vec<T>, ClickHouseError> {
        let sql = format!("{sql}\nFORMAT JSONEachRow");
        let body = if tables.is_empty() {
            let url = self.request_url(None, params, true);
            self.send(self.client.post(url).body(sql)).await?
        } else {
            let mut url = self.request_url(Some(&sql), params, true);
            let mut form = reqwest::multipart::Form::new();
            {
                let mut pairs = url.query_pairs_mut();
                for table in &tables {
                    pairs.append_pair(&format!("{}_format", table.name), "JSONEachRow");
                    pairs.append_pair(&format!("{}_structure", table.name), table.structure);
                }
            }
            for table in tables {
                form = form.part(
                    table.name,
                    reqwest::multipart::Part::bytes(table.rows).file_name(table.name),
                );
            }
            self.send(self.client.post(url).multipart(form)).await?
        };
        body.split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).map_err(|_| ClickHouseError::Decode))
            .collect()
    }

    /// Inserts rows into `table` as one block. Waits until ClickHouse has
    /// written them, so a successful return means they are readable.
    pub async fn insert<T: Serialize>(
        &self,
        table: &'static str,
        rows: impl IntoIterator<Item = T>,
    ) -> Result<(), ClickHouseError> {
        let mut url = self.request_url(
            Some(&format!("INSERT INTO {table} FORMAT JSONEachRow")),
            &[],
            true,
        );
        url.query_pairs_mut().append_pair("async_insert", "0");
        let body = json_lines(rows)?;
        if body.is_empty() {
            return Ok(());
        }
        self.send(self.client.post(url).body(body))
            .await
            .map(|_| ())
    }

    /// The store's id (its `gateway_meta` row), if it has one and still
    /// has its `requests` table: an id beside a recreated, empty table would
    /// vouch for a copy that is gone. `None` when either table, or the
    /// database, is missing.
    pub async fn store_id(&self) -> Result<Option<Uuid>, ClickHouseError> {
        #[derive(serde::Deserialize)]
        struct IdRow {
            instance_id: Uuid,
        }
        let rows: Vec<IdRow> = match self
            .query(
                "SELECT instance_id FROM gateway_meta FINAL
                 WHERE id = {id:UInt8}
                   AND (SELECT count() FROM system.tables
                        WHERE database = currentDatabase() AND name = 'requests') = 1",
                &[Param::int("id", 1)],
                Vec::new(),
            )
            .await
        {
            Ok(rows) => rows,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(rows.first().map(|row| row.instance_id))
    }

    /// Creates the database if it is missing, then runs every schema
    /// migration's statements in order, recording the versions not yet
    /// recorded. Every statement runs each time, recorded or not, so a
    /// table dropped by hand comes back: each must be idempotent (`IF NOT
    /// EXISTS`), which also lets two workers starting at once both succeed.
    ///
    /// If `requests` was missing while `gateway_meta` was not, the store's
    /// id is removed with it, so the sink notices and copies history
    /// again instead of trusting an empty table.
    pub async fn migrate(&self) -> Result<u32, ClickHouseError> {
        #[derive(serde::Deserialize)]
        struct Count {
            n: u64,
        }
        let exists: Vec<Count> = {
            let url = self.request_url(None, &[Param::string("db", &self.database)], false);
            let body = self
                .send(self.client.post(url).body(
                    "SELECT count() AS n FROM system.databases WHERE name = {db:String} FORMAT JSONEachRow",
                ))
                .await?;
            body.split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).map_err(|_| ClickHouseError::Decode))
                .collect::<Result<_, _>>()?
        };
        if exists.first().is_none_or(|count| count.n == 0) {
            // The name passed `is_identifier`.
            self.execute_in(
                &format!("CREATE DATABASE IF NOT EXISTS {}", self.database),
                &[],
                false,
            )
            .await?;
        }
        self.execute(
            "CREATE TABLE IF NOT EXISTS gateway_schema_migrations (
                 version UInt32,
                 applied_at DateTime64(3, 'UTC') DEFAULT now64(3)
             ) ENGINE = ReplacingMergeTree ORDER BY version",
            &[],
        )
        .await?;
        #[derive(serde::Deserialize)]
        struct Applied {
            version: u32,
        }
        let applied: Vec<Applied> = self
            .query(
                "SELECT DISTINCT version FROM gateway_schema_migrations",
                &[],
                Vec::new(),
            )
            .await?;
        #[derive(serde::Deserialize)]
        struct Table {
            name: String,
        }
        let tables: Vec<Table> = self
            .query(
                "SELECT name FROM system.tables WHERE database = currentDatabase()",
                &[],
                Vec::new(),
            )
            .await?;
        let has = |name: &str| tables.iter().any(|table| table.name == name);
        let requests_lost = has(META_TABLE) && !has(REQUESTS_TABLE);
        let mut ran = 0;
        for (version, statements) in MIGRATIONS {
            for statement in *statements {
                self.execute(statement, &[]).await?;
            }
            if applied.iter().any(|row| row.version == *version) {
                continue;
            }
            #[derive(Serialize)]
            struct Version {
                version: u32,
            }
            self.insert("gateway_schema_migrations", [Version { version: *version }])
                .await?;
            ran += 1;
        }
        if requests_lost {
            self.execute(&format!("TRUNCATE TABLE {META_TABLE}"), &[])
                .await?;
        }
        Ok(ran)
    }
}

/// UNKNOWN_TABLE or UNKNOWN_DATABASE.
pub fn missing(error: &ClickHouseError) -> bool {
    matches!(error, ClickHouseError::Server { code: 60 | 81, .. })
}

/// The table every request is a row of; see migration 1.
pub const REQUESTS_TABLE: &str = "requests";
/// One row: the store's id.
pub const META_TABLE: &str = "gateway_meta";

/// ClickHouse schema migrations, by version. Never edit one that shipped:
/// add the next. Every statement runs on every start ([`ClickHouse::migrate`]),
/// so each must be idempotent (`IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`).
const MIGRATIONS: &[(u32, &[&str])] = &[(
    1,
    &[
        "CREATE TABLE IF NOT EXISTS gateway_meta (
             id UInt8,
             instance_id UUID,
             created_at DateTime64(6, 'UTC') DEFAULT now64(6)
         ) ENGINE = ReplacingMergeTree(created_at) ORDER BY id",
        // One row per request (reservation), its newest version winning
        // (`version`: microseconds of the PostgreSQL read it came from), and
        // a deleted request hidden (`is_deleted`). The sort key never
        // changes for a request, so every version replaces the last.
        "CREATE TABLE IF NOT EXISTS requests (
             organization_id UUID,
             created_at DateTime64(6, 'UTC'),
             reservation_id UUID,
             version UInt64,
             is_deleted UInt8,
             has_trace UInt8,
             trace_id Nullable(UUID),
             recorded_at Nullable(DateTime64(6, 'UTC')),
             correlation_id Nullable(UUID),
             team_id Nullable(UUID),
             service_account_id Nullable(UUID),
             reservation_team_id Nullable(UUID),
             reservation_service_account_id Nullable(UUID),
             api_key_id Nullable(UUID),
             end_user Nullable(String),
             model LowCardinality(Nullable(String)),
             reservation_model LowCardinality(String),
             alias LowCardinality(Nullable(String)),
             deployment LowCardinality(String),
             provider_family LowCardinality(Nullable(String)),
             location LowCardinality(Nullable(String)),
             operation LowCardinality(Nullable(String)),
             request_status LowCardinality(Nullable(String)),
             trace_status LowCardinality(Nullable(String)),
             outcome LowCardinality(Nullable(String)),
             error_category LowCardinality(Nullable(String)),
             trace_currency LowCardinality(Nullable(String)),
             trace_amount_minor Nullable(Int64),
             duration_ms Nullable(Int64),
             batch_id Nullable(UUID),
             model_master LowCardinality(Nullable(String)),
             has_usage UInt8,
             usage_currency LowCardinality(Nullable(String)),
             usage_confidence LowCardinality(Nullable(String)),
             usage_completeness LowCardinality(Nullable(String)),
             input_tokens Nullable(Int64),
             output_tokens Nullable(Int64),
             cached_input_tokens Nullable(Int64),
             reasoning_tokens Nullable(Int64),
             other_tokens Nullable(Int64),
             usage_amount_minor Nullable(Int64),
             usage_attributed_at Nullable(DateTime64(6, 'UTC'))
         ) ENGINE = ReplacingMergeTree(version, is_deleted)
         PARTITION BY toYYYYMM(created_at)
         ORDER BY (organization_id, created_at, reservation_id)",
    ],
)];

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn unset_url_turns_the_sink_off() {
        assert!(
            ClickHouseConfig::from_lookup(&lookup(&[]))
                .unwrap()
                .is_none()
        );
        assert!(
            ClickHouseConfig::from_lookup(&lookup(&[("GATEWAY_CLICKHOUSE_URL", " ")]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn settings_have_defaults_and_keep_the_password_out_of_debug() {
        let config = ClickHouseConfig::from_lookup(&lookup(&[
            ("GATEWAY_CLICKHOUSE_URL", "https://ch.example:8443"),
            ("GATEWAY_CLICKHOUSE_PASSWORD", "s3cret"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.database, "gateway");
        assert_eq!(config.user, "default");
        assert_eq!(config.url.scheme(), "https");
        let debug = format!("{config:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
    }

    #[test]
    fn a_password_goes_over_plain_http_only_to_loopback_or_when_allowed() {
        let with = |url: &str, allow: Option<&str>| {
            let mut pairs = vec![
                ("GATEWAY_CLICKHOUSE_URL", url),
                ("GATEWAY_CLICKHOUSE_PASSWORD", "s3cret"),
            ];
            if let Some(allow) = allow {
                pairs.push(("GATEWAY_CLICKHOUSE_ALLOW_HTTP_PASSWORD", allow));
            }
            ClickHouseConfig::from_lookup(&lookup(&pairs)).map(|config| config.is_some())
        };
        assert_eq!(
            with("http://ch:8123", None),
            Err(ConfigProblem::PlaintextPassword)
        );
        assert_eq!(
            with("http://ch:8123", Some("yes")),
            Err(ConfigProblem::PlaintextPassword)
        );
        assert_eq!(with("http://ch:8123", Some("true")), Ok(true));
        for url in [
            "https://ch:8443",
            "http://127.0.0.1:8123",
            "http://[::1]:8123",
            "http://localhost:8123",
        ] {
            assert_eq!(with(url, None), Ok(true), "{url}");
        }
        // No password, nothing to protect.
        assert!(
            ClickHouseConfig::from_lookup(&lookup(&[("GATEWAY_CLICKHOUSE_URL", "http://ch:8123")]))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn urls_with_credentials_paths_or_queries_and_odd_names_are_refused() {
        for url in [
            "ftp://ch.example",
            "https://user:pw@ch.example",
            "https://ch.example/?database=x",
            "https://ch.example/play",
            "not a url",
        ] {
            assert_eq!(
                ClickHouseConfig::from_lookup(&lookup(&[("GATEWAY_CLICKHOUSE_URL", url)]))
                    .unwrap_err(),
                ConfigProblem::Url,
                "{url}"
            );
        }
        for database in ["a-b", "1st", "x; DROP", &"d".repeat(65)] {
            assert_eq!(
                ClickHouseConfig::from_lookup(&lookup(&[
                    ("GATEWAY_CLICKHOUSE_URL", "http://ch:8123"),
                    ("GATEWAY_CLICKHOUSE_DATABASE", database),
                ]))
                .unwrap_err(),
                ConfigProblem::Database,
                "{database}"
            );
        }
        assert_eq!(
            ClickHouseConfig::from_lookup(&lookup(&[
                ("GATEWAY_CLICKHOUSE_URL", "http://ch:8123"),
                ("GATEWAY_CLICKHOUSE_CA_FILE", "/nonexistent/ca.pem"),
            ]))
            .unwrap_err(),
            ConfigProblem::CaFile
        );
    }

    #[test]
    fn parameters_are_escaped_for_their_format() {
        assert_eq!(Param::string("s", "a\\b\tc\n").value, "a\\\\b\\tc\\n");
        assert_eq!(
            Param::strings("a", &["x'y", "b\\c"]).value,
            "['x\\'y','b\\\\c']"
        );
        assert_eq!(Param::strings::<&str>("a", &[]).value, "[]");
        let id = Uuid::nil();
        assert_eq!(
            Param::uuids("u", &[id]).value,
            "['00000000-0000-0000-0000-000000000000']"
        );
        let at = DateTime::parse_from_rfc3339("2026-06-10T01:02:03.000004Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(Param::time("t", at).value, "2026-06-10 01:02:03.000004");
    }
}
