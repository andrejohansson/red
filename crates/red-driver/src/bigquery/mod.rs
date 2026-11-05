//! BigQuery driver: the seventh source of `DatabaseDriver` and the odd one out in
//! almost every structural respect.
//!
//! Built on the **REST API** over the same reqwest/rustls stack the ClickHouse
//! driver uses, and for the same reason: no heavy native-protocol client, and out-
//! of-band cancel is a second request (`jobs.cancel`, the shape ClickHouse's
//! `KILL QUERY` already proves). Four things make it unlike every other driver in
//! this crate, and each one drove a decision recorded in `docs/adr/`:
//!
//! - **There is no connection.** Every request carries a short-lived bearer token
//!   (see [`auth`]), so this driver holds a credential that expires where the
//!   others hold a socket or a pool.
//! - **A project is not a namespace.** BigQuery nests project → dataset → table,
//!   one level deeper than [`TableRef`], so a connection pins one project and the
//!   dataset is the namespace (`docs/adr/0001`). [`quote_table`] is where the
//!   pinned project gets injected back.
//! - **Reads cost money.** Every query is a job billed by bytes scanned, and the
//!   trait's read seams are stateless — `count`, `fetch_page` and `key_bounds` each
//!   take a SQL string and the browse open probe fires all three at once. So the
//!   driver caches the job a result already ran as and re-reads it by `startIndex`
//!   at no further scan cost (`docs/adr/0002`), and prices a query before running
//!   it (`docs/adr/0004`).
//! - **Offset paging beats keyset paging**, the exact reverse of every other
//!   engine: a keyset seek is a fresh scan per page, while re-reading a finished
//!   job is free. `DbKind::supports_keyset_paging` is what turns the seek off.
//!
//! Writes are insert-only (`docs/adr/0003`): a writable connection is a copy /
//! migration target, but in-grid `UPDATE`/`DELETE` stay off because a point edit
//! here is a full-table scan.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use red_core::{
    Column, ColumnMeta, ColumnPredicate, ColumnValue, ConnectionConfig, DbKind, EditOp,
    ExportFormat, ExportOutcome, FkEdge, FkJoin, KeySpec, ObjectKind, ProxyConfig, ProxyKind,
    QueryOptions, QueryPlan, RedError, Result, ResultPage, RowEditCaps, RowWindow, SchemaMeta,
    TableDetail, TableRef, Value,
};
use serde_json::Value as Json;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::format::{ExportWriter, ProgressThrottle, strip_trailing};
use crate::{
    AbortSignal, CancelToken, CellCap, DatabaseDriver, PageCap, QueryCursor, driver_err,
    human_bytes, refuse_if_read_only, window_prealloc,
};

mod auth;
mod catalog;
#[cfg(test)]
mod tests;
mod value;
mod write;

use auth::{Credentials, TokenSource};
use value::{Field, read_row, read_schema};

/// The API root. A field on the driver rather than a constant so the mock-server
/// tests can drive every path in this file without credentials or a project — the
/// always-run half of the test strategy.
const DEFAULT_BASE_URL: &str = "https://bigquery.googleapis.com/bigquery/v2";

/// How long a cached job stays usable (`docs/adr/0002`).
///
/// This is a **correctness** parameter, not a performance one. BigQuery's own temp
/// result tables live 24 hours and the cache could too; it must not, because RED's
/// refresh re-opens a result with *identical* SQL, so a long TTL would answer a
/// deliberate refresh with minute-old rows and nothing in the UI to say so. A
/// minute is long enough for the three-way open probe and a scroll burst to share
/// one job, short enough that any human refresh re-runs. Wrong in this direction
/// costs one extra scan; wrong in the other shows false data.
const JOB_CACHE_TTL: Duration = Duration::from_secs(60);

/// How many jobs to remember. One per open result plus headroom; the eviction is
/// oldest-first, and a miss is a re-run rather than a failure.
const JOB_CACHE_MAX: usize = 32;

/// How long to let the server wait for a job before falling back to polling. Short
/// enough to keep a cancel responsive, long enough that a quick query completes in
/// the submitting request.
const SERVER_WAIT: Duration = Duration::from_secs(8);

/// Gap between `getQueryResults` polls for a job still running.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Items per page when walking a REST list endpoint. The API's own default is 50,
/// which would make the schema tree a series of round trips for no reason.
const LIST_PAGE_SIZE: usize = 1000;

/// Pages one list walk will follow before giving up. At [`LIST_PAGE_SIZE`] this is
/// 50,000 items — far past any real dataset, and a backstop against a
/// `nextPageToken` that never clears rather than a limit anyone should meet.
const LIST_PAGE_CAP: usize = 50;

/// The cost defaults live in `red-core`: the app's settings layer needs them for
/// its own `Default` impl and does not depend on `red-driver`.
pub use red_core::{DEFAULT_COST_CONFIRM_BYTES, DEFAULT_MAX_BYTES_BILLED};

/// The live caps, process-global for the same reason `DISPLAY_CELL_CAP` is: a
/// settings change should apply to every subsequent query across all sessions
/// without threading the value through each call. The service sets them on launch
/// and on every reload.
static MAX_BYTES_BILLED: AtomicU64 = AtomicU64::new(DEFAULT_MAX_BYTES_BILLED);
static COST_CONFIRM_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_COST_CONFIRM_BYTES);

/// Set the BigQuery cost guards: the hard `maximumBytesBilled` ceiling every job
/// carries, and the estimate above which a query is confirmed instead of run.
/// `0` disables either guard.
pub fn set_bigquery_cost_caps(max_bytes_billed: u64, confirm_bytes: u64) {
    MAX_BYTES_BILLED.store(max_bytes_billed, Ordering::Relaxed);
    COST_CONFIRM_BYTES.store(confirm_bytes, Ordering::Relaxed);
}

/// A submitted job's identity: everything `getQueryResults` and `jobs.cancel` need.
///
/// `location` is *learned*, never configured — from the dataset at connect when one
/// is bound, else from the job's own `jobReference.location`. It matters because
/// both follow-up calls require it for any region outside the US/EU multi-regions,
/// and a cancel that cannot address its job is a cancel that keeps billing.
#[derive(Clone, Debug)]
struct JobRef {
    id: String,
    location: Option<String>,
}

/// One page of results as BigQuery returns it.
struct Batch {
    columns: Vec<Column>,
    fields: Vec<Field>,
    rows: Vec<Json>,
    page_token: Option<String>,
    total_rows: Option<u64>,
    /// `numDmlAffectedRows`, present only for a DML job.
    affected: Option<u64>,
}

/// A live BigQuery session. Every field after `read_only` is shared by
/// [`DatabaseDriver::scoped`], which is a field-level clone: the token cache and
/// the job cache must be shared, or a scoped handle would re-authenticate and
/// re-run every query the parent had already paid for.
pub struct BigqueryDriver {
    client: reqwest::Client,
    base_url: String,
    /// The pinned GCP project (`docs/adr/0001`). Also the billing project for every
    /// job, which is why it is the connection's authority rather than a namespace.
    project: String,
    /// The connection's default dataset.
    dataset: String,
    /// Overrides [`Self::dataset`] for this handle's requests (see
    /// [`DatabaseDriver::scoped`]). BigQuery names `defaultDataset` per job, so
    /// rebinding costs nothing at all — no extra round trip, just a different field.
    scope: Option<String>,
    read_only: bool,
    tokens: Arc<TokenSource>,
    location: Arc<Mutex<Option<String>>>,
    jobs: Arc<Mutex<Vec<(String, JobRef, Instant)>>>,
}

impl BigqueryDriver {
    /// Dial a BigQuery connection: resolve credentials, then confirm they work.
    ///
    /// `dsn` is the `sqlalchemy-bigquery` form, `bigquery://<project>/<dataset>`
    /// with an optional `?credentials_path=` (`docs/adr/0005`).
    ///
    /// `proxy` is handed to the HTTP client **natively** rather than being stood up
    /// as a local port forward the way every other engine's is. A forward is what
    /// breaks here: dialling `127.0.0.1` cannot present a valid certificate for
    /// `bigquery.googleapis.com`, so the connection would fail at the TLS handshake
    /// with a confusing error rather than at setup with a clear one.
    pub async fn connect(dsn: &str, read_only: bool, proxy: Option<&ProxyConfig>) -> Result<Self> {
        let parsed = ConnectionConfig::parse_conn_str(dsn).ok_or_else(|| {
            RedError::Connect(format!(
                "not a BigQuery connection string: {dsn:?}. Expected \
                 bigquery://<project>/<dataset>"
            ))
        })?;
        if parsed.host.trim().is_empty() {
            return Err(RedError::Connect(
                "a BigQuery connection needs a project: bigquery://<project>/<dataset>".to_string(),
            ));
        }
        let mut builder = reqwest::Client::builder();
        if let Some(p) = proxy {
            builder = builder.proxy(build_proxy(p)?);
        }
        let client = builder.build().map_err(driver_err)?;
        let creds = Credentials::load(&parsed.credentials_path)?;
        let driver = Self {
            client: client.clone(),
            base_url: DEFAULT_BASE_URL.to_string(),
            project: parsed.host,
            dataset: parsed.database,
            scope: None,
            read_only,
            tokens: Arc::new(TokenSource::new(creds, client)),
            location: Arc::new(Mutex::new(None)),
            jobs: Arc::new(Mutex::new(Vec::new())),
        };
        // Resolving the dataset's region here rather than reading it off the first
        // job is what makes cancel correct from the first query: `jobs.cancel` needs
        // the location for any non-US/EU region, and a job we cannot address is a
        // job that keeps scanning after the user gave up on it.
        driver.resolve_location().await;
        driver.ping().await?;
        Ok(driver)
    }

    /// Point this driver at a different API root. Test-only seam: it is what lets
    /// the mock-server suite exercise paging, the job cache, and error mapping with
    /// no credentials and no cost.
    #[cfg(test)]
    #[allow(dead_code)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// The dataset this handle's unqualified names bind against.
    fn namespace(&self) -> &str {
        self.scope.as_deref().unwrap_or(&self.dataset)
    }

    /// Learn the bound dataset's region, best-effort. A failure is silent on
    /// purpose: the region is an optimisation for addressing jobs, and a connection
    /// whose dataset is not readable yet (or not named at all) must still open.
    async fn resolve_location(&self) {
        if self.dataset.trim().is_empty() {
            return;
        }
        let path = format!("projects/{}/datasets/{}", self.project, self.dataset);
        if let Ok(json) = self.get(&path, &[]).await
            && let Some(loc) = json.get("location").and_then(Json::as_str)
        {
            *self.location.lock().await = Some(loc.to_string());
        }
    }

    /// The known region, if any.
    async fn known_location(&self) -> Option<String> {
        self.location.lock().await.clone()
    }

    /// A GET against the API, authenticated and error-mapped.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Json> {
        let token = self.tokens.token().await?;
        let request = self
            .client
            .get(format!("{}/{path}", self.base_url))
            .bearer_auth(&*token)
            .query(query);
        self.send(request).await
    }

    /// Page through a REST *list* endpoint, following `nextPageToken`, and collect
    /// every item under `key`.
    ///
    /// Paginating rather than reading the first page is not a nicety: `datasets.list`
    /// and `tables.list` both default to 50 items, so a single-page read would draw a
    /// silently truncated schema tree — the failure mode that reads as "those tables
    /// don't exist". [`LIST_PAGE_CAP`] bounds a pathological project rather than
    /// looping forever, and says so in the log when it bites.
    async fn list_items(&self, path: &str, key: &str) -> Result<Vec<Json>> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        for page in 0..LIST_PAGE_CAP {
            let mut query = vec![("maxResults", LIST_PAGE_SIZE.to_string())];
            if let Some(t) = &token {
                query.push(("pageToken", t.clone()));
            }
            let json = self.get(path, &query).await?;
            if let Some(rows) = json.get(key).and_then(Json::as_array) {
                items.extend(rows.iter().cloned());
            }
            token = json
                .get("nextPageToken")
                .and_then(Json::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if token.is_none() {
                return Ok(items);
            }
            if page + 1 == LIST_PAGE_CAP {
                tracing::warn!(
                    path,
                    items = items.len(),
                    "bigquery: stopped listing at the page cap; the tree is incomplete"
                );
            }
        }
        Ok(items)
    }

    /// A POST against the API, authenticated and error-mapped.
    async fn post(&self, path: &str, body: &Json) -> Result<Json> {
        let token = self.tokens.token().await?;
        let request = self
            .client
            .post(format!("{}/{path}", self.base_url))
            .bearer_auth(&*token)
            .json(body);
        self.send(request).await
    }

    /// Send a prepared request and turn a non-2xx into a typed [`RedError`].
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Json> {
        let response = request.send().await.map_err(driver_err)?;
        let status = response.status();
        let body = response.text().await.map_err(driver_err)?;
        if status.is_success() {
            // A 200 with an empty body is legal for `jobs.cancel`.
            return if body.trim().is_empty() {
                Ok(Json::Null)
            } else {
                serde_json::from_str(&body).map_err(driver_err)
            };
        }
        Err(api_error(status.as_u16(), &body))
    }

    /// The `configuration.query` half of a job, shared by submit and dry run.
    fn query_config(&self, sql: &str, params: &[&Value], columns: &[Column]) -> Json {
        let mut query = serde_json::json!({
            "query": sql,
            // GoogleSQL, never legacy SQL: the whole driver assumes standard
            // identifiers, `?` parameters, and INFORMATION_SCHEMA.
            "useLegacySql": false,
        });
        let namespace = self.namespace();
        if !namespace.is_empty() {
            query["defaultDataset"] = serde_json::json!({
                "projectId": self.project,
                "datasetId": namespace,
            });
        }
        let cap = MAX_BYTES_BILLED.load(Ordering::Relaxed);
        if cap > 0 {
            // An int64 field, which the API requires as a *string*.
            query["maximumBytesBilled"] = Json::String(cap.to_string());
        }
        if !params.is_empty() {
            query["parameterMode"] = Json::String("POSITIONAL".to_string());
            query["queryParameters"] = Json::Array(
                params
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        write::query_parameter(
                            v,
                            columns.get(i).and_then(|c| c.decl_type.as_deref()),
                        )
                    })
                    .collect(),
            );
        }
        query
    }

    /// Price a statement without running it: BigQuery's `dryRun`, which returns the
    /// bytes it *would* scan and costs nothing.
    async fn dry_run(&self, sql: &str) -> Result<u64> {
        let body = serde_json::json!({
            "configuration": {
                "dryRun": true,
                "query": self.query_config(sql, &[], &[]),
            }
        });
        let json = self
            .post(&format!("projects/{}/jobs", self.project), &body)
            .await?;
        Ok(json
            .pointer("/statistics/totalBytesProcessed")
            .and_then(json_u64)
            .unwrap_or(0))
    }

    /// Apply the two cost tiers to a user-initiated query (`docs/adr/0004`).
    ///
    /// Over the hard cap it is refused outright; between the two thresholds it is
    /// reported as needing confirmation and **not** run. Approval arrives as
    /// [`QueryOptions::cost_confirmed`] on the re-send, which skips this entirely.
    /// A dry run is a round trip, so it is only spent when a threshold is actually
    /// armed.
    async fn gate_cost(&self, sql: &str, confirmed: bool) -> Result<()> {
        let confirm = COST_CONFIRM_BYTES.load(Ordering::Relaxed);
        let cap = MAX_BYTES_BILLED.load(Ordering::Relaxed);
        if confirmed || (confirm == 0 && cap == 0) {
            return Ok(());
        }
        let bytes = self.dry_run(sql).await?;
        if cap > 0 && bytes > cap {
            // Refused here as well as by `maximumBytesBilled` on the job itself:
            // this path can name both numbers, where the engine's own rejection
            // arrives as an opaque quota message.
            return Err(RedError::Query(format!(
                "this query would scan {} , over the {} cap; raise \
                 `[bigquery] max_bytes_billed` to run it",
                human_bytes(bytes),
                human_bytes(cap)
            )));
        }
        if confirm > 0 && bytes > confirm {
            return Err(RedError::QueryCostConfirm {
                bytes,
                limit: confirm,
            });
        }
        Ok(())
    }

    /// Submit a job and return its identity, without reading any rows.
    ///
    /// The job id is minted **here**, not by BigQuery, which is the whole reason
    /// this uses `jobs.insert` rather than the simpler `jobs.query`: knowing the id
    /// before the request returns is what lets [`AbortSignal`] cancel a job that is
    /// already scanning. `jobs.query` allocates the id server-side, so a cancel
    /// arriving during submission would have nothing to address and the query would
    /// bill on regardless — the same reasoning behind ClickHouse's client-minted
    /// `query_id`.
    async fn submit(&self, sql: &str, params: &[&Value], columns: &[Column]) -> Result<JobRef> {
        let id = format!("red_{}", uuid::Uuid::new_v4().simple());
        let location = self.known_location().await;
        let mut reference = serde_json::json!({ "projectId": self.project, "jobId": id });
        if let Some(loc) = &location {
            reference["location"] = Json::String(loc.clone());
        }
        let body = serde_json::json!({
            "jobReference": reference,
            "configuration": { "query": self.query_config(sql, params, columns) },
        });
        let json = self
            .post(&format!("projects/{}/jobs", self.project), &body)
            .await?;
        job_error(&json)?;
        // The response's own location is authoritative: it is what BigQuery
        // resolved, which may differ from what we guessed (or be the first we learn
        // when no dataset was bound).
        let resolved = json
            .pointer("/jobReference/location")
            .and_then(Json::as_str)
            .map(str::to_string)
            .or(location);
        if let Some(loc) = &resolved
            && self.known_location().await.is_none()
        {
            *self.location.lock().await = Some(loc.clone());
        }
        Ok(JobRef {
            id,
            location: resolved,
        })
    }

    /// Read one page of a job's results.
    ///
    /// `start_index` is the random-access door — it reads from the job's finished
    /// result set at no further scan cost, which is what makes `fetch_page` cheap
    /// here and what the job cache exists to exploit. `page_token` is the streaming
    /// door, used by the cursor.
    async fn results(
        &self,
        job: &JobRef,
        start_index: Option<u64>,
        page_token: Option<&str>,
        max: usize,
    ) -> Result<Batch> {
        let mut query: Vec<(&str, String)> = vec![
            ("maxResults", max.to_string()),
            ("timeoutMs", SERVER_WAIT.as_millis().to_string()),
        ];
        if let Some(loc) = &job.location {
            query.push(("location", loc.clone()));
        }
        if let Some(i) = start_index {
            query.push(("startIndex", i.to_string()));
        }
        if let Some(t) = page_token {
            query.push(("pageToken", t.to_string()));
        }
        let path = format!("projects/{}/queries/{}", self.project, job.id);
        loop {
            let json = self.get(&path, &query).await?;
            job_error(&json)?;
            // `jobComplete: false` means the server's wait elapsed with the query
            // still running; poll rather than treating it as an empty result.
            if !json
                .get("jobComplete")
                .and_then(Json::as_bool)
                .unwrap_or(true)
            {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            let (columns, fields) = read_schema(json.get("schema"));
            return Ok(Batch {
                columns,
                fields,
                rows: json
                    .get("rows")
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default(),
                page_token: json
                    .get("pageToken")
                    .and_then(Json::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
                total_rows: json.get("totalRows").and_then(json_u64),
                affected: json.get("numDmlAffectedRows").and_then(json_u64),
            });
        }
    }

    /// Stop a job. Best-effort and never surfaced: it runs on the cancel path,
    /// where the caller already knows what it asked for, and a failure here must
    /// not mask the `Interrupted` the user is waiting to see.
    async fn cancel_job(&self, job: &JobRef) {
        let mut query = Vec::new();
        if let Some(loc) = &job.location {
            query.push(("location", loc.clone()));
        }
        let path = format!("projects/{}/jobs/{}/cancel", self.project, job.id);
        let token = match self.tokens.token().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("bigquery: cannot authenticate to cancel {}: {e}", job.id);
                return;
            }
        };
        let request = self
            .client
            .post(format!("{}/{path}", self.base_url))
            .bearer_auth(&*token)
            .query(&query)
            .json(&Json::Object(Default::default()));
        if let Err(e) = request.send().await {
            tracing::warn!("bigquery: cancel of {} failed: {e}", job.id);
        }
    }

    /// The job that already ran `sql` for this namespace, if one is still fresh.
    /// Expired entries are dropped on the way past, so the cache needs no sweeper.
    async fn cached_job(&self, sql: &str) -> Option<JobRef> {
        let key = self.cache_key(sql);
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|(_, _, at)| at.elapsed() < JOB_CACHE_TTL);
        jobs.iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, job, _)| job.clone())
    }

    async fn remember_job(&self, sql: &str, job: &JobRef) {
        let key = self.cache_key(sql);
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|(k, _, at)| *k != key && at.elapsed() < JOB_CACHE_TTL);
        if jobs.len() >= JOB_CACHE_MAX {
            jobs.remove(0);
        }
        jobs.push((key, job.clone(), Instant::now()));
    }

    /// The cache key. The namespace is part of it because the *same* SQL means a
    /// different query under a different `defaultDataset` — keying on SQL alone
    /// would serve one dataset's rows for another's browse.
    fn cache_key(&self, sql: &str) -> String {
        format!("{}\u{0}{sql}", self.namespace())
    }

    /// Run `sql` to a job, reusing the cached one when it is still fresh, and arm
    /// `abort` to cancel it at the engine.
    async fn job_for(&self, sql: &str, abort: &AbortSignal) -> Result<JobRef> {
        if let Some(job) = self.cached_job(sql).await {
            return Ok(job);
        }
        let job = self.submit(sql, &[], &[]).await?;
        // Arm the cancel only after the id exists, but note that the id was minted
        // client-side, so this covers the scan itself rather than just the wait.
        let token = self.cancel_token(&job);
        let _guard = abort.arm(token);
        if abort.is_aborted() {
            self.cancel_job(&job).await;
            return Err(RedError::Interrupted);
        }
        self.remember_job(sql, &job).await;
        Ok(job)
    }

    /// A cancel handle for one job: fires `jobs.cancel` on its own task, since
    /// [`CancelToken`] is synchronous and the cancel is a round trip.
    fn cancel_token(&self, job: &JobRef) -> CancelToken {
        let driver = self.clone_handle();
        let job = job.clone();
        CancelToken::new(move || {
            let driver = driver.clone_handle();
            let job = job.clone();
            tokio::spawn(async move { driver.cancel_job(&job).await });
        })
    }

    /// A field-level clone sharing the token cache, job cache, and learned region.
    fn clone_handle(&self) -> Self {
        Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            project: self.project.clone(),
            dataset: self.dataset.clone(),
            scope: self.scope.clone(),
            read_only: self.read_only,
            tokens: Arc::clone(&self.tokens),
            location: Arc::clone(&self.location),
            jobs: Arc::clone(&self.jobs),
        }
    }

    /// Collect a bounded read into rows: submit (or reuse), then read one page.
    /// The workhorse behind `fetch_page`, `count`, `column_stats` and the catalog.
    async fn run_collect(
        &self,
        sql: &str,
        limit: usize,
        abort: &AbortSignal,
        cap: Option<CellCap>,
    ) -> Result<ResultPage> {
        let job = self.job_for(sql, abort).await?;
        let batch = self.results(&job, None, None, limit).await?;
        Ok(ResultPage {
            rows: batch
                .rows
                .iter()
                .map(|r| read_row(r, &batch.fields, cap))
                .collect(),
            columns: batch.columns,
        })
    }

    /// One row of a single-row read (`count`, `column_stats`, `key_bounds`).
    async fn one_row(&self, sql: &str, abort: &AbortSignal) -> Result<Vec<Value>> {
        let page = self.run_collect(sql, 1, abort, None).await?;
        Ok(page.rows.into_iter().next().unwrap_or_default())
    }

    /// Run a statement that returns no rows, and report what it affected. Used by
    /// every write path; refuses on a read-only connection first.
    async fn run_dml(&self, sql: &str, params: &[&Value], columns: &[Column]) -> Result<u64> {
        refuse_if_read_only(self.read_only)?;
        let job = self.submit(sql, params, columns).await?;
        let batch = self.results(&job, None, None, 0).await?;
        Ok(batch.affected.unwrap_or(0))
    }
}

/// Translate a [`ProxyConfig`] into a `reqwest` proxy. Both kinds tunnel every
/// scheme, so one proxy covers the API and the OAuth token endpoint alike --
/// which matters, because a network that needs a proxy to reach
/// `bigquery.googleapis.com` needs one to reach `oauth2.googleapis.com` too.
fn build_proxy(config: &ProxyConfig) -> Result<reqwest::Proxy> {
    let scheme = match config.kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::HttpConnect => "http",
    };
    let proxy = reqwest::Proxy::all(format!("{scheme}://{}:{}", config.host, config.port))
        .map_err(|e| RedError::Connect(format!("invalid proxy for BigQuery: {e}")))?;
    Ok(if config.user.is_empty() {
        proxy
    } else {
        proxy.basic_auth(&config.user, &config.password)
    })
}

/// Quote one identifier for GoogleSQL.
///
/// BigQuery quotes with backticks and, unlike every other engine here, provides no
/// escape for a backtick *inside* an identifier — such a name simply cannot exist.
/// Stripping is therefore not lossy for any real identifier, and it closes the
/// injection route a raw interpolation would open.
fn bq_quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', ""))
}

/// Read a JSON int64 field, which BigQuery may send as a number *or* as a string
/// (it does both, per field, and the string form is the one that survives values
/// above 2^53).
fn json_u64(v: &Json) -> Option<u64> {
    match v {
        Json::Number(n) => n.as_u64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Map an HTTP error body onto a typed [`RedError`].
fn api_error(status: u16, body: &str) -> RedError {
    let message = serde_json::from_str::<Json>(body)
        .ok()
        .and_then(|j| {
            j.pointer("/error/message")
                .and_then(Json::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string());
    match status {
        // A bad or expired token, or a role without the right. Fatal for the
        // connect path's classifier, so the UI stops retrying and prompts.
        401 | 403 => RedError::Auth(message),
        404 => RedError::Query(message),
        _ => RedError::Query(message),
    }
}

/// Surface a job-level failure, which BigQuery reports **inside a 200 response**
/// rather than as an HTTP status. Missing this is how a failed query would read as
/// an empty result.
fn job_error(json: &Json) -> Result<()> {
    let Some(err) = json.pointer("/status/errorResult") else {
        return Ok(());
    };
    let reason = err.get("reason").and_then(Json::as_str).unwrap_or("");
    let message = err
        .get("message")
        .and_then(Json::as_str)
        .unwrap_or("query failed")
        .to_string();
    Err(match reason {
        // The user's own cancel, coming back as a job status.
        "stopped" | "cancelled" => RedError::Interrupted,
        "bytesBilledLimitExceeded" | "quotaExceeded" => RedError::Query(format!(
            "{message} (RED caps every job at {}; raise `[bigquery] max_bytes_billed`)",
            human_bytes(MAX_BYTES_BILLED.load(Ordering::Relaxed))
        )),
        "accessDenied" => RedError::Auth(message),
        _ => RedError::Query(message),
    })
}

/// A streaming cursor over one job's results, walking `pageToken`.
struct BqCursor {
    columns: Vec<Column>,
    fields: Vec<Field>,
    driver: BigqueryDriver,
    job: JobRef,
    cancel: CancelToken,
    cancelled: Arc<AtomicBool>,
    full: bool,
    window: usize,
    /// The next page's token, and whether the stream is drained. `None` token with
    /// `started` set means the result ended.
    state: Mutex<(Option<String>, bool)>,
}

#[async_trait]
impl QueryCursor for BqCursor {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    async fn next_window(&self, max: usize) -> Result<RowWindow> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(RedError::Interrupted);
        }
        let mut state = self.state.lock().await;
        let (token, started) = (state.0.clone(), state.1);
        if started && token.is_none() {
            return Ok(RowWindow {
                rows: Vec::new(),
                exhausted: true,
            });
        }
        // `max` is caller-supplied and can be enormous (the cancel-mid-fetch test
        // asks for a billion), so the request is bounded by the configured window.
        let want = max.min(self.window.max(1));
        let batch = self
            .driver
            .results(&self.job, None, token.as_deref(), want)
            .await?;
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(RedError::Interrupted);
        }
        let cap = if self.full {
            None
        } else {
            CellCap::display([None, None])
        };
        let mut rows = Vec::with_capacity(window_prealloc(want));
        for row in &batch.rows {
            rows.push(read_row(row, &self.fields, cap));
        }
        *state = (batch.page_token.clone(), true);
        Ok(RowWindow {
            exhausted: batch.page_token.is_none(),
            rows,
        })
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

#[async_trait]
impl DatabaseDriver for BigqueryDriver {
    async fn ping(&self) -> Result<()> {
        // Reads no table data and bills nothing either way. When the connection
        // names a dataset, prove *that* rather than the project: a role granted
        // access to one dataset and nothing else (the common least-privilege shape)
        // can read its dataset but may not enumerate the project's, so probing the
        // list would refuse a connection that works perfectly.
        let path = match self.namespace() {
            ns if !ns.is_empty() => format!("projects/{}/datasets/{ns}", self.project),
            _ => format!("projects/{}/datasets", self.project),
        };
        self.get(&path, &[("maxResults", "1".to_string())])
            .await
            .map(|_| ())
    }

    fn server_version(&self) -> String {
        // BigQuery is a service, not a versioned server: there is no number to
        // report and inventing one would be worse than saying so.
        "BigQuery".to_string()
    }

    fn scoped(self: Arc<Self>, namespace: Option<&str>) -> Arc<dyn DatabaseDriver> {
        let mut next = self.clone_handle();
        next.scope = namespace.map(str::to_string);
        Arc::new(next)
    }

    async fn open_cursor(&self, sql: &str, opts: QueryOptions) -> Result<Box<dyn QueryCursor>> {
        self.gate_cost(sql, opts.cost_confirmed).await?;
        let job = self.submit(sql, &[], &[]).await?;
        // Read the schema without stepping rows, honouring the trait's "cheap by
        // design" contract: `maxResults = 0` returns the schema and no data.
        let head = self.results(&job, None, None, 0).await?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = {
            let flag = Arc::clone(&cancelled);
            let inner = self.cancel_token(&job);
            CancelToken::new(move || {
                flag.store(true, Ordering::Relaxed);
                inner.cancel();
            })
        };
        Ok(Box::new(BqCursor {
            columns: head.columns,
            fields: head.fields,
            driver: self.clone_handle(),
            job,
            cancel,
            cancelled,
            full: opts.full_fidelity,
            window: opts.window,
            state: Mutex::new((None, false)),
        }))
    }

    async fn list_objects(&self) -> Result<Vec<SchemaMeta>> {
        catalog::list_objects(self).await
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetail> {
        catalog::describe_table(self, schema, table).await
    }

    async fn object_group_counts(&self) -> Result<Vec<(String, ObjectKind, usize)>> {
        catalog::object_group_counts(self).await
    }

    async fn list_object_group(
        &self,
        namespace: &str,
        kind: ObjectKind,
    ) -> Result<Vec<red_core::ObjectMeta>> {
        catalog::list_object_group(self, namespace, kind).await
    }

    async fn edit_caps(
        &self,
        schema: &str,
        table: &str,
        detail: &TableDetail,
    ) -> Result<RowEditCaps> {
        let _ = (schema, table, detail);
        // Overridden rather than inherited, because the inherited answer would be
        // wrong twice over: a BigQuery `PRIMARY KEY` is **not enforced**, so it is
        // not the unique row identity the guarded contract needs, and a point
        // `UPDATE` is a full-table scan besides (`docs/adr/0003`). Draft-row insert
        // is unaffected -- it is governed by `write_caps().insert`.
        Ok(RowEditCaps {
            note: Some(
                "BigQuery rows can't be edited in place: a declared PRIMARY KEY is not \
                 enforced here, so it can't address one row, and a point UPDATE scans the \
                 whole table. Inserting new rows is available."
                    .to_string(),
            ),
            ..RowEditCaps::default()
        })
    }

    async fn foreign_keys(&self) -> Result<Vec<FkEdge>> {
        catalog::foreign_keys(self).await
    }

    fn eq_predicate(&self, pairs: &[ColumnValue]) -> String {
        crate::eq_clause(pairs, bq_quote, true)
    }

    fn cmp_predicate(&self, preds: &[ColumnPredicate]) -> String {
        // GoogleSQL has no `ILIKE`, so the LIKE-shaped comparisons are
        // case-*sensitive* here where they are insensitive on MySQL/SQLite. The
        // alternative -- lowering both sides -- cannot be expressed through the
        // shared builder, which sees the column but not the value.
        crate::cmp_clause(
            preds,
            bq_quote,
            |c| format!("CAST({c} AS STRING)"),
            "LIKE",
            true,
            false,
        )
    }

    fn contains_predicate(&self, columns: &[ColumnMeta], term: &str) -> Option<String> {
        // Case-insensitivity by lowering *both* sides: the column via `as_text` and
        // the term before it becomes a literal. GoogleSQL's `LIKE` is
        // case-sensitive and it has no `ILIKE`, so a raw `LIKE` would silently miss
        // every differently-cased match.
        crate::contains_clause(
            columns,
            &term.to_lowercase(),
            bq_quote,
            |c| format!("LOWER(CAST({c} AS STRING))"),
            "LIKE",
            true,
            false,
        )
    }

    fn fk_join_wrap(&self, base: &str, base_cols: &[String], joins: &[FkJoin]) -> String {
        crate::join_wrap(base, base_cols, joins, bq_quote)
    }

    async fn count(&self, sql: &str, abort: &AbortSignal) -> Result<i64> {
        // If this result has already run, its own job knows how many rows it has:
        // `totalRows` on a finished job is exact, and reading it is a metadata call
        // that scans nothing. That makes the count *free* on the common path -- the
        // browse open probe counts the same SQL `fetch_page` is fetching -- and it
        // avoids depending on `count(*)` over a `SELECT *` subquery pruning to zero
        // columns, which is one of the premises in `docs/deferred.md`.
        if let Some(job) = self.cached_job(sql).await
            && let Ok(batch) = self.results(&job, None, None, 0).await
            && let Some(total) = batch.total_rows
        {
            return Ok(total.min(i64::MAX as u64) as i64);
        }
        let base = format!("SELECT count(*) FROM ({}) AS _red", strip_trailing(sql));
        let row = self.one_row(&base, abort).await?;
        Ok(match row.first() {
            Some(Value::Integer(i)) => *i,
            Some(Value::Text(t)) => t.parse().unwrap_or(0),
            _ => 0,
        })
    }

    async fn column_stats(
        &self,
        sql: &str,
        column: &str,
        flags: red_core::StatsFlags,
        abort: &AbortSignal,
    ) -> Result<red_core::ColumnStats> {
        let base = crate::stats_sql(sql, column, flags, bq_quote);
        let cells = self.one_row(&base, abort).await?;
        Ok(crate::parse_stats(&cells, flags))
    }

    async fn fetch_page(
        &self,
        sql: &str,
        offset: usize,
        limit: usize,
        cap: PageCap,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        // The reason this driver has a job cache: the page is read out of the
        // finished job by `startIndex`, so scrolling a result costs nothing beyond
        // the first query. Wrapping the SQL in `LIMIT/OFFSET` instead would make
        // every page a textually distinct query and therefore a fresh scan.
        let job = self.job_for(sql, abort).await?;
        let batch = self.results(&job, Some(offset as u64), None, limit).await?;
        let cap = CellCap::resolve(&cap, &batch.columns);
        Ok(ResultPage {
            rows: batch
                .rows
                .iter()
                .map(|r| read_row(r, &batch.fields, cap))
                .collect(),
            columns: batch.columns,
        })
    }

    async fn fetch_seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        scroll: red_core::SortDirection,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        // Implemented for real even though `DbKind::supports_keyset_paging` is false
        // for BigQuery, so the seam is honest and the conformance battery can check
        // it. Policy keeps it unused; correctness lives here.
        let page = self
            .seek(sql, key, bound, scroll, 0, limit, false, abort)
            .await?;
        Ok(page)
    }

    async fn fetch_seek_skip(
        &self,
        sql: &str,
        key: &KeySpec,
        from: Option<&[Value]>,
        skip: usize,
        limit: usize,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        self.seek(
            sql,
            key,
            from,
            red_core::SortDirection::Asc,
            skip,
            limit,
            true,
            abort,
        )
        .await
    }

    async fn key_bounds(
        &self,
        sql: &str,
        key: &KeySpec,
        abort: &AbortSignal,
    ) -> Result<Option<(i64, i64)>> {
        let _ = (sql, key, abort);
        // Contract-legal ("`None` when ... the key isn't an integer") and
        // deliberate: a MIN/MAX probe scans the key column, which on a large table
        // is tens of GB and real money -- for a fraction-jump optimisation that
        // BigQuery's offset paging does not need, since `fetch_page` is O(1) into a
        // finished job. The browse open probe therefore never pays for it.
        Ok(None)
    }

    async fn execute_abort(&self, sql: &str, abort: &AbortSignal) -> Result<u64> {
        refuse_if_read_only(self.read_only)?;
        let job = self.submit(sql, &[], &[]).await?;
        let token = self.cancel_token(&job);
        let _guard = abort.arm(token);
        let batch = self.results(&job, None, None, 0).await?;
        Ok(batch.affected.unwrap_or(0))
    }

    async fn execute_batch_abort(
        &self,
        statements: &[String],
        abort: &AbortSignal,
    ) -> Result<Vec<u64>> {
        refuse_if_read_only(self.read_only)?;
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        // **Sequential, not atomic** -- the escape hatch the trait doc names ("an
        // engine without multi-statement transactions runs them sequentially; it
        // documents that"), taken here for a different reason. BigQuery *does* have
        // `BEGIN TRANSACTION`, but a multi-statement script reports one
        // `numDmlAffectedRows` for the whole job, so the atomic shape could not
        // return the per-statement counts this method promises without fabricating
        // a split. Per-statement jobs inside a BigQuery *session* would give both;
        // that is deferred with `begin_sandbox`.
        let mut affected = Vec::with_capacity(statements.len());
        for statement in statements {
            affected.push(self.execute_abort(statement, abort).await?);
        }
        Ok(affected)
    }

    async fn apply_edits(&self, ops: &[EditOp]) -> Result<u64> {
        write::apply_edits(self, ops).await
    }

    async fn insert_rows(
        &self,
        table: &TableRef,
        columns: &[Column],
        rows: &[Vec<Value>],
    ) -> Result<u64> {
        write::insert_rows(self, table, columns, rows).await
    }

    async fn clear_table(&self, table: &TableRef) -> Result<u64> {
        write::clear_table(self, table).await
    }

    async fn create_table(&self, table: &TableRef, columns: &[ColumnMeta]) -> Result<u64> {
        refuse_if_read_only(self.read_only)?;
        let sql = red_core::ddl::create_table_sql(
            &self.qualified(table),
            columns,
            DbKind::Bigquery,
            bq_quote,
        );
        self.execute(&sql).await
    }

    async fn drop_table(&self, table: &TableRef) -> Result<u64> {
        refuse_if_read_only(self.read_only)?;
        let sql = red_core::ddl::drop_table_sql(&self.qualified(table), bq_quote);
        self.execute(&sql).await
    }

    async fn create_namespace(&self, name: &str) -> Result<()> {
        refuse_if_read_only(self.read_only)?;
        self.execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {}.{}",
            bq_quote(&self.project),
            bq_quote(name)
        ))
        .await
        .map(|_| ())
    }

    fn quote_table(&self, table: &TableRef) -> String {
        // Where a pinned project becomes part of every name (`docs/adr/0001`). A
        // `TableRef` carries only two levels, so the third is supplied here; an
        // unqualified ref binds to this handle's namespace rather than being left
        // to BigQuery's (absent) default.
        let dataset = table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.namespace());
        if dataset.is_empty() {
            return format!("{}.{}", bq_quote(&self.project), bq_quote(&table.name));
        }
        format!(
            "{}.{}.{}",
            bq_quote(&self.project),
            bq_quote(dataset),
            bq_quote(&table.name)
        )
    }

    fn quote_ident(&self, ident: &str) -> String {
        bq_quote(ident)
    }

    async fn create_index(
        &self,
        table: &TableRef,
        name: &str,
        unique: bool,
        columns: &[String],
    ) -> Result<u64> {
        let _ = (table, name, unique, columns);
        // BigQuery has only `CREATE SEARCH INDEX`, which is a text-search structure
        // rather than a secondary index: there is nothing faithful to emit. The
        // migrate job treats this error as a logged skip.
        Err(RedError::Driver(
            "BigQuery has no secondary indexes (only search indexes)".to_string(),
        ))
    }

    async fn add_foreign_key(
        &self,
        child: &TableRef,
        columns: &[String],
        parent: &TableRef,
        ref_columns: &[String],
    ) -> Result<u64> {
        let _ = (child, columns, parent, ref_columns);
        // BigQuery accepts `ADD CONSTRAINT … NOT ENFORCED`, which would record a
        // relationship it never checks. Declining is more honest than writing
        // metadata that reads as a guarantee; the migrate job logs the skip.
        Err(RedError::Driver(
            "BigQuery foreign keys are never enforced, so the migration skips them".to_string(),
        ))
    }

    async fn object_ddl(&self, namespace: &str, name: &str, kind: ObjectKind) -> Result<String> {
        catalog::object_ddl(self, namespace, name, kind).await
    }

    async fn precheck_cost(&self, sql: &str, confirmed: bool) -> Result<()> {
        self.gate_cost(sql, confirmed).await
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<QueryPlan> {
        if !analyze {
            // A dry run: BigQuery has no textual `EXPLAIN`, and this honours the
            // trait's promise that plain `explain` never executes the statement --
            // while also answering the question that matters most here, which is
            // what the query will cost.
            let bytes = self.dry_run(sql).await?;
            return Ok(QueryPlan {
                nodes: vec![red_core::PlanNode {
                    label: format!("Dry run: {} scanned", human_bytes(bytes)),
                    detail: Some(
                        "BigQuery prices a query by bytes read; no plan stages exist until \
                         it runs."
                            .to_string(),
                    ),
                    metrics: vec![("bytes billed".to_string(), bytes.to_string())],
                    children: Vec::new(),
                }],
                raw: format!("dryRun: totalBytesProcessed = {bytes}"),
                analyzed: false,
            });
        }
        catalog::explain_analyze(self, sql).await
    }

    async fn export(
        &self,
        sql: &str,
        path: &Path,
        format: ExportFormat,
        cancel: Arc<AtomicBool>,
        progress: UnboundedSender<u64>,
    ) -> Result<ExportOutcome> {
        let job = self.submit(sql, &[], &[]).await?;
        let head = self.results(&job, None, None, 0).await?;
        let names: Vec<String> = head.columns.iter().map(|c| c.name.clone()).collect();
        let file = std::fs::File::create(path).map_err(driver_err)?;
        let mut writer = ExportWriter::begin(
            std::io::BufWriter::new(file),
            format,
            names,
            path,
            DbKind::Bigquery,
        )
        .map_err(driver_err)?;
        let mut throttle = ProgressThrottle::new(progress);

        let mut token: Option<String> = None;
        let mut first = true;
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(writer);
                let _ = std::fs::remove_file(path);
                self.cancel_job(&job).await;
                return Err(RedError::Interrupted);
            }
            if !first && token.is_none() {
                break;
            }
            first = false;
            let batch = self
                .results(&job, None, token.as_deref(), EXPORT_PAGE_ROWS)
                .await?;
            for row in &batch.rows {
                if cancel.load(Ordering::Relaxed) {
                    drop(writer);
                    let _ = std::fs::remove_file(path);
                    self.cancel_job(&job).await;
                    return Err(RedError::Interrupted);
                }
                // Full fidelity: an export is never display-capped.
                writer
                    .write_row(&read_row(row, &head.fields, None))
                    .map_err(driver_err)?;
                throttle.tick(writer.written());
            }
            token = batch.page_token;
        }
        writer.finish().map_err(driver_err)
    }

    async fn server_metrics(&self) -> Result<red_core::server::ServerSnapshot> {
        catalog::server_metrics(self).await
    }

    async fn server_sessions(&self) -> Result<(Vec<red_core::ServerSession>, bool)> {
        catalog::server_sessions(self).await
    }

    async fn kill_session(
        &self,
        key: &red_core::SessionKey,
        mode: red_core::KillMode,
    ) -> Result<()> {
        refuse_if_read_only(self.read_only)?;
        let _ = mode;
        // There is no session to terminate apart from its job (one request is one
        // job), which is why `session_caps().can_terminate` is false; both modes
        // land on the same cancel.
        self.cancel_job(&JobRef {
            id: key.0.clone(),
            location: self.known_location().await,
        })
        .await;
        Ok(())
    }
}

/// Rows per page while exporting. Larger than a display page (nobody is looking at
/// it) but still bounded, so a huge export never materialises whole.
const EXPORT_PAGE_ROWS: usize = 10_000;

impl BigqueryDriver {
    /// A [`TableRef`] with its dataset filled in from this handle's namespace, so
    /// the shared DDL builders (which know nothing of a third level) still produce
    /// a fully-qualified name via [`bq_quote`].
    fn qualified(&self, table: &TableRef) -> TableRef {
        TableRef {
            schema: Some(match table.schema.as_deref() {
                Some(s) if !s.is_empty() => format!("{}.{}", self.project, s),
                _ => format!("{}.{}", self.project, self.namespace()),
            }),
            name: table.name.clone(),
        }
    }

    /// The shared body of both seek paths.
    #[allow(clippy::too_many_arguments)]
    async fn seek(
        &self,
        sql: &str,
        key: &KeySpec,
        bound: Option<&[Value]>,
        scroll: red_core::SortDirection,
        skip: usize,
        limit: usize,
        inclusive: bool,
        abort: &AbortSignal,
    ) -> Result<ResultPage> {
        let bound_len = bound.map(<[Value]>::len).unwrap_or(0);
        let (where_clause, order_by) =
            crate::seek_clauses(key, bound_len, scroll, inclusive, bq_quote, |_| {
                "?".to_string()
            });
        let offset = if skip > 0 {
            format!(" OFFSET {skip}")
        } else {
            String::new()
        };
        let seek_sql = format!(
            "SELECT * FROM ({}) AS _red {where_clause}ORDER BY {order_by} LIMIT {limit}{offset}",
            strip_trailing(sql)
        );
        let params: Vec<&Value> = bound.map(<[Value]>::iter).into_iter().flatten().collect();
        // A seek binds values, so it cannot ride the job cache (which is keyed by
        // SQL text alone) and always submits. That is the cost this engine's paging
        // policy exists to avoid.
        let job = self.submit(&seek_sql, &params, &[]).await?;
        let token = self.cancel_token(&job);
        let _guard = abort.arm(token);
        let batch = self.results(&job, None, None, limit).await?;
        let cap = CellCap::display(crate::key_positions(key, &batch.columns));
        Ok(ResultPage {
            rows: batch
                .rows
                .iter()
                .map(|r| read_row(r, &batch.fields, cap))
                .collect(),
            columns: batch.columns,
        })
    }

    /// Run a catalog query and hand back its rows, for the `catalog` module.
    async fn catalog_rows(&self, sql: &str) -> Result<ResultPage> {
        self.run_collect(sql, CATALOG_PAGE_ROWS, &AbortSignal::new(), None)
            .await
    }
}

/// Cap on rows one catalog read returns. `INFORMATION_SCHEMA` reads are bounded by
/// contract (`list_objects` is "names + kinds only"), and a project with more
/// objects than this has a tree nobody scrolls anyway.
const CATALOG_PAGE_ROWS: usize = 10_000;
