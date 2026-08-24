//! The always-run half of the BigQuery test strategy: everything that can be
//! checked without credentials, a project, or a billed query.
//!
//! Two kinds of test live here. Most are pure — quoting, error classification, the
//! job cache's expiry rule — and need nothing but a constructed driver. The rest
//! drive the real HTTP paths against a loopback server replaying canned REST
//! responses, which is what lets `fetch_page`'s `startIndex` arithmetic and the
//! cost gate be checked at all.
//!
//! **What these tests cannot tell you** is whether BigQuery behaves as this driver
//! assumes, because the fixtures encode the same assumptions the code does and so
//! agree with it by construction. That is the job of the env-gated live suite, and
//! the premises it must check are listed in `docs/deferred.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use red_core::{RedError, TableRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::*;

/// A driver pointed at `base_url` with a token already in hand.
fn driver_at(base_url: &str, dataset: &str) -> BigqueryDriver {
    let client = reqwest::Client::new();
    BigqueryDriver {
        client: client.clone(),
        base_url: base_url.to_string(),
        project: "my-project".to_string(),
        dataset: dataset.to_string(),
        scope: None,
        read_only: false,
        tokens: Arc::new(TokenSource::preloaded("test-token", client)),
        location: Arc::new(Mutex::new(Some("US".to_string()))),
        jobs: Arc::new(Mutex::new(Vec::new())),
    }
}

/// A loopback server that answers every request with the next canned response,
/// recording the request lines it saw so a test can assert on the URL.
struct MockApi {
    base_url: String,
    seen: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicUsize>,
}

impl MockApi {
    /// Serve `responses` in order, repeating the last one once exhausted (so a test
    /// need only specify as many distinct replies as it cares about).
    async fn start(responses: Vec<String>) -> MockApi {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (task_seen, task_hits) = (Arc::clone(&seen), Arc::clone(&hits));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (seen, hits) = (Arc::clone(&task_seen), Arc::clone(&task_hits));
                let responses = responses.clone();
                tokio::spawn(async move {
                    // One request per connection is enough: reqwest will open
                    // another, and reading to the end of the headers is all the
                    // assertions need (the request line carries the query string).
                    let mut buf = vec![0u8; 16 * 1024];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = request.lines().next().unwrap_or("").to_string();
                    seen.lock().await.push(line);
                    let i = hits.fetch_add(1, Ordering::SeqCst);
                    let body = responses
                        .get(i)
                        .or_else(|| responses.last())
                        .cloned()
                        .unwrap_or_else(|| "{}".to_string());
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        MockApi {
            base_url: format!("http://127.0.0.1:{port}"),
            seen,
            hits,
        }
    }

    async fn requests(&self) -> Vec<String> {
        self.seen.lock().await.clone()
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// A `jobs.insert` reply.
fn job_response(id: &str) -> String {
    format!(
        r#"{{"jobReference":{{"projectId":"my-project","jobId":"{id}","location":"US"}},
            "status":{{"state":"DONE"}}}}"#
    )
}

/// A `getQueryResults` reply with one INT64 column.
fn results_response(values: &[i64], total: u64) -> String {
    let rows: Vec<String> = values
        .iter()
        .map(|v| format!(r#"{{"f":[{{"v":"{v}"}}]}}"#))
        .collect();
    format!(
        r#"{{"jobComplete":true,
            "schema":{{"fields":[{{"name":"n","type":"INTEGER"}}]}},
            "totalRows":"{total}",
            "rows":[{}]}}"#,
        rows.join(",")
    )
}

// ---------------------------------------------------------------- pure tests

#[test]
fn identifiers_are_backtick_quoted_and_backticks_stripped() {
    assert_eq!(bq_quote("users"), "`users`");
    // BigQuery provides no escape for a backtick inside an identifier, so such a
    // name cannot exist; stripping closes the injection route without being lossy
    // for anything real.
    assert_eq!(bq_quote("we`ird"), "`weird`");
}

#[test]
fn quote_table_injects_the_pinned_project() {
    // The load-bearing consequence of ADR-0001: `TableRef` carries two levels and
    // BigQuery needs three, so the project is supplied here or the name is wrong.
    let driver = driver_at("http://unused", "analytics");
    assert_eq!(
        driver.quote_table(&TableRef {
            schema: Some("warehouse".to_string()),
            name: "events".to_string(),
        }),
        "`my-project`.`warehouse`.`events`"
    );
}

#[test]
fn an_unqualified_table_binds_to_the_handles_namespace() {
    let driver = driver_at("http://unused", "analytics");
    assert_eq!(
        driver.quote_table(&TableRef {
            schema: None,
            name: "events".to_string(),
        }),
        "`my-project`.`analytics`.`events`"
    );
}

#[test]
fn http_401_and_403_are_auth_failures_so_the_ui_stops_retrying() {
    let body = r#"{"error":{"code":403,"message":"Access Denied: Table x"}}"#;
    assert!(matches!(api_error(403, body), RedError::Auth(m) if m.contains("Access Denied")));
    assert!(matches!(api_error(401, body), RedError::Auth(_)));
    assert!(matches!(api_error(400, body), RedError::Query(_)));
}

#[test]
fn a_job_level_error_surfaces_instead_of_reading_as_an_empty_result() {
    // The regression this guards: BigQuery reports a *failed query* inside a 200
    // response, so missing it would show a failure as zero rows.
    let json: Json = serde_json::from_str(
        r#"{"status":{"errorResult":{"reason":"invalidQuery","message":"Syntax error"}}}"#,
    )
    .expect("fixture");
    assert!(matches!(job_error(&json), Err(RedError::Query(m)) if m.contains("Syntax error")));
}

#[test]
fn a_stopped_job_is_an_interruption_not_a_failure() {
    let json: Json =
        serde_json::from_str(r#"{"status":{"errorResult":{"reason":"stopped","message":"x"}}}"#)
            .expect("fixture");
    assert!(matches!(job_error(&json), Err(RedError::Interrupted)));
}

#[test]
fn exceeding_the_billed_cap_names_the_cap_in_the_message() {
    let json: Json = serde_json::from_str(
        r#"{"status":{"errorResult":{"reason":"bytesBilledLimitExceeded","message":"too big"}}}"#,
    )
    .expect("fixture");
    match job_error(&json) {
        Err(RedError::Query(m)) => {
            assert!(
                m.contains("max_bytes_billed"),
                "should name the setting: {m}"
            );
        }
        other => panic!("expected a Query error, got {other:?}"),
    }
}

#[test]
fn a_clean_job_status_is_not_an_error() {
    let json: Json = serde_json::from_str(r#"{"status":{"state":"DONE"}}"#).expect("fixture");
    assert!(job_error(&json).is_ok());
}

#[tokio::test]
async fn the_job_cache_is_scoped_by_namespace_not_by_sql_alone() {
    // Keying on SQL alone would serve one dataset's rows for another dataset's
    // browse, since the same text means a different query under a different
    // `defaultDataset`.
    let a = driver_at("http://unused", "one");
    let mut b = a.clone_handle();
    b.scope = Some("two".to_string());
    assert_ne!(a.cache_key("SELECT 1"), b.cache_key("SELECT 1"));
}

#[tokio::test]
async fn an_expired_job_is_not_reused() {
    let driver = driver_at("http://unused", "ds");
    let stale = Instant::now() - JOB_CACHE_TTL - Duration::from_secs(1);
    driver.jobs.lock().await.push((
        driver.cache_key("SELECT 1"),
        JobRef {
            id: "old".to_string(),
            location: None,
        },
        stale,
    ));
    // The TTL is a correctness property (ADR-0002): a refresh re-opens with
    // identical SQL, so a stale hit would show minute-old rows as current.
    assert!(driver.cached_job("SELECT 1").await.is_none());
}

#[tokio::test]
async fn a_fresh_job_is_reused() {
    let driver = driver_at("http://unused", "ds");
    driver
        .remember_job(
            "SELECT 1",
            &JobRef {
                id: "fresh".to_string(),
                location: None,
            },
        )
        .await;
    assert_eq!(
        driver.cached_job("SELECT 1").await.map(|j| j.id),
        Some("fresh".to_string())
    );
}

#[tokio::test]
async fn the_job_cache_is_bounded() {
    let driver = driver_at("http://unused", "ds");
    for i in 0..(JOB_CACHE_MAX + 10) {
        driver
            .remember_job(
                &format!("SELECT {i}"),
                &JobRef {
                    id: i.to_string(),
                    location: None,
                },
            )
            .await;
    }
    assert!(driver.jobs.lock().await.len() <= JOB_CACHE_MAX);
}

// --------------------------------------------------------- server-backed tests

#[tokio::test]
async fn a_page_fetch_reads_the_cached_job_by_start_index() {
    let mock = MockApi::start(vec![
        job_response("job1"),
        results_response(&[10, 11], 500),
        results_response(&[12, 13], 500),
    ])
    .await;
    let driver = driver_at(&mock.base_url, "ds");
    let abort = AbortSignal::new();

    let first = driver
        .fetch_page("SELECT n FROM t", 0, 2, PageCap::Full, &abort)
        .await
        .expect("first page");
    assert_eq!(first.rows.len(), 2);

    // The second page must not submit a second job: this is the whole point of
    // ADR-0002, and getting it wrong means every scroll is a fresh billed scan.
    let before = mock.hit_count();
    let second = driver
        .fetch_page("SELECT n FROM t", 100, 2, PageCap::Full, &abort)
        .await
        .expect("second page");
    assert_eq!(second.rows.len(), 2);
    assert_eq!(
        mock.hit_count() - before,
        1,
        "a cached result should cost one results read, not a re-submit"
    );

    let requests = mock.requests().await;
    assert!(
        requests.iter().any(|r| r.contains("startIndex=100")),
        "the offset must ride as startIndex: {requests:?}"
    );
    assert!(
        requests.iter().filter(|r| r.contains("POST")).count() == 1,
        "exactly one job submitted: {requests:?}"
    );
}

#[tokio::test]
async fn count_reads_total_rows_off_a_cached_job_without_a_second_query() {
    let mock = MockApi::start(vec![
        job_response("job1"),
        results_response(&[1], 4_242),
        results_response(&[], 4_242),
    ])
    .await;
    let driver = driver_at(&mock.base_url, "ds");
    let abort = AbortSignal::new();
    driver
        .fetch_page("SELECT n FROM t", 0, 1, PageCap::Full, &abort)
        .await
        .expect("page");

    let total = driver
        .count("SELECT n FROM t", &abort)
        .await
        .expect("count");
    assert_eq!(total, 4_242);
    // One POST for the original job and none for the count: a finished job already
    // knows its own row count.
    assert_eq!(
        mock.requests()
            .await
            .iter()
            .filter(|r| r.contains("POST"))
            .count(),
        1
    );
}

#[tokio::test]
async fn key_bounds_never_probes_the_engine() {
    // Contract-legal `None`, and deliberate: a MIN/MAX probe scans the key column,
    // which is real money on a large table, for a fraction-jump this engine's
    // offset paging does not need.
    let mock = MockApi::start(vec!["{}".to_string()]).await;
    let driver = driver_at(&mock.base_url, "ds");
    let key = red_core::KeySpec::single("id", red_core::KeyKind::Int);
    let bounds = driver
        .key_bounds("SELECT * FROM t", &key, &AbortSignal::new())
        .await
        .expect("key_bounds");
    assert!(bounds.is_none());
    assert_eq!(mock.hit_count(), 0, "it must not talk to BigQuery at all");
}

#[tokio::test]
async fn a_query_over_the_confirm_threshold_is_not_run() {
    // A dry run reports 40 GiB, which is over the 20 GiB confirm default and under
    // the 100 GiB hard cap: the query must come back as a decision, not a failure,
    // and must not have executed.
    let bytes = 40u64 * 1024 * 1024 * 1024;
    let mock = MockApi::start(vec![format!(
        r#"{{"statistics":{{"totalBytesProcessed":"{bytes}"}}}}"#
    )])
    .await;
    let driver = driver_at(&mock.base_url, "ds");
    match driver
        .open_cursor("SELECT * FROM huge", QueryOptions::default())
        .await
    {
        Err(RedError::QueryCostConfirm { bytes: b, limit }) => {
            assert_eq!(b, bytes);
            assert_eq!(limit, DEFAULT_COST_CONFIRM_BYTES);
        }
        Err(other) => panic!("expected a cost confirmation, got {other:?}"),
        Ok(_) => panic!("the query must not have run"),
    }
    assert_eq!(
        mock.hit_count(),
        1,
        "only the dry run should have been sent"
    );
}

#[tokio::test]
async fn a_query_over_the_hard_cap_is_refused_outright() {
    // Over the hard cap it is not the user's to wave through from a dialog, so it
    // is a plain failure naming both numbers.
    let bytes = 500u64 * 1024 * 1024 * 1024;
    let mock = MockApi::start(vec![format!(
        r#"{{"statistics":{{"totalBytesProcessed":"{bytes}"}}}}"#
    )])
    .await;
    let driver = driver_at(&mock.base_url, "ds");
    match driver
        .open_cursor("SELECT * FROM huge", QueryOptions::default())
        .await
    {
        Err(RedError::Query(m)) => {
            assert!(m.contains("max_bytes_billed"), "should be correctable: {m}");
        }
        Err(other) => panic!("expected an outright refusal, got {other:?}"),
        Ok(_) => panic!("the query must not have run"),
    }
}

#[tokio::test]
async fn an_approved_query_skips_the_gate_entirely() {
    let mock = MockApi::start(vec![job_response("job1"), results_response(&[1], 1)]).await;
    let driver = driver_at(&mock.base_url, "ds");
    let opts = QueryOptions {
        cost_confirmed: true,
        ..QueryOptions::default()
    };
    driver
        .open_cursor("SELECT * FROM huge", opts)
        .await
        .expect("an approved query runs");
    // No dry run: the first request is the job submission itself. Re-pricing an
    // approved query would spend a round trip to ask a question already answered.
    let requests = mock.requests().await;
    assert!(
        requests.first().is_some_and(|r| r.contains("POST")),
        "the first call should be the job, not a dry run: {requests:?}"
    );
}

#[tokio::test]
async fn a_cursor_walks_page_tokens_to_exhaustion() {
    let paged = r#"{"jobComplete":true,
        "schema":{"fields":[{"name":"n","type":"INTEGER"}]},
        "rows":[{"f":[{"v":"1"}]}],"pageToken":"next"}"#;
    let mock = MockApi::start(vec![
        job_response("job1"),
        // The schema read at open (maxResults=0).
        results_response(&[], 2),
        paged.to_string(),
        results_response(&[2], 2),
    ])
    .await;
    let driver = driver_at(&mock.base_url, "ds");
    let opts = QueryOptions {
        cost_confirmed: true,
        ..QueryOptions::default()
    };
    let cursor = driver
        .open_cursor("SELECT n FROM t", opts)
        .await
        .expect("cursor");
    assert_eq!(cursor.columns().len(), 1, "columns known without stepping");

    let first = cursor.next_window(10).await.expect("first window");
    assert_eq!(first.rows.len(), 1);
    assert!(!first.exhausted, "a page token means more rows");

    let second = cursor.next_window(10).await.expect("second window");
    assert!(second.exhausted, "no token means the result ended");
}

#[tokio::test]
async fn in_grid_updates_are_refused_with_a_reason() {
    let driver = driver_at("http://unused", "ds");
    let op = red_core::EditOp::Delete {
        table: TableRef {
            schema: None,
            name: "t".to_string(),
        },
        keys: vec![red_core::ColumnValue {
            column: "id".to_string(),
            value: Value::Integer(1),
            decl_type: None,
        }],
    };
    match driver.apply_edits(&[op]).await {
        Err(RedError::Driver(m)) => {
            assert!(m.contains("not enforced"), "should explain why: {m}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn edit_caps_report_not_editable_with_a_reason() {
    let driver = driver_at("http://unused", "ds");
    let caps = driver
        .edit_caps("ds", "t", &red_core::TableDetail::default())
        .await
        .expect("caps");
    assert_eq!(caps.mode, red_core::EditMode::None);
    assert!(caps.identity.is_empty());
    assert!(caps.note.is_some_and(|n| n.contains("Inserting")));
}

#[tokio::test]
async fn a_read_only_connection_refuses_writes_in_the_driver() {
    // BigQuery has no `readonly=1` server setting like ClickHouse's, so the driver
    // itself is the gate; the IAM role cannot be assumed to be restricted.
    let mut driver = driver_at("http://unused", "ds");
    driver.read_only = true;
    let table = TableRef {
        schema: None,
        name: "t".to_string(),
    };
    assert!(driver.clear_table(&table).await.is_err());
    assert!(driver.insert_rows(&table, &[], &[vec![]]).await.is_err());
}

#[tokio::test]
async fn plain_explain_is_a_dry_run_that_never_executes_the_statement() {
    // The trait promises `explain(analyze: false)` does not run the query. On
    // BigQuery that promise *and* the cost answer come from the same `dryRun`, so
    // this checks both: one request, and the byte figure surfaced in the plan.
    let bytes = 7u64 * 1024 * 1024 * 1024;
    let mock = MockApi::start(vec![format!(
        r#"{{"statistics":{{"totalBytesProcessed":"{bytes}"}}}}"#
    )])
    .await;
    let driver = driver_at(&mock.base_url, "ds");
    let plan = driver
        .explain("SELECT * FROM t", false)
        .await
        .expect("plan");
    assert!(!plan.analyzed, "a dry run has no actuals");
    assert!(
        plan.raw.contains(&bytes.to_string()),
        "the plan must carry the byte count: {}",
        plan.raw
    );
    assert!(
        plan.nodes[0].label.contains("7.0 GiB"),
        "and read it as a human byte count: {}",
        plan.nodes[0].label
    );
    assert_eq!(
        mock.hit_count(),
        1,
        "exactly one call, and no job submitted"
    );
}
