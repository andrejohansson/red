//! BigQuery introspection, the Server panel, and `EXPLAIN ANALYZE`.
//!
//! Everything here reads `INFORMATION_SCHEMA`, and the important structural note is
//! *which* `INFORMATION_SCHEMA`. BigQuery exposes these views at two scopes: per
//! dataset (`` `my_dataset`.INFORMATION_SCHEMA.TABLES ``) and per region
//! (`` `region-us`.INFORMATION_SCHEMA.TABLES ``, covering every dataset in the
//! project). The region scope is what keeps [`list_objects`] honest about its
//! cheapness contract — one query for the whole connection rather than one per
//! dataset — and it is the reason the driver bothers to learn its location.
//!
//! Catalog reads are metadata-only and bill no bytes, so they are the one part of
//! this driver that costs nothing to repeat.

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

/// The schema-tree skeleton: every dataset in the project with its relations.
///
/// Two region-scoped reads, not one per dataset: `SCHEMATA` so a dataset with no
/// tables still draws (an empty dataset is real, and omitting it would make the
/// tree disagree with the console), and `TABLES` for the relations themselves.
pub(super) async fn list_objects(driver: &BigqueryDriver) -> Result<Vec<SchemaMeta>> {
    let region = region(driver).await;
    let datasets = driver
        .catalog_rows(&format!(
            "SELECT schema_name FROM {region}.INFORMATION_SCHEMA.SCHEMATA ORDER BY schema_name"
        ))
        .await?;
    let tables = driver
        .catalog_rows(&format!(
            "SELECT table_schema, table_name, table_type \
             FROM {region}.INFORMATION_SCHEMA.TABLES \
             ORDER BY table_schema, table_name"
        ))
        .await?;

    let mut schemas: Vec<SchemaMeta> = datasets
        .rows
        .iter()
        .map(|r| SchemaMeta {
            name: text(r, 0),
            objects: Vec::new(),
        })
        .collect();
    for row in &tables.rows {
        let (schema, name, kind) = (text(row, 0), text(row, 1), table_kind(&text(row, 2)));
        let Some(target) = schemas.iter_mut().find(|s| s.name == schema) else {
            // A table in a dataset `SCHEMATA` did not list (a race, or a permission
            // that covers the table but not the dataset listing): keep the object
            // rather than dropping it, and grow the group for it.
            schemas.push(SchemaMeta {
                name: schema,
                objects: vec![ObjectMeta { name, kind }],
            });
            continue;
        };
        target.objects.push(ObjectMeta { name, kind });
    }
    Ok(schemas)
}

/// Map `TABLES.table_type` onto RED's kinds. BigQuery reports `BASE TABLE`,
/// `VIEW`, `MATERIALIZED VIEW`, `EXTERNAL`, `SNAPSHOT` and `CLONE`; the last four
/// are all *tables you can select from*, so they draw as tables rather than being
/// hidden for lacking a dedicated kind.
fn table_kind(table_type: &str) -> ObjectKind {
    match table_type.to_ascii_uppercase().as_str() {
        "VIEW" => ObjectKind::View,
        "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
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
    let qualified = format!("{}.{}", bq_quote(&driver.project), bq_quote(&dataset));
    // Literals rather than parameters: a catalog read is built from identifiers RED
    // already holds, and the table name is escaped for the one context it enters.
    let literal = table.replace('\'', "\\'");
    let columns = driver
        .catalog_rows(&format!(
            "SELECT column_name, data_type, is_nullable, column_default \
             FROM {qualified}.INFORMATION_SCHEMA.COLUMNS \
             WHERE table_name = '{literal}' ORDER BY ordinal_position"
        ))
        .await?;
    // A table with no declared primary key is the norm here, and the join through
    // TABLE_CONSTRAINTS is what tells a PRIMARY KEY from a FOREIGN KEY constraint.
    let keys = driver
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
        // Not fatal: these views are newer than the rest of INFORMATION_SCHEMA and a
        // role may not be able to read them. A table without a reported key still
        // browses; it simply has no key badge.
        .unwrap_or_default();
    let key_names: Vec<String> = keys.rows.iter().map(|r| text(r, 0)).collect();

    Ok(TableDetail {
        columns: columns
            .rows
            .iter()
            .map(|r| {
                let name = text(r, 0);
                ColumnMeta {
                    not_null: text(r, 2).eq_ignore_ascii_case("NO"),
                    primary_key: key_names.contains(&name),
                    type_name: Some(text(r, 1)).filter(|t| !t.is_empty()),
                    default: Some(text(r, 3)).filter(|d| !d.is_empty() && d != "NULL"),
                    // BigQuery has no identity or sequence column: a generated id is
                    // an explicit `GENERATE_UUID()` default, not auto-numbering.
                    auto_increment: false,
                    name,
                }
            })
            .collect(),
        // Secondary indexes do not exist on BigQuery (only search indexes), and
        // foreign keys are reported through `foreign_keys` rather than per table.
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    })
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

/// How many routines of each kind live in each dataset, so the tree never draws a
/// group that turns out to be empty.
pub(super) async fn object_group_counts(
    driver: &BigqueryDriver,
) -> Result<Vec<(String, ObjectKind, usize)>> {
    let region = region(driver).await;
    let rows = driver
        .catalog_rows(&format!(
            "SELECT routine_schema, routine_type, count(*) \
             FROM {region}.INFORMATION_SCHEMA.ROUTINES \
             GROUP BY routine_schema, routine_type"
        ))
        .await
        // A project whose routines are not readable reports nothing, and the groups
        // fall back to click-to-discover. The caller tells that from a failure by
        // whether the call succeeded.
        .unwrap_or_default();
    Ok(rows
        .rows
        .iter()
        .filter_map(|r| {
            routine_kind(&text(r, 1)).map(|kind| (text(r, 0), kind, number(r, 2) as usize))
        })
        .collect())
}

/// The names of one routine kind in one dataset, pulled when its group is expanded.
pub(super) async fn list_object_group(
    driver: &BigqueryDriver,
    namespace: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectMeta>> {
    let Some(wanted) = routine_type(kind) else {
        return Ok(Vec::new());
    };
    let qualified = format!("{}.{}", bq_quote(&driver.project), bq_quote(namespace));
    let rows = driver
        .catalog_rows(&format!(
            "SELECT routine_name FROM {qualified}.INFORMATION_SCHEMA.ROUTINES \
             WHERE routine_type = '{wanted}' ORDER BY routine_name"
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .map(|r| ObjectMeta {
            name: text(r, 0),
            kind,
        })
        .collect())
}

/// `ROUTINES.routine_type` -> RED's kind. A `TABLE_FUNCTION` is reported as a
/// function: it is a routine you call, and RED has no separate kind for it.
fn routine_kind(routine_type: &str) -> Option<ObjectKind> {
    match routine_type.to_ascii_uppercase().as_str() {
        "PROCEDURE" => Some(ObjectKind::Procedure),
        "FUNCTION" | "TABLE_FUNCTION" => Some(ObjectKind::Function),
        _ => None,
    }
}

/// The `routine_type` literal for one kind, or `None` for a kind BigQuery has no
/// routines of.
fn routine_type(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Procedure => Some("PROCEDURE"),
        ObjectKind::Function => Some("FUNCTION"),
        _ => None,
    }
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
