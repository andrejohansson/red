//! The BigQuery write paths: bulk insert, create, truncate.
//!
//! Insert-only by design (`docs/adr/0003`). A writable BigQuery connection is a
//! copy / migration **target** and can take draft rows from the grid; in-grid
//! `UPDATE`/`DELETE` are refused here rather than half-supported, because a
//! declared `PRIMARY KEY` on BigQuery is not enforced (so it cannot address one
//! row) and a point update scans the whole table.
//!
//! Everything is **bound**, never interpolated: BigQuery takes positional `?`
//! parameters, so the shared `insert_sql` body works unchanged and the
//! "identifiers quoted, values bound" invariant holds on this engine exactly as it
//! does on the relational ones.

use red_core::{Column, EditOp, RedError, Result, TableRef, Value};
use serde_json::Value as Json;

use super::{BigqueryDriver, bq_quote};
use crate::{DatabaseDriver, insert_chunk_rows, insert_sql, refuse_if_read_only};

/// Draft rows headed for one table under one column list, so a multi-row insert
/// becomes a single statement.
struct InsertGroup<'a> {
    table: &'a TableRef,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

/// Cap on bound parameters in one request. BigQuery's documented ceiling is 10,000
/// query parameters; staying well under it leaves room for the statement itself and
/// keeps a request body a sane size.
const PARAM_CAP: usize = 5_000;

/// Bulk-insert `rows` as multi-row `INSERT … VALUES`, sub-chunked under
/// [`PARAM_CAP`].
///
/// Each chunk is its own job, so this is **not** atomic across chunks — unlike the
/// relational drivers, which wrap the whole call in one transaction. BigQuery has
/// transactions but not across separate jobs (that needs a session), so a large
/// insert that fails midway leaves the earlier chunks committed. The trait's
/// insert contract makes no one-row assertion to break, and the failure is
/// reported rather than hidden, but a caller retrying a failed bulk insert should
/// expect duplicates rather than a clean slate.
pub(super) async fn insert_rows(
    driver: &BigqueryDriver,
    table: &TableRef,
    columns: &[Column],
    rows: &[Vec<Value>],
) -> Result<u64> {
    refuse_if_read_only(driver.read_only)?;
    if rows.is_empty() {
        return Ok(0);
    }
    let target = driver.qualified(table);
    let per_chunk = insert_chunk_rows(columns.len(), PARAM_CAP);
    let mut total = 0u64;
    for chunk in rows.chunks(per_chunk) {
        let (sql, params) = insert_sql(&target, columns, chunk, bq_quote, |_, _, _| {
            // The cast rides on the *parameter type* (see `query_parameter`), not on
            // the placeholder, so every slot is a bare `?`.
            "?".to_string()
        });
        let affected = driver.run_dml(&sql, &params, columns).await?;
        // A successful INSERT affects exactly the rows it was given; BigQuery
        // reports it, but fall back to the chunk size rather than reporting 0 for a
        // statement that plainly worked.
        total += if affected == 0 {
            chunk.len() as u64
        } else {
            affected
        };
    }
    Ok(total)
}

/// Apply a batch of grid edits.
///
/// An all-`Insert` batch is the draft-row path and folds into [`insert_rows`],
/// which is why inserting stays available on a table whose `edit_caps` report
/// `EditMode::None`. Anything else is refused with the reason, so the user gets an
/// explanation rather than a silent no-op.
pub(super) async fn apply_edits(driver: &BigqueryDriver, ops: &[EditOp]) -> Result<u64> {
    refuse_if_read_only(driver.read_only)?;
    if ops.is_empty() {
        return Ok(0);
    }
    // Grouped by (target, column list) so a multi-row draft insert becomes one
    // statement rather than one per row. Insertion order is preserved: `Vec` rather
    // than a map, because a draft's rows should land in the order the user entered
    // them, and the group count is tiny.
    let mut groups: Vec<InsertGroup<'_>> = Vec::new();
    for op in ops {
        let EditOp::Insert { table, values } = op else {
            return Err(RedError::Driver(
                "BigQuery rows can't be updated or deleted from the grid: a declared \
                 PRIMARY KEY is not enforced there, so it can't address a single row, \
                 and a point update scans the whole table"
                    .to_string(),
            ));
        };
        let columns: Vec<Column> = values
            .iter()
            .map(|cv| Column {
                name: cv.column.clone(),
                decl_type: cv.decl_type.clone(),
            })
            .collect();
        let row: Vec<Value> = values.iter().map(|cv| cv.value.clone()).collect();
        match groups
            .iter_mut()
            .find(|g| g.table == table && g.columns == columns)
        {
            Some(group) => group.rows.push(row),
            None => groups.push(InsertGroup {
                table,
                columns,
                rows: vec![row],
            }),
        }
    }
    let mut total = 0u64;
    for group in &groups {
        total += insert_rows(driver, group.table, &group.columns, &group.rows).await?;
    }
    Ok(total)
}

/// Empty `table`.
///
/// **`TRUNCATE`, not `DELETE`** — a documented deviation from the trait's
/// prescription (`docs/adr/0003`). That prescription exists because MySQL's
/// `TRUNCATE` auto-commits and resets auto-increment; BigQuery has no
/// auto-increment, and here the economics are lopsided: `TRUNCATE TABLE` is a free
/// metadata operation while `DELETE FROM t WHERE TRUE` (BigQuery rejects a bare
/// `DELETE`) is a billed full-table scan.
pub(super) async fn clear_table(driver: &BigqueryDriver, table: &TableRef) -> Result<u64> {
    refuse_if_read_only(driver.read_only)?;
    let sql = format!("TRUNCATE TABLE {}", driver.quote_table(table));
    driver.execute(&sql).await
}

/// Render one bound value as a BigQuery query parameter.
///
/// BigQuery types its parameters explicitly, which is what makes the `decl_type`
/// argument load-bearing rather than advisory: a value RED decoded to
/// [`Value::Text`] (a date, a timestamp, a NUMERIC) must be declared as the target
/// column's type, or the insert fails with a type mismatch. Declaring it `STRING`
/// and hoping for coercion does not work — GoogleSQL does not implicitly convert a
/// string parameter to a `DATE`.
pub(super) fn query_parameter(value: &Value, decl_type: Option<&str>) -> Json {
    let (ty, rendered) = match value {
        // Never reached: `insert_sql`/`edit_sql` emit a literal `NULL` for a null
        // cell and do not bind it, precisely so no engine's binder meets a typeless
        // null. Kept total rather than panicking.
        Value::Null => ("STRING".to_string(), Json::Null),
        Value::Integer(i) => (
            // An integer landing in a non-integer column (a 0/1 into a BOOL, an
            // epoch into a TIMESTAMP) is declared as the column's type so BigQuery
            // does the conversion it does know how to do.
            param_type(decl_type, "INT64"),
            Json::String(i.to_string()),
        ),
        Value::Real(f) => (
            param_type(decl_type, "FLOAT64"),
            Json::String(f.to_string()),
        ),
        Value::Text(t) => (param_type(decl_type, "STRING"), Json::String(t.to_string())),
        Value::Blob(b) => {
            use base64::Engine;
            (
                "BYTES".to_string(),
                Json::String(base64::engine::general_purpose::STANDARD.encode(b)),
            )
        }
        // A capped cell never reaches a write path by contract (only display fetches
        // produce one); binding its truncated head would silently write a prefix, so
        // send the head with the column's type and let a mismatch surface.
        Value::Capped(c) => (
            param_type(decl_type, "STRING"),
            Json::String(c.head.clone()),
        ),
    };
    serde_json::json!({
        "parameterType": { "type": ty },
        "parameterValue": { "value": rendered },
    })
}

/// The GoogleSQL parameter type for a target column, falling back to `default`
/// when the column's declared type is unknown or is not one BigQuery names.
///
/// Deliberately narrow: only the scalar types a bound value can meaningfully carry
/// are recognised. A `STRUCT`/`ARRAY` column falls back rather than being guessed
/// at, because a parameter for one needs a nested type declaration this shape
/// cannot express.
fn param_type(decl_type: Option<&str>, default: &str) -> String {
    let Some(decl) = decl_type else {
        return default.to_string();
    };
    let head = decl
        .trim()
        .split(['(', '<'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    match head.as_str() {
        "INT64" | "INTEGER" | "INT" | "SMALLINT" | "BIGINT" | "TINYINT" | "BYTEINT" => {
            "INT64".to_string()
        }
        "FLOAT64" | "FLOAT" | "DOUBLE" => "FLOAT64".to_string(),
        "NUMERIC" | "DECIMAL" => "NUMERIC".to_string(),
        "BIGNUMERIC" | "BIGDECIMAL" => "BIGNUMERIC".to_string(),
        "BOOL" | "BOOLEAN" => "BOOL".to_string(),
        "STRING" | "CHAR" | "VARCHAR" | "TEXT" => "STRING".to_string(),
        "BYTES" | "BLOB" => "BYTES".to_string(),
        "DATE" => "DATE".to_string(),
        "DATETIME" => "DATETIME".to_string(),
        "TIME" => "TIME".to_string(),
        "TIMESTAMP" => "TIMESTAMP".to_string(),
        "JSON" => "JSON".to_string(),
        "GEOGRAPHY" => "GEOGRAPHY".to_string(),
        _ => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_value_is_declared_as_its_target_column_type() {
        // The regression this guards: declaring a date string as STRING makes
        // BigQuery reject the insert, because it will not implicitly convert a
        // string parameter to a DATE.
        let p = query_parameter(&Value::Text("2024-01-01".into()), Some("DATE"));
        assert_eq!(p["parameterType"]["type"], "DATE");
        assert_eq!(p["parameterValue"]["value"], "2024-01-01");
    }

    #[test]
    fn an_unknown_column_type_falls_back_to_the_value_shape() {
        let p = query_parameter(&Value::Integer(5), Some("ARRAY<INT64>"));
        assert_eq!(p["parameterType"]["type"], "INT64");
    }

    #[test]
    fn integers_are_sent_as_strings_so_int64_survives() {
        // JSON numbers cannot hold the full INT64 range losslessly, which is why
        // BigQuery specifies the string form for every int64-valued field.
        let p = query_parameter(&Value::Integer(i64::MAX), None);
        assert_eq!(p["parameterValue"]["value"], i64::MAX.to_string());
    }

    #[test]
    fn blobs_are_base64_encoded_as_bytes() {
        let p = query_parameter(&Value::Blob(b"hi".to_vec()), Some("STRING"));
        // BYTES wins over the declared type: the value shape is decisive when the
        // two disagree about binary-ness.
        assert_eq!(p["parameterType"]["type"], "BYTES");
        assert_eq!(p["parameterValue"]["value"], "aGk=");
    }

    #[test]
    fn decimal_columns_keep_their_precision_type() {
        let p = query_parameter(&Value::Text("1.25".into()), Some("BIGNUMERIC(40, 20)"));
        assert_eq!(p["parameterType"]["type"], "BIGNUMERIC");
    }
}
