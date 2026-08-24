//! Mapping BigQuery's REST payloads onto RED's [`Column`]/[`Value`] pair.
//!
//! The wire format is the thing to understand here: BigQuery's REST API returns
//! **every scalar as a JSON string**, whatever its type — an `INT64` arrives as
//! `"42"`, a `FLOAT64` as `"1.5"`, a `BOOL` as `"true"`. So unlike the ClickHouse
//! driver, which classifies a cell from the JSON token it arrived as, this one is
//! driven entirely by the column's declared type from the job schema.
//!
//! That turns out to be a *safer* shape for the two cases that lose data silently
//! elsewhere: `NUMERIC`/`BIGNUMERIC` never pass through an `f64` (they keep the
//! digits the server sent), and `INT64` is parsed from its own text rather than
//! from a JSON number that could not hold it.
//!
//! Structured values are the exception to "everything is a string". A `REPEATED`
//! field arrives as `[{"v": …}, …]` and a `RECORD` as `{"f": [{"v": …}, …]}`, so
//! both are re-rendered as compact JSON text — the same treatment the ClickHouse
//! driver gives `Array`/`Tuple`/`Map`, and for the same reason: a nested tree has
//! no [`Value`] to be.

use red_core::{Column, Value};
use serde_json::Value as Json;

use crate::CellCap;

/// One column of a job's result schema: the name, the declared GoogleSQL type, and
/// whether it repeats. `repeated` has to ride separately because BigQuery spells it
/// as a `mode`, not as part of the type — an `ARRAY<INT64>` is reported as type
/// `INTEGER` with mode `REPEATED`, and treating that as a plain integer would try
/// to parse `[{"v":"1"}]` as a number.
#[derive(Debug, Clone)]
pub(super) struct Field {
    pub(super) name: String,
    pub(super) ty: String,
    pub(super) repeated: bool,
}

impl Field {
    /// The declared type as RED reports it, with `ARRAY<…>` reconstructed for a
    /// repeated field so the grid header and the type lattice see the real shape
    /// rather than the element type.
    fn decl_type(&self) -> String {
        if self.repeated {
            format!("ARRAY<{}>", self.ty)
        } else {
            self.ty.clone()
        }
    }
}

/// Read a job's `schema.fields` into result columns plus the field metadata the
/// row mapper needs. An absent or malformed schema yields empty vectors rather
/// than an error: a DML statement's job legitimately has no schema at all.
pub(super) fn read_schema(schema: Option<&Json>) -> (Vec<Column>, Vec<Field>) {
    let fields: Vec<Field> = schema
        .and_then(|s| s.get("fields"))
        .and_then(Json::as_array)
        .map(|fs| {
            fs.iter()
                .map(|f| {
                    let str_at = |k: &str| f.get(k).and_then(Json::as_str).unwrap_or("");
                    Field {
                        name: str_at("name").to_string(),
                        // `type` is BigQuery's legacy spelling in places
                        // (`INTEGER`/`FLOAT`/`BOOLEAN`) and the GoogleSQL one in
                        // others (`INT64`/`FLOAT64`/`BOOL`); `base_type` folds both.
                        ty: str_at("type").to_string(),
                        repeated: str_at("mode").eq_ignore_ascii_case("REPEATED"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let columns = fields
        .iter()
        .map(|f| Column {
            name: f.name.clone(),
            decl_type: Some(f.decl_type()),
        })
        .collect();
    (columns, fields)
}

/// Map one `{"f": [{"v": …}, …]}` row object into display values.
///
/// A row shorter than the schema pads with [`Value::Null`] rather than erroring:
/// the alternative is failing a whole page over one malformed row, and a missing
/// cell is genuinely unknown.
pub(super) fn read_row(row: &Json, fields: &[Field], cap: Option<CellCap>) -> Vec<Value> {
    let cells = row.get("f").and_then(Json::as_array);
    (0..fields.len())
        .map(|i| {
            let raw = cells.and_then(|c| c.get(i)).and_then(|c| c.get("v"));
            match raw {
                Some(v) => cell_value(v, &fields[i], CellCap::caps(cap, i)),
                None => Value::Null,
            }
        })
        .collect()
}

/// Map one cell, guided by its declared type.
fn cell_value(v: &Json, field: &Field, max: Option<usize>) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    // Structured cells: re-render the subtree as compact JSON. Checked before the
    // type switch because a repeated INT64 is still an array on the wire.
    if field.repeated || is_struct(&field.ty) {
        return text_value(&render_json(v, field), max);
    }
    let Some(s) = v.as_str() else {
        // Not a string and not structured: BigQuery does not do this, but rendering
        // whatever arrived is strictly better than dropping the cell.
        return text_value(&v.to_string(), max);
    };
    match base_type(&field.ty) {
        "INT64" => s.parse::<i64>().map(Value::Integer).unwrap_or_else(|_| {
            // Out of i64 range should be impossible for INT64, but keeping the
            // digits is the non-lossy fallback.
            text_value(s, max)
        }),
        "FLOAT64" => s
            .parse::<f64>()
            .map(Value::Real)
            .unwrap_or_else(|_| text_value(s, max)),
        // Matching the ClickHouse driver, which also renders a boolean as 0/1 so
        // the grid's numeric alignment and its filters behave consistently.
        "BOOL" => Value::Integer(i64::from(s.eq_ignore_ascii_case("true"))),
        // BYTES arrive base64-encoded. Decoding restores the real bytes so the blob
        // viewer and an export see the value rather than its transport encoding.
        "BYTES" => match decode_base64(s) {
            Some(bytes) => match max {
                // A capped fetch keeps only the length, exactly as every other
                // driver does for an over-cap blob.
                Some(m) if bytes.len() > m => Value::capped_blob(bytes.len()),
                _ => Value::Blob(bytes),
            },
            None => text_value(s, max),
        },
        // Everything else keeps its text verbatim. This is what makes NUMERIC and
        // BIGNUMERIC exact: they are decimals whose digits do not survive an f64,
        // and BigQuery has already rendered them correctly.
        _ => text_value(s, max),
    }
}

/// Whether a declared type is a nested record.
fn is_struct(ty: &str) -> bool {
    let t = base_type(ty);
    t == "STRUCT" || t == "RECORD"
}

/// Normalise BigQuery's two spellings of the same type onto the GoogleSQL one.
/// The REST schema uses the legacy names (`INTEGER`, `FLOAT`, `BOOLEAN`) while
/// `INFORMATION_SCHEMA` and DDL use the modern ones, and both reach this code.
fn base_type(ty: &str) -> &str {
    let t = ty.trim();
    // A parameterised type (`NUMERIC(10, 2)`, `STRING(255)`) classifies on its head.
    let head = t.split(['(', '<']).next().unwrap_or(t).trim();
    match head.to_ascii_uppercase().as_str() {
        "INTEGER" | "INT64" | "INT" | "SMALLINT" | "BIGINT" | "TINYINT" | "BYTEINT" => "INT64",
        "FLOAT" | "FLOAT64" | "DOUBLE" => "FLOAT64",
        "BOOLEAN" | "BOOL" => "BOOL",
        "BYTES" => "BYTES",
        "RECORD" | "STRUCT" => "STRUCT",
        _ => head,
    }
}

/// Re-render a structured cell as compact JSON, unwrapping BigQuery's `{"v": …}` /
/// `{"f": […]}` envelope so the text reads as the data rather than as the transport.
///
/// A `RECORD`'s field *names* are not available here (the nested schema is not
/// threaded down), so a struct renders as a JSON array of its values in declared
/// order. That is the honest rendering of what arrived; inventing positional keys
/// would read as names the server never sent.
fn render_json(v: &Json, field: &Field) -> String {
    fn unwrap_cell(v: &Json) -> Json {
        // A repeated element or record field: `{"v": …}` -> the inner value.
        if let Some(inner) = v.get("v") {
            return unwrap_cell(inner);
        }
        // A record: `{"f": [ … ]}` -> an array of its unwrapped fields.
        if let Some(fs) = v.get("f").and_then(Json::as_array) {
            return Json::Array(fs.iter().map(unwrap_cell).collect());
        }
        match v {
            Json::Array(items) => Json::Array(items.iter().map(unwrap_cell).collect()),
            other => other.clone(),
        }
    }
    let _ = field;
    unwrap_cell(v).to_string()
}

/// A text [`Value`], capped to a display prefix when `max` is set. The twin of the
/// ClickHouse driver's helper of the same name.
fn text_value(s: &str, max: Option<usize>) -> Value {
    match max {
        Some(m) => Value::capped_text(s, m),
        None => Value::Text(s.into()),
    }
}

/// Decode standard (padded) base64, the encoding BigQuery uses for `BYTES`.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(ty: &str) -> Field {
        Field {
            name: "c".to_string(),
            ty: ty.to_string(),
            repeated: false,
        }
    }

    fn cell(json: &str, ty: &str) -> Value {
        let v: Json = serde_json::from_str(json).expect("test json");
        cell_value(&v, &field(ty), None)
    }

    #[test]
    fn scalars_are_classified_from_the_declared_type_not_the_json() {
        // Every scalar arrives as a JSON *string*; the type is what decides.
        assert_eq!(cell(r#""42""#, "INTEGER"), Value::Integer(42));
        assert_eq!(cell(r#""1.5""#, "FLOAT"), Value::Real(1.5));
        assert_eq!(cell(r#""true""#, "BOOLEAN"), Value::Integer(1));
        assert_eq!(cell(r#""false""#, "BOOL"), Value::Integer(0));
    }

    #[test]
    fn big_decimals_keep_their_digits() {
        // The regression this guards: routing a BIGNUMERIC through f64 silently
        // rounds it, and the grid then shows -- and exports -- the wrong number.
        let exact = "12345678901234567890.12345678901234567890";
        assert_eq!(
            cell(&format!(r#""{exact}""#), "BIGNUMERIC"),
            Value::Text(exact.into())
        );
    }

    #[test]
    fn null_is_null_whatever_the_type() {
        assert_eq!(cell("null", "INTEGER"), Value::Null);
        assert_eq!(cell("null", "STRING"), Value::Null);
    }

    #[test]
    fn repeated_fields_render_as_json_rather_than_parsing_as_their_element_type() {
        let f = Field {
            name: "xs".to_string(),
            ty: "INTEGER".to_string(),
            repeated: true,
        };
        let v: Json = serde_json::from_str(r#"[{"v":"1"},{"v":"2"}]"#).expect("test json");
        assert_eq!(cell_value(&v, &f, None), Value::Text(r#"["1","2"]"#.into()));
    }

    #[test]
    fn records_unwrap_their_envelope() {
        let v: Json = serde_json::from_str(r#"{"f":[{"v":"a"},{"v":"b"}]}"#).expect("test json");
        assert_eq!(
            cell_value(&v, &field("RECORD"), None),
            Value::Text(r#"["a","b"]"#.into())
        );
    }

    #[test]
    fn bytes_are_base64_decoded() {
        // "hi" == aGk=
        assert_eq!(cell(r#""aGk=""#, "BYTES"), Value::Blob(b"hi".to_vec()));
    }

    #[test]
    fn a_short_row_pads_with_nulls_rather_than_failing_the_page() {
        let (_, fields) = read_schema(Some(
            &serde_json::from_str(
                r#"{"fields":[{"name":"a","type":"INTEGER"},{"name":"b","type":"STRING"}]}"#,
            )
            .expect("test json"),
        ));
        let row: Json = serde_json::from_str(r#"{"f":[{"v":"7"}]}"#).expect("test json");
        assert_eq!(
            read_row(&row, &fields, None),
            vec![Value::Integer(7), Value::Null]
        );
    }

    #[test]
    fn schema_reports_array_types_for_repeated_columns() {
        let (columns, _) = read_schema(Some(
            &serde_json::from_str(r#"{"fields":[{"name":"xs","type":"INT64","mode":"REPEATED"}]}"#)
                .expect("test json"),
        ));
        assert_eq!(columns[0].decl_type.as_deref(), Some("ARRAY<INT64>"));
    }

    #[test]
    fn a_dml_job_with_no_schema_yields_no_columns() {
        let (columns, fields) = read_schema(None);
        assert!(columns.is_empty() && fields.is_empty());
    }
}
