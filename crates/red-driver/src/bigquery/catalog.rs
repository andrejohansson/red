//! BigQuery introspection, the Server panel, and `EXPLAIN ANALYZE`.
//!
//! The structural thing to know here is that BigQuery exposes its metadata **two
//! ways**, and which one a call uses is a question about *privileges*, not speed:
//!
//! - **REST metadata endpoints** (`datasets.list`, `tables.list`, `tables.get`,
//!   `routines.list`) need only per-dataset permissions and create no query job.
//!   The schema tree and [`describe_table`] use these.
//! - **`INFORMATION_SCHEMA` views**, at either dataset or region scope. The region
//!   scope answers the whole project in one query, which made it the obvious first
//!   choice for [`list_objects`] — and it was wrong: querying it needs
//!   `bigquery.tables.list` and `bigquery.routines.list` across the entire project,
//!   so a role granted a few specific datasets (the ordinary shape of BigQuery
//!   access) is refused, and the tree would not load at all for a database it can
//!   query perfectly. The views are now used only where nothing else answers: the
//!   declared primary key, an object's `ddl` text, and the job history behind the
//!   Server panel.
//!
//! Catalog reads bill no bytes either way, so this is the one part of the driver
//! that costs nothing to repeat. The `INFORMATION_SCHEMA` ones still cost a *job*,
//! which is why the REST path is preferred wherever it can answer, and why every
//! view-backed read here degrades rather than failing its caller.

use red_core::server::{MetricGroup, MetricValue, ServerMetric, ServerSnapshot};
use red_core::{
    ColumnMeta, DbKind, FkEdge, ObjectKind, ObjectMeta, PlanNode, QueryPlan, RedError, Result,
    SchemaMeta, ServerSession, SessionKey, TableDetail, Value,
};
use serde_json::Value as Json;

use super::{BigqueryDriver, bq_quote, json_u64};
use crate::now_unix;

/// The default region to read `INFORMATION_SCHEMA` from when the connection's own
/// location is not known yet (no dataset bound, and no job run). `us` is
/// BigQuery's own default for a dataset created without a location.
const DEFAULT_REGION: &str = "us";

/// The `region-<loc>` qualifier for a project-wide `INFORMATION_SCHEMA` read.
async fn region(driver: &BigqueryDriver) -> String {
    let loc = driver
        .known_location()
        .await
        .unwrap_or_else(|| DEFAULT_REGION.to_string());
    bq_quote(&format!("region-{}", loc.to_ascii_lowercase()))
}

/// A cell as plain text, for the catalog reads (every `INFORMATION_SCHEMA` column
/// RED reads is a string).
fn text(row: &[Value], i: usize) -> String {
    match row.get(i) {
        Some(Value::Text(t)) => t.to_string(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn number(row: &[Value], i: usize) -> u64 {
    match row.get(i) {
        Some(Value::Integer(n)) => (*n).max(0) as u64,
        Some(Value::Real(f)) => f.max(0.0) as u64,
        Some(Value::Text(t)) => t.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The schema-tree skeleton: the datasets this connection can see, each with its
/// relations.
///
/// **Read over REST (`datasets.list` + `tables.list`), not over
/// `INFORMATION_SCHEMA`**, and that choice is about *permissions* rather than
/// speed. The region-scoped `` `region-x`.INFORMATION_SCHEMA.TABLES `` view answers
/// the whole project in one query, which is why it was the obvious first choice —
/// but querying it requires `bigquery.tables.list` and `bigquery.routines.list`
/// across the project, so a role granted access to a few specific datasets is
/// refused outright:
///
/// ```text
/// Access Denied: Table p:region-europe-north1.INFORMATION_SCHEMA.TABLES:
/// User does not have the required permissions (…) to query system entity
/// ```
///
/// Dataset-scoped grants are the normal shape of BigQuery access, so a schema tree
/// that needs project-wide privileges is a schema tree most users cannot load. The
/// REST endpoints need only `bigquery.tables.list` **on each dataset**, which is
/// exactly what the console and every other client use — and they create no query
/// job at all, where an `INFORMATION_SCHEMA` read does.
///
/// The cost is one round trip per dataset instead of one query. They are metadata
/// calls, not scans, and they run concurrently, so this stays inside
/// `list_objects`' cheapness contract: names and kinds only, nothing per-table.
pub(super) async fn list_objects(driver: &BigqueryDriver) -> Result<Vec<SchemaMeta>> {
    let names = dataset_names(driver).await?;
    // Concurrent rather than sequential: the calls are independent and a project
    // with thirty datasets should not take thirty round trips end to end.
    let listings = futures_util::future::join_all(
        names
            .iter()
            .map(|name| async move { (name.clone(), dataset_tables(driver, name).await) }),
    )
    .await;
    Ok(listings
        .into_iter()
        .map(|(name, objects)| SchemaMeta {
            name,
            // A dataset whose tables could not be listed still draws, empty. It is a
            // dataset the user really has (it came back from `datasets.list`), and
            // dropping it would hide the thing they need to fix the grant on.
            objects: objects.unwrap_or_default(),
        })
        .collect())
}

/// The datasets to draw: everything `datasets.list` reports, plus the connection's
/// own bound dataset.
///
/// The union matters at both ends. A role with no project-level grant may be unable
/// to *enumerate* datasets while still reading the one it was given, so a failed or
/// empty listing falls back to the bound dataset rather than yielding an empty tree.
/// And a bound dataset missing from the listing is still browsable, so it is added
/// rather than hidden.
async fn dataset_names(driver: &BigqueryDriver) -> Result<Vec<String>> {
    let bound = driver.namespace().to_string();
    let listed = driver
        .list_items(&format!("projects/{}/datasets", driver.project), "datasets")
        .await;
    let mut names: Vec<String> = match listed {
        Ok(items) => items
            .iter()
            .filter_map(|d| {
                d.pointer("/datasetReference/datasetId")
                    .and_then(Json::as_str)
                    .map(str::to_string)
            })
            .collect(),
        Err(e) => {
            // Not fatal, and worth a line: this is the "granted one dataset, not the
            // project" case, where the tree is legitimately just that dataset.
            tracing::info!("bigquery: cannot list datasets ({e}); showing the bound one only");
            Vec::new()
        }
    };
    if !bound.is_empty() && !names.contains(&bound) {
        names.push(bound);
    }
    names.sort();
    if names.is_empty() {
        return Err(RedError::Query(
            "no datasets are visible to this connection: bind one on the connection, \
             or grant the account access to a dataset in this project"
                .to_string(),
        ));
    }
    Ok(names)
}

/// One dataset's relations, by name and kind.
async fn dataset_tables(driver: &BigqueryDriver, dataset: &str) -> Result<Vec<ObjectMeta>> {
    let path = format!("projects/{}/datasets/{dataset}/tables", driver.project);
    Ok(driver
        .list_items(&path, "tables")
        .await?
        .iter()
        .filter_map(|t| {
            let name = t
                .pointer("/tableReference/tableId")
                .and_then(Json::as_str)?
                .to_string();
            let kind = table_kind(t.get("type").and_then(Json::as_str).unwrap_or(""));
            Some(ObjectMeta { name, kind })
        })
        .collect())
}

/// Map a table's reported type onto RED's kinds.
///
/// Covers both spellings, because the two sources disagree: REST reports `TABLE`,
/// `VIEW`, `MATERIALIZED_VIEW`, `EXTERNAL`, `SNAPSHOT`; `INFORMATION_SCHEMA` says
/// `BASE TABLE` and `MATERIALIZED VIEW` (space, not underscore). Everything that is
/// not a view is drawn as a table, since all of them are things you can select
/// from and RED has no narrower kind for a snapshot or an external table.
fn table_kind(table_type: &str) -> ObjectKind {
    match table_type.to_ascii_uppercase().as_str() {
        "VIEW" => ObjectKind::View,
        "MATERIALIZED_VIEW" | "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
        _ => ObjectKind::Table,
    }
}

/// One table's columns and primary key.
///
/// The primary key is reported even though BigQuery **does not enforce it**: it is
/// true schema information and belongs in the tree. What it must not do is drive
/// row identity or keyset paging, which is why `DbKind::supports_keyset_paging` is
/// false for this engine and `edit_caps` is overridden — see `docs/adr/0003`.
pub(super) async fn describe_table(
    driver: &BigqueryDriver,
    schema: &str,
    table: &str,
) -> Result<TableDetail> {
    let dataset = if schema.is_empty() {
        driver.namespace().to_string()
    } else {
        schema.to_string()
    };
    // `tables.get` over REST, for the same permission reason `list_objects` uses
    // `tables.list`: it needs only `bigquery.tables.get` on this one table, where an
    // `INFORMATION_SCHEMA.COLUMNS` read needs the dataset-wide view privilege *and*
    // spends a query job. It also carries more than the view does — the `mode` on
    // every field, and the nested `fields` of a `RECORD`.
    let path = format!(
        "projects/{}/datasets/{dataset}/tables/{table}",
        driver.project
    );
    let json = driver.get(&path, &[]).await?;
    let fields = json
        .pointer("/schema/fields")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    // The declared primary key, which BigQuery reports only through
    // `INFORMATION_SCHEMA` (there is no REST field for it). Best-effort and
    // deliberately non-fatal: these views are newer than the rest, they cost a query
    // job, and a role may not read them. A table with no reported key still browses;
    // it just carries no key badge. It also drives nothing here — see
    // `DbKind::supports_keyset_paging` and `edit_caps`.
    let key_names = primary_key(driver, &dataset, table).await;

    Ok(TableDetail {
        columns: fields
            .iter()
            .filter_map(|f| {
                let name = f.get("name").and_then(Json::as_str)?.to_string();
                let mode = f.get("mode").and_then(Json::as_str).unwrap_or("NULLABLE");
                Some(ColumnMeta {
                    // `REQUIRED` is BigQuery's NOT NULL; `NULLABLE` and `REPEATED`
                    // are both nullable (an empty array is a legal repeated value).
                    not_null: mode.eq_ignore_ascii_case("REQUIRED"),
                    primary_key: key_names.contains(&name),
                    type_name: Some(field_type(f)),
                    default: f
                        .get("defaultValueExpression")
                        .and_then(Json::as_str)
                        .map(str::to_string),
                    // BigQuery has no identity or sequence column: a generated id is
                    // an explicit `GENERATE_UUID()` default, not auto-numbering.
                    auto_increment: false,
                    name,
                })
            })
            .collect(),
        // Secondary indexes do not exist on BigQuery (only search indexes), and
        // foreign keys are reported through `foreign_keys` rather than per table.
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    })
}

/// A schema field's type as RED reports it, reconstructing the shapes BigQuery
/// splits across `type` and `mode`: a repeated field becomes `ARRAY<…>` and a
/// record spells out its members, so the tree shows `ARRAY<STRUCT<id INT64>>`
/// rather than a bare `RECORD` that says nothing about what is inside.
fn field_type(field: &Json) -> String {
    // Folded onto the GoogleSQL spelling first, because REST answers with the legacy
    // names and `INTEGER` there means INT64 — see `value::base_type`. Reporting the
    // raw name would have a migration create a 32-bit column for a 64-bit one.
    let raw = field.get("type").and_then(Json::as_str).unwrap_or("STRING");
    let base = super::value::base_type(raw);
    let inner = match base {
        "STRUCT" => {
            let members: Vec<String> = field
                .get("fields")
                .and_then(Json::as_array)
                .map(|fs| {
                    fs.iter()
                        .filter_map(|f| {
                            let name = f.get("name").and_then(Json::as_str)?;
                            Some(format!("{name} {}", field_type(f)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if members.is_empty() {
                "STRUCT".to_string()
            } else {
                format!("STRUCT<{}>", members.join(", "))
            }
        }
        _ => base.to_string(),
    };
    if field
        .get("mode")
        .and_then(Json::as_str)
        .is_some_and(|m| m.eq_ignore_ascii_case("REPEATED"))
    {
        format!("ARRAY<{inner}>")
    } else {
        inner
    }
}

/// The columns of `table`'s declared `PRIMARY KEY`, or empty when it has none, the
/// views are unreadable, or the query is refused. Never an error: this is a badge,
/// not a prerequisite.
async fn primary_key(driver: &BigqueryDriver, dataset: &str, table: &str) -> Vec<String> {
    let qualified = format!("{}.{}", bq_quote(&driver.project), bq_quote(dataset));
    // A literal rather than a parameter: a catalog read is built from identifiers RED
    // already holds, escaped for the one context it enters.
    let literal = table.replace('\'', "\\'");
    let rows = driver
        .catalog_rows(&format!(
            "SELECT k.column_name \
             FROM {qualified}.INFORMATION_SCHEMA.KEY_COLUMN_USAGE AS k \
             JOIN {qualified}.INFORMATION_SCHEMA.TABLE_CONSTRAINTS AS c \
               ON c.constraint_name = k.constraint_name \
              AND c.table_name = k.table_name \
             WHERE k.table_name = '{literal}' AND c.constraint_type = 'PRIMARY KEY' \
             ORDER BY k.ordinal_position"
        ))
        .await
        .unwrap_or_default();
    rows.rows.iter().map(|r| text(r, 0)).collect()
}

/// The connection-wide foreign-key graph.
///
/// **Empty for now**, the documented degradation the trait already allows ("an
/// engine without relational FKs returns an empty graph, so the feature degrades to
/// absent"). BigQuery does have declarable foreign keys, so this is a gap rather
/// than an absence — see the BigQuery entry in `docs/deferred.md`. It is left
/// unimplemented rather than guessed at because the read spans three
/// `INFORMATION_SCHEMA` views whose region-level availability could not be verified
/// against a live project, and a relation graph that is subtly wrong sends
/// click-through to the wrong table.
pub(super) async fn foreign_keys(_driver: &BigqueryDriver) -> Result<Vec<FkEdge>> {
    Ok(Vec::new())
}

/// How many routines of each kind live in each dataset.
///
/// **Deliberately empty**, so the Functions and Procedures groups fall back to
/// click-to-discover — the degradation the trait documents for an engine that
/// cannot answer.
///
/// Answering it would cost a `routines.list` per dataset at *connect*, which is
/// precisely the "one query per schema multiplying connect cost on a server with
/// many schemas" that the trait's own doc warns against, for a list nobody has
/// asked to see. The project-wide `INFORMATION_SCHEMA.ROUTINES` read that would
/// answer it in one query is the thing that made the schema tree unloadable for a
/// dataset-scoped role in the first place (see [`list_objects`]).
///
/// The visible cost is a group node that may deflate to "none" on expand. That is
/// the lesser evil here: the alternative is a slower connect for everyone.
pub(super) async fn object_group_counts(
    _driver: &BigqueryDriver,
) -> Result<Vec<(String, ObjectKind, usize)>> {
    Ok(Vec::new())
}

/// The names of one routine kind in one dataset, pulled when its group is expanded.
///
/// `routines.list` over REST, needing only `bigquery.routines.list` on this one
/// dataset — the permission the project-wide `INFORMATION_SCHEMA.ROUTINES` read
/// demanded across the whole project.
pub(super) async fn list_object_group(
    driver: &BigqueryDriver,
    namespace: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectMeta>> {
    if routine_kind_wanted(kind).is_none() {
        return Ok(Vec::new());
    }
    let dataset = if namespace.is_empty() {
        driver.namespace()
    } else {
        namespace
    };
    let path = format!("projects/{}/datasets/{dataset}/routines", driver.project);
    let mut names: Vec<ObjectMeta> = driver
        .list_items(&path, "routines")
        .await?
        .iter()
        .filter_map(|r| {
            // Filtered client-side: `routines.list` takes no type filter, and one
            // dataset's routine list is small enough that asking for all of them and
            // keeping the matching ones beats a second call.
            let found = routine_kind(r.get("routineType").and_then(Json::as_str).unwrap_or(""))?;
            if found != kind {
                return None;
            }
            let name = r
                .pointer("/routineReference/routineId")
                .and_then(Json::as_str)?
                .to_string();
            Some(ObjectMeta { name, kind })
        })
        .collect();
    names.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(names)
}

/// A routine's reported type -> RED's kind. A `TABLE_VALUED_FUNCTION` is reported as
/// a function: it is a routine you call, and RED has no separate kind for it.
/// Covers both spellings, since REST says `SCALAR_FUNCTION` where
/// `INFORMATION_SCHEMA` says `FUNCTION`.
fn routine_kind(routine_type: &str) -> Option<ObjectKind> {
    match routine_type.to_ascii_uppercase().as_str() {
        "PROCEDURE" => Some(ObjectKind::Procedure),
        "SCALAR_FUNCTION" | "TABLE_VALUED_FUNCTION" | "FUNCTION" | "TABLE_FUNCTION" => {
            Some(ObjectKind::Function)
        }
        _ => None,
    }
}

/// Whether BigQuery has routines of this kind at all.
fn routine_kind_wanted(kind: ObjectKind) -> Option<ObjectKind> {
    matches!(kind, ObjectKind::Procedure | ObjectKind::Function).then_some(kind)
}

/// An object's definition as SQL.
///
/// BigQuery keeps a `ddl` column on both `TABLES` and `ROUTINES`, so this returns
/// **the server's own text** verbatim rather than assembling a statement from the
/// catalog — the honest answer, and the same one the ClickHouse/MySQL/SQLite arms
/// give from their `SHOW CREATE`.
pub(super) async fn object_ddl(
    driver: &BigqueryDriver,
    namespace: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<String> {
    let dataset = if namespace.is_empty() {
        driver.namespace()
    } else {
        namespace
    };
    let qualified = format!("{}.{}", bq_quote(&driver.project), bq_quote(dataset));
    let literal = name.replace('\'', "\\'");
    let sql = match kind {
        ObjectKind::Function | ObjectKind::Procedure => format!(
            "SELECT ddl FROM {qualified}.INFORMATION_SCHEMA.ROUTINES \
             WHERE routine_name = '{literal}'"
        ),
        _ => format!(
            "SELECT ddl FROM {qualified}.INFORMATION_SCHEMA.TABLES \
             WHERE table_name = '{literal}'"
        ),
    };
    let rows = driver.catalog_rows(&sql).await?;
    let ddl = rows.rows.first().map(|r| text(r, 0)).unwrap_or_default();
    if ddl.is_empty() {
        return Err(RedError::Query(format!(
            "BigQuery reports no definition for {dataset}.{name}"
        )));
    }
    Ok(ddl)
}

/// `EXPLAIN ANALYZE`: run the statement and read the stages BigQuery recorded.
///
/// There is no plan without execution on this engine — the query plan lives in the
/// finished job's statistics — which is exactly why plain `explain` is a dry run
/// instead (see the caller).
pub(super) async fn explain_analyze(driver: &BigqueryDriver, sql: &str) -> Result<QueryPlan> {
    let job = driver.submit(sql, &[], &[]).await?;
    // Draining no rows: the plan is on the job, not in the result.
    driver.results(&job, None, None, 0).await?;
    let mut query = Vec::new();
    if let Some(loc) = &job.location {
        query.push(("location", loc.clone()));
    }
    let json = driver
        .get(
            &format!("projects/{}/jobs/{}", driver.project, job.id),
            &query,
        )
        .await?;
    let stages = json
        .pointer("/statistics/query/queryPlan")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let nodes: Vec<PlanNode> = stages.iter().map(plan_stage).collect();
    let bytes = json
        .pointer("/statistics/query/totalBytesProcessed")
        .and_then(json_u64)
        .unwrap_or(0);
    // `raw` is the "Copy plan" payload and the fallback the UI shows when `nodes` is
    // empty, so it carries the stage JSON *and* the byte count -- the figure a
    // BigQuery user is most often reading a plan to find.
    let raw = format!(
        "totalBytesProcessed = {bytes}\n{}",
        serde_json::to_string_pretty(&stages).unwrap_or_default()
    );
    Ok(QueryPlan {
        nodes,
        raw,
        analyzed: true,
    })
}

/// One `queryPlan` stage as a [`PlanNode`], with its substeps as children.
fn plan_stage(stage: &Json) -> PlanNode {
    let str_at = |k: &str| {
        stage
            .get(k)
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string()
    };
    let num_at = |k: &str| stage.get(k).and_then(json_u64);
    let mut metrics = Vec::new();
    for (key, label) in [
        ("recordsRead", "records read"),
        ("recordsWritten", "records written"),
        ("shuffleOutputBytes", "shuffle bytes"),
        ("parallelInputs", "parallel inputs"),
    ] {
        if let Some(v) = num_at(key) {
            metrics.push((label.to_string(), v.to_string()));
        }
    }
    let children = stage
        .get("steps")
        .and_then(Json::as_array)
        .map(|steps| {
            steps
                .iter()
                .map(|step| {
                    let kind = step.get("kind").and_then(Json::as_str).unwrap_or("step");
                    let substeps = step
                        .get("substeps")
                        .and_then(Json::as_array)
                        .map(|s| {
                            s.iter()
                                .filter_map(Json::as_str)
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .filter(|s| !s.is_empty());
                    PlanNode {
                        label: kind.to_string(),
                        detail: substeps,
                        metrics: Vec::new(),
                        children: Vec::new(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    PlanNode {
        label: format!("{} ({})", str_at("name"), str_at("status")),
        detail: None,
        metrics,
        children,
    }
}

/// A live snapshot of what the project is spending and running.
///
/// BigQuery has no server to be out of memory, so this reports what is meaningful
/// for a serverless warehouse instead: bytes billed over the last day, slot time
/// consumed, and jobs in flight. `ServerSnapshot` carries an open list of metrics
/// rather than fixed memory/uptime fields, which is what makes that possible.
pub(super) async fn server_metrics(driver: &BigqueryDriver) -> Result<ServerSnapshot> {
    let region = region(driver).await;
    let mut snapshot = ServerSnapshot::new(DbKind::Bigquery, now_unix());
    let rows = driver
        .catalog_rows(&format!(
            "SELECT \
               countif(state = 'RUNNING'), \
               countif(state = 'PENDING'), \
               ifnull(sum(total_bytes_billed), 0), \
               ifnull(sum(total_slot_ms), 0), \
               count(*) \
             FROM {region}.INFORMATION_SCHEMA.JOBS_BY_PROJECT \
             WHERE creation_time > timestamp_sub(current_timestamp(), INTERVAL 1 DAY)"
        ))
        .await;
    let Ok(page) = rows else {
        // The likeliest cause by far is a role without `bigquery.jobs.listAll`, and
        // reporting it is the contract: a check that silently vanishes reads as a
        // clean bill of health.
        snapshot.note_unavailable(
            "job history: this role may not read INFORMATION_SCHEMA.JOBS_BY_PROJECT",
        );
        return Ok(snapshot);
    };
    let row = page.rows.first().cloned().unwrap_or_default();
    snapshot.push(ServerMetric::new(
        MetricGroup::Throughput,
        "jobs_running",
        "Jobs running",
        MetricValue::Count(number(&row, 0)),
    ));
    snapshot.push(ServerMetric::new(
        MetricGroup::Throughput,
        "jobs_pending",
        "Jobs queued",
        MetricValue::Count(number(&row, 1)),
    ));
    snapshot.push(ServerMetric::new(
        MetricGroup::Storage,
        "bytes_billed_24h",
        "Billed (last 24h)",
        MetricValue::Bytes(number(&row, 2)),
    ));
    snapshot.push(ServerMetric::new(
        MetricGroup::Throughput,
        "slot_ms_24h",
        "Slot time (last 24h)",
        MetricValue::Total(number(&row, 3)),
    ));
    snapshot.push(ServerMetric::new(
        MetricGroup::Throughput,
        "jobs_24h",
        "Jobs (last 24h)",
        MetricValue::Total(number(&row, 4)),
    ));
    Ok(snapshot)
}

/// The jobs running right now, longest first.
///
/// The second return value is the "you are only seeing your own" flag: without
/// `bigquery.jobs.listAll` a role sees just its own jobs, which is a normal state
/// on a locked-down project rather than an error.
pub(super) async fn server_sessions(driver: &BigqueryDriver) -> Result<(Vec<ServerSession>, bool)> {
    let region = region(driver).await;
    let rows = driver
        .catalog_rows(&format!(
            "SELECT job_id, user_email, state, query, \
               timestamp_diff(current_timestamp(), creation_time, MILLISECOND), \
               ifnull(destination_table.dataset_id, '') \
             FROM {region}.INFORMATION_SCHEMA.JOBS_BY_PROJECT \
             WHERE state != 'DONE' \
             ORDER BY creation_time \
             LIMIT 200"
        ))
        .await;
    let Ok(page) = rows else {
        return Ok((Vec::new(), true));
    };
    let sessions = page
        .rows
        .iter()
        .map(|r| ServerSession {
            key: SessionKey(text(r, 0)),
            user: Some(text(r, 1)).filter(|u| !u.is_empty()),
            application: None,
            client_addr: None,
            database: Some(text(r, 5)).filter(|d| !d.is_empty()),
            state: text(r, 2),
            wait: None,
            blocked_by: Vec::new(),
            query: Some(text(r, 3)).filter(|q| !q.is_empty()),
            // Computed server-side, so a wrong local clock cannot render a negative
            // duration.
            elapsed_secs: number(r, 4) as f64 / 1000.0,
            // RED's own jobs are the ones it minted; the prefix is how they are told
            // apart, since a job carries no connection identity.
            is_self: text(r, 0).starts_with("red_"),
        })
        .collect();
    Ok((sessions, false))
}
