//! Shared domain types for RED. No UI, no async runtime, no driver knowledge:
//! the protocol/service/UI layers all speak in these types. Mirrors the role of
//! `nyx-core` in the Nyx codebase.

use std::fmt;
use std::sync::Arc;

pub mod ai;
pub mod conn;
pub mod ddl;
pub mod diff;
pub mod doc;
pub mod edit;
pub mod health;
pub mod kv;
pub mod schema;
pub mod schema_diff;
pub mod server;
pub mod sql;
pub mod transfer;
pub mod typemap;
pub mod valuefmt;

// Re-exported at the crate root, where every consumer already imports them from:
// splitting this file is a readability change, not a breaking one.
pub use ai::{
    ActivityId, ActivityKind, ActivityNode, ActivityStatus, AiLimits, AiPolicy, AiTier, PlanStep,
    PlanStepStatus,
};
pub use conn::{
    ConnEnv, ConnectionConfig, DbKind, NamespaceCaps, ParsedDsn, ProxyConfig, ProxyKind, SshAuth,
    SshConfig, WriteCaps, dsn_requests_tls,
};
pub use edit::{EditMode, RowEditCaps};
pub use schema::{
    BASE_ALIAS, ColumnMeta, FkEdge, FkJoin, ForeignKeyMeta, IndexMeta, KeyKind, KeySpec,
    ObjectKind, ObjectMeta, SchemaMeta, SortDirection, TableDetail,
};

/// How risky a database operation is, for the read-only gate and the
/// destructive-operation confirm. Shared across seams: Redis console commands
/// ([`kv::classify_command`]) and MongoDB writes ([`doc::classify_doc_op`]) both
/// classify into this, so the confirm-gating logic can match on one type
/// regardless of engine. A `Destructive` op needs an explicit confirm even on a
/// writable connection, so neither the console nor the AI can slip one through.
///
/// `Read` is only produced by seams that can tell reads from writes (the KV
/// console); write-only classifiers such as [`doc::classify_doc_op`] never
/// return it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// A read: always allowed, never gated.
    Read,
    /// An ordinary write: blocked on a read-only connection, otherwise allowed.
    Write,
    /// Wide-blast-radius or irreversible: gated behind a confirm even on a
    /// writable connection. Covers Redis's key-nuking / server-control family
    /// (`FLUSHALL`, `DEL`, `SHUTDOWN`, `CONFIG SET`, ...) and the MongoDB
    /// footguns (dropping a collection, an unfiltered or multi-document
    /// `update`/`delete`).
    Destructive,
}

/// One cell value in a result set. A deliberately small, render-friendly tagged
/// union: drivers map their native types onto this; the UI formats per variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    /// Text is an `Arc<str>`, not a `String`, so the display path can share the
    /// same buffer (a refcount bump) instead of copying the bytes a second time
    /// into the grid's `SharedString` cell — a resident text cell is stored once,
    /// not twice. `Arc<str>` is also narrower (16 vs 24 bytes). No UI/runtime dep:
    /// `Arc<str>` is std, and gpui's `SharedString` wraps it zero-copy.
    Text(Arc<str>),
    Blob(Vec<u8>),
    /// A cell the *display* fetch path capped: only a bounded prefix was read
    /// (over-cap text) or only the length (blob), so the full value was never
    /// materialized. Produced solely by capped fetch paths, never by `export` or
    /// a write path, where every cell is whole.
    ///
    /// Boxed: it's the rare, cold variant, and inlining its `String`+`usize`+`bool`
    /// (40 bytes) would widen *every* `Value` — including the common `Integer`/
    /// `Text` cell — to that size across a whole resident window. The box keeps
    /// `Value` at the `String`/`Vec` width (one indirection only on a capped cell).
    Capped(Box<CappedCell>),
}

/// The payload of a [`Value::Capped`] cell: what the grid shows plus the true byte
/// length of the source value, so a detail view can re-fetch the whole thing.
#[derive(Debug, Clone, PartialEq)]
pub struct CappedCell {
    /// The shown text: a char-boundary-safe prefix of over-cap text, or empty for
    /// a blob (rendered as its `<len bytes>` summary). No ellipsis; that's added
    /// at render time, so a copy path can tell the real head from the marker.
    pub head: String,
    /// True byte length of the source value (full text length, or blob size).
    pub len: usize,
    /// Blob vs text: drives the `<N bytes>` summary and the grid's faint styling.
    pub blob: bool,
}

impl Value {
    /// Build a display value from full text `s`, capping to `max_bytes`. Under the
    /// cap it's a whole [`Value::Text`]; over it, a [`Value::Capped`] holding only a
    /// char-boundary-safe prefix plus the true length; the bytes past the cap are
    /// never copied into the value, which is the point on the display fetch path.
    pub fn capped_text(s: &str, max_bytes: usize) -> Value {
        if s.len() <= max_bytes {
            return Value::Text(Arc::from(s));
        }
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        Value::Capped(Box::new(CappedCell {
            head: s[..end].to_owned(),
            len: s.len(),
            blob: false,
        }))
    }

    /// A blob reduced to its length for display; the bytes are never read. The
    /// grid only ever paints a blob as its `<N bytes>` summary; a copy/inspector
    /// re-fetches the real bytes on demand.
    pub fn capped_blob(len: usize) -> Value {
        Value::Capped(Box::new(CappedCell {
            head: String::new(),
            len,
            blob: true,
        }))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(n) => write!(f, "{n}"),
            Value::Real(x) => write!(f, "{x}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Blob(b) => write!(f, "<{} bytes>", b.len()),
            Value::Capped(c) if c.blob => write!(f, "<{} bytes>", c.len),
            Value::Capped(c) => write!(f, "{}…", c.head),
        }
    }
}

/// Column metadata for a result set. `name` is always present; `decl_type` is the
/// engine's declared type (best-effort; `None` for computed expressions) and
/// feeds type-aware cell rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub decl_type: Option<String>,
}

/// A one-column aggregate summary, all engine-computed by a single pushdown query
/// (`SELECT count(*), count(col), … FROM (<the result's filtered SQL>) sub`),
/// never by scanning the materialized window. `min`/`max`/`sum`/`avg` ride through
/// as typed [`Value`]s so the UI formats them like grid cells; the counts are
/// plain integers. See `DatabaseDriver::column_stats`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// `count(*)` over the (filtered) result.
    pub total: i64,
    /// `count(col)`: the non-null rows. `nulls = total - non_null` (derived).
    pub non_null: i64,
    /// `count(distinct col)`; `None` when not computed (the count-distinct guard
    /// withheld it on a large result, recomputable on demand).
    pub distinct: Option<i64>,
    /// `min(col)`, valid for text (lexicographic) and numbers.
    pub min: Value,
    /// `max(col)`.
    pub max: Value,
    /// `sum(col)`: numeric columns only (`None` otherwise, or when every row is
    /// null).
    pub sum: Option<Value>,
    /// `avg(col)`, numeric columns only.
    pub avg: Option<Value>,
}

/// Which aggregates a [`ColumnStats`] request should compute. Bundles the two
/// independent toggles so the boolean pair can't be transposed at a call site
/// (`column_stats(sql, col, numeric, distinct)` → `column_stats(sql, col, flags)`).
/// `numeric` adds the `sum`/`avg` pair (decided UI-side from the column's declared
/// type); `distinct` adds the potentially expensive `count(distinct col)` (guarded
/// UI-side behind a row threshold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsFlags {
    /// Compute `sum(col)`/`avg(col)` (numeric columns only).
    pub numeric: bool,
    /// Compute `count(distinct col)`.
    pub distinct: bool,
}

/// One row of a foreign-key lookup list (the in-cell id picker): a referenced-table
/// row's identity value plus an optional human-readable label column, so the UI can
/// show `"<id> — <label>"` and let the user search/pick an existing id instead of
/// typing it. See `DatabaseDriver::fetch_lookup`.
#[derive(Debug, Clone, PartialEq)]
pub struct LookupRow {
    /// The referenced (id) column's value — what an FK edit actually writes.
    pub id: Value,
    /// A human label column's value (a name/title/…), when one was resolved;
    /// `None` means only the id is shown.
    pub label: Option<Value>,
}

/// Whether a declared column type is numeric (integer or floating/decimal). Drives
/// the column-stats `sum`/`avg` aggregates, which only make sense on a number, and
/// reuses the same base-token whitelists the key/edit-coercion paths use.
pub fn is_numeric_type(decl_type: Option<&str>) -> bool {
    decl_type.is_some_and(|t| is_int_type(t) || is_real_type(t))
}

/// A result-narrowing filter pushed into the query. The service wraps
/// the open's base SQL in `SELECT * FROM (base) WHERE <predicate>` *before* the
/// count / key-bounds probe, so the whole result (count, keyset seek, sort,
/// export) operates on the filtered set without ever materializing it. Because
/// the wrap preserves `SELECT *`, the key column survives and keyset paging is
/// unaffected.
///
/// The UI stays driver-independent by sending this *semantic* filter, not SQL: the
/// backend renders [`Contains`](Self::Contains) to a portable, escaped predicate
/// per engine (see `DatabaseDriver::contains_predicate`) and wraps
/// [`Where`](Self::Where) (power-user SQL, same trust level as the editor)
/// verbatim.
///
/// (`Eq` carries [`ColumnValue`], whose [`Value`] is `PartialEq` but not `Eq`, so
/// this enum is `PartialEq` only.)
#[derive(Debug, Clone, PartialEq)]
pub enum ResultFilter {
    /// "Any text-representable column contains this term": the portable quick
    /// filter. Rendered per engine to a case-insensitive `LIKE`/`ILIKE` OR-chain
    /// over the non-blob columns, with the term escaped to match literally.
    Contains(String),
    /// A raw boolean SQL expression, wrapped verbatim into the `WHERE`. For users
    /// who want a precise predicate; trusted like editor SQL.
    Where(String),
    /// A conjunction of `column = value` equalities (foreign-key follow):
    /// one [`ColumnValue`] for a single-column FK, several for a composite. Rendered
    /// per engine to an escaped *literal* predicate (`DatabaseDriver::eq_predicate`)
    /// AND-joined; comparison context coerces each literal to the column's type, so
    /// no cast is needed and the column stays index-usable. Built by the UI from an
    /// [`FkEdge`] + the followed row's values, never from raw SQL; NULL values are
    /// excluded by the caller (a null FK isn't followable).
    Eq(Vec<ColumnValue>),
    /// A conjunction of column comparisons the user *built* rather than typed: the
    /// "filter by this value" cell menu and the bar's Column mode. AND-joined and
    /// rendered per engine to an escaped literal predicate
    /// (`DatabaseDriver::cmp_predicate`), the same no-cast, index-usable shape as
    /// [`Eq`](Self::Eq) but across the full operator set. Structured, so the UI can
    /// show it as chips and add a term to it without re-parsing SQL — and so no
    /// user text ever reaches the query as SQL.
    Cmp(Vec<ColumnPredicate>),
}

/// How a [`ColumnPredicate`] compares its column to its value. Deliberately the
/// small, portable set every engine RED speaks renders identically; anything
/// richer is what [`ResultFilter::Where`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Pattern match. Unlike [`Contains`](Self::Contains), the value's `%`/`_` are
    /// **not** escaped: a `LIKE` term is a pattern the user wrote on purpose.
    Like,
    /// Substring match on one column: [`ResultFilter::Contains`]'s semantics
    /// (case-insensitive, the value's `%`/`_`/`\` escaped so it matches
    /// literally, the column cast to text) narrowed from "any column" to this
    /// one. The common data-grid ask — "rows where this column contains that" —
    /// without having to know that `%` is a wildcard.
    ///
    /// The cast is what makes it work on a non-text column ("id contains 42"),
    /// and is why this is the one operator here that gives up index usability. A
    /// substring search is a scan regardless.
    Contains,
    IsNull,
    IsNotNull,
}

impl CmpOp {
    /// The operators in picker order.
    pub const ALL: [CmpOp; 10] = [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
        CmpOp::Contains,
        CmpOp::Like,
        CmpOp::IsNull,
        CmpOp::IsNotNull,
    ];

    /// Whether the operator compares against a value at all. `IS NULL` /
    /// `IS NOT NULL` are unary, so their [`ColumnPredicate::value`] is `None`.
    pub fn takes_value(self) -> bool {
        !matches!(self, CmpOp::IsNull | CmpOp::IsNotNull)
    }

    /// Whether the operator only makes sense on a text-representable column.
    /// Drives the Column-mode picker, which narrows the operator list by the
    /// column's declared type (a bare `LIKE` against an integer column errors on
    /// Postgres, and the comparison operators carry no cast by design).
    /// [`Contains`](Self::Contains) is *not* text-only: it casts, so it works on
    /// any column.
    pub fn text_only(self) -> bool {
        matches!(self, CmpOp::Like)
    }

    /// Whether the operator orders its operands, so it's only offered on columns
    /// that have a meaningful order (numbers, dates, text).
    pub fn is_ordering(self) -> bool {
        matches!(self, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge)
    }
}

/// One `column <op> value` term of a [`ResultFilter::Cmp`]. A *semantic*
/// comparison carrying no SQL: the driver renders it, quoting the identifier and
/// escaping the value as a literal, so a value taken from a result cell (which on
/// a shared database is attacker-controlled) can never break out.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnPredicate {
    /// The result column's name, as it appears in the result (so an inline-expanded
    /// reference column's dotted name works too).
    pub column: String,
    pub op: CmpOp,
    /// The compared value. `None` exactly when [`CmpOp::takes_value`] is false.
    pub value: Option<Value>,
    /// The column's declared type, as the engine spells it, when the caller knows
    /// it. Carried for the same reason [`ColumnValue`] carries it: the *value*
    /// alone does not always say how to render the comparison. A Postgres
    /// `boolean` decodes as `Integer(0|1)`, and `"active" = 1` is a 42883 there
    /// because Postgres has no int→bool cast, so the renderer needs the column's
    /// type to write `= false` instead.
    pub decl_type: Option<String>,
}

/// A single guarded data edit, keyed on a result's row identity. Built by
/// the UI from the result's [`RowEditCaps::identity`] + base table; a *semantic* edit
/// carrying no SQL, so the UI stays engine-independent. The driver renders it to
/// dialect SQL, **binds** every value (never interpolates), and asserts it touches
/// exactly one row (rolling back otherwise). NULL values are emitted as the literal
/// `NULL` keyword by the renderer, so the per-engine value binders only ever see
/// non-null values.
///
/// The identity is a **vector**, not one column: a composite primary key is exactly
/// as sound a row address as a single one (it just needs a conjunction), and on
/// ClickHouse -- where nothing is unique -- the address is a snapshot of several
/// columns' values, checked by a preflight count before anything runs.
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Set one or more columns of the identified row.
    Update {
        table: TableRef,
        /// The identifying `column = value` pairs, AND-joined by the renderer. Never
        /// empty: an op with no identity would address the whole table.
        keys: Vec<ColumnValue>,
        set: Vec<ColumnValue>,
    },
    /// Delete the identified row.
    Delete {
        table: TableRef,
        keys: Vec<ColumnValue>,
    },
    /// Insert a row with the given column values; omitted columns take their DB
    /// default. After a successful insert the caller refetches to surface
    /// server-assigned values (autoincrement PK, defaults).
    Insert {
        table: TableRef,
        values: Vec<ColumnValue>,
    },
}

/// A (schema, name) table reference for an [`EditOp`]. `schema` is the namespace a
/// browse came from (`OpenResult.table`); the renderer qualifies and quotes it.
// Serializable because the persisted health report (`health::TableSize`) names its
// objects with one; nothing else on this type depends on serde.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

/// One `column = value` pair of an [`EditOp`]. `decl_type` is the column's declared
/// engine type (best-effort, `None` for a key/PK or where it isn't known): the
/// driver needs it to bind correctly into a non-text column. A value the driver
/// decoded to [`Value::Text`] (jsonb, timestamp, uuid, numeric, an enum) must be
/// *cast* back to that column type on write, because Postgres has no implicit
/// (assignment) cast from `text` to those, so a bare text bind is rejected. Keys
/// (always int/text, see `PkKey`) bind fine without it, so they carry `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnValue {
    pub column: String,
    pub value: Value,
    pub decl_type: Option<String>,
}

impl EditOp {
    /// The table this op writes to, whichever kind it is.
    pub fn table(&self) -> &TableRef {
        match self {
            EditOp::Update { table, .. }
            | EditOp::Delete { table, .. }
            | EditOp::Insert { table, .. } => table,
        }
    }

    /// The human verb for the confirm modal ("Update" / "Delete" / "Insert").
    pub fn verb(&self) -> &'static str {
        match self {
            EditOp::Update { .. } => "Update",
            EditOp::Delete { .. } => "Delete",
            EditOp::Insert { .. } => "Insert",
        }
    }

    /// A readable, **display-only** rendering of the statement, values inlined as
    /// literals; what the confirm modal shows. This is *not* what executes: the
    /// driver renders dialect SQL and binds the values as parameters. Quoting is
    /// generic (double-quoted identifiers); the live statement uses the engine's.
    pub fn preview_sql(&self) -> String {
        let q = |id: &str| format!("\"{}\"", id.replace('"', "\"\""));
        let qualify = |t: &TableRef| match &t.schema {
            Some(s) if !s.is_empty() => format!("{}.{}", q(s), q(&t.name)),
            _ => q(&t.name),
        };
        // The identity conjunction, `IS NULL` for a null value (a `= NULL` would
        // match nothing, which is the opposite of what the row's value means).
        let where_clause = |keys: &[ColumnValue]| {
            keys.iter()
                .map(|cv| match &cv.value {
                    Value::Null => format!("{} IS NULL", q(&cv.column)),
                    v => format!("{} = {}", q(&cv.column), literal(v)),
                })
                .collect::<Vec<_>>()
                .join(" AND ")
        };
        match self {
            EditOp::Update { table, keys, set } => {
                let assigns = set
                    .iter()
                    .map(|cv| format!("{} = {}", q(&cv.column), literal(&cv.value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "UPDATE {} SET {} WHERE {}",
                    qualify(table),
                    assigns,
                    where_clause(keys)
                )
            }
            EditOp::Delete { table, keys } => format!(
                "DELETE FROM {} WHERE {}",
                qualify(table),
                where_clause(keys)
            ),
            EditOp::Insert { table, values } => {
                let cols = values
                    .iter()
                    .map(|cv| q(&cv.column))
                    .collect::<Vec<_>>()
                    .join(", ");
                let vals = values
                    .iter()
                    .map(|cv| literal(&cv.value))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    qualify(table),
                    cols,
                    vals
                )
            }
        }
    }
}

/// One background mutation the engine is applying, or has finished applying (see
/// `DatabaseDriver::mutations`). ClickHouse's `system.mutations`, generalised.
///
/// This exists because a best-effort edit doesn't end when the submit returns. An
/// `ALTER TABLE … UPDATE` rewrites every part its predicate can touch, which on a
/// production-sized table can run for minutes after the UI said "submitted"; without
/// somewhere to watch it, the only feedback is a grid that hasn't changed yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationInfo {
    pub database: String,
    pub table: String,
    /// The engine's handle for this mutation, and what a kill addresses it by.
    pub id: String,
    /// The statement it is applying, verbatim.
    pub command: String,
    /// When the engine accepted it, as the engine renders it.
    pub created: String,
    /// Parts still to rewrite. The honest progress signal: it counts *parts*, not
    /// rows, which is also why a one-cell edit can have a large number here.
    pub parts_to_do: i64,
    pub done: bool,
    /// Why it is stuck, when it is. `None` while healthy.
    pub fail_reason: Option<String>,
}

/// One unit of work a database server is doing for some client: a running
/// statement, an idle-in-transaction session, a waiting backend.
///
/// The SQL twin of Redis's `CLIENT LIST` row. Named *session* rather than
/// *activity* because [`ActivityKind`] is already the AI agent's timeline and
/// the two would be indistinguishable in a protocol import list.
// No `Eq`: `elapsed_secs` is a float.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSession {
    /// What a kill addresses this session by, engine-native (a Postgres backend
    /// pid, a MySQL thread id, a ClickHouse query_id). Carried as text because
    /// the three are not the same type and only the driver that minted one ever
    /// interprets it.
    pub key: SessionKey,
    pub user: Option<String>,
    pub application: Option<String>,
    pub client_addr: Option<String>,
    pub database: Option<String>,
    /// The engine's own word for what this session is doing ("active", "idle in
    /// transaction", "Sending data"), verbatim: translating it would lose the
    /// distinctions an operator is looking for.
    pub state: String,
    /// What it is waiting on, when it is waiting. `None` while running.
    pub wait: Option<String>,
    /// Sessions blocking this one, for the wait tree. Empty when free.
    pub blocked_by: Vec<SessionKey>,
    /// The statement, as the server reports it. `None` when the connected role
    /// may not see it, which is normal rather than an error.
    pub query: Option<String>,
    /// Seconds since the statement started, computed **server-side** so a wrong
    /// clock on this machine cannot render a negative duration.
    pub elapsed_secs: f64,
    /// This is RED's own connection. Never offered a kill: shooting it produces a
    /// reconnect dance and no useful outcome.
    pub is_self: bool,
}

/// A server-side session handle. Opaque text; only the driver that produced one
/// parses it back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(pub String);

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How hard to stop a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillMode {
    /// Stop the running statement, keep the session (`pg_cancel_backend`,
    /// `KILL QUERY`). The reversible one.
    Cancel,
    /// Drop the whole session (`pg_terminate_backend`, `KILL`). Rolls back an
    /// open transaction, so it is the strictly heavier hammer and is graded that
    /// way by the confirmation ladder.
    Terminate,
}

impl KillMode {
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel query",
            Self::Terminate => "Terminate session",
        }
    }
}

/// What a server lets RED see and do about its sessions. Another capability
/// descriptor, for the same reason [`DbKind::write_caps`] is one: the UI holds a
/// [`DbKind`] and never a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCaps {
    /// Whether there is a server with other sessions at all. `false` hides the
    /// panel outright (SQLite is a file).
    pub supported: bool,
    pub can_cancel: bool,
    pub can_terminate: bool,
    /// Whether `blocked_by` is ever populated, so the UI knows a flat list is the
    /// engine's answer rather than "nothing is blocked right now".
    pub has_wait_graph: bool,
}

/// How a batch of [`EditOp`]s should be applied. The two contracts are genuinely
/// different promises, not a strictness dial, so the caller names which one it wants
/// and the driver answers with the matching shape (one total, or one outcome per op).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchMode {
    /// One transaction, exactly one row per op, rolled back whole on any failure:
    /// `DatabaseDriver::apply_edits`. What the relational engines do.
    Atomic,
    /// Per-op, non-atomic, preflight-guarded: `apply_edits_best_effort`. ClickHouse.
    BestEffort {
        /// The user saw "this matches N rows" in the confirm dialog and said to apply
        /// it to all of them. Without this a preflight count other than 1 is refused,
        /// which is the whole guard against writing the wrong row on an engine that
        /// has no unique row address.
        allow_multi_match: bool,
    },
}

/// What one op of a batch *would* do, for the confirm dialog: the real statement the
/// driver will run and how many rows its identity currently matches. Produced by
/// `DatabaseDriver::preflight_edits` before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpPlan {
    /// Position in the submitted batch, so the UI can point at the staged change.
    pub index: usize,
    /// `"Update"` / `"Delete"` / `"Insert"` (see [`EditOp::verb`]).
    pub verb: &'static str,
    /// The statement as it will actually run, in the engine's own dialect, values
    /// shown inline. Display only -- the executed statement binds them.
    pub sql: String,
    /// Rows the identity predicate matches right now. `None` when the op addresses no
    /// existing row (an insert) or the driver didn't count.
    pub matches: Option<u64>,
    /// Why this op can't run at all, when it can't: a key-column edit, a row whose
    /// identity isn't comparable, a table the engine won't mutate. Blocked ops are
    /// shown and skipped, never submitted and failed.
    pub blocked: Option<String>,
}

/// What actually happened to one op of a best-effort batch. Unlike the atomic path's
/// single total, every op reports for itself, because on an engine with no
/// transaction "3 of 5 applied" is the truth and a blanket success would not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpOutcome {
    pub index: usize,
    pub verb: &'static str,
    pub status: OpStatus,
}

/// The result of one op in a best-effort batch (see [`OpOutcome`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpStatus {
    /// Ran, and the engine confirmed it: `affected` rows changed.
    Applied { affected: u64 },
    /// Accepted by the engine and still running: an asynchronous mutation that
    /// outlived the statement timeout. **Not** an error -- the work is queued and
    /// will land -- so the UI must not re-arm the submit, since a retry is a second
    /// full part rewrite.
    Submitted,
    /// Refused before running, by the preflight rather than the engine: the row moved
    /// or vanished, the identity matched more than one row, or the op was never
    /// runnable. Nothing was written.
    Blocked(String),
    /// The engine rejected it. Nothing was written *for this op*; earlier ops in the
    /// batch may already have landed (there is no transaction to undo them).
    Failed(String),
}

impl OpStatus {
    /// Whether the op left the batch's work undone, so the UI keeps it staged.
    pub fn unfinished(&self) -> bool {
        matches!(self, OpStatus::Blocked(_) | OpStatus::Failed(_))
    }

    /// The reason an op didn't land, for the report. `None` when it did.
    pub fn reason(&self) -> Option<&str> {
        match self {
            OpStatus::Blocked(r) | OpStatus::Failed(r) => Some(r),
            _ => None,
        }
    }
}

/// Render a [`Value`] as a SQL literal for an [`EditOp`] **preview only**, quoting
/// text with embedded quotes doubled. Never used to build executed SQL (the driver
/// binds values), so it's a readability helper, not an injection surface.
fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        Value::Capped(c) if c.blob => format!("<{} bytes>", c.len),
        Value::Capped(c) => format!("'{}…'", c.head.replace('\'', "''")),
    }
}

/// Coerce user-entered `text` to a [`Value`] for an edit, guided by the column's
/// declared type. An empty string maps to [`Value::Null`] (clearing a cell). Numeric
/// columns parse to [`Value::Integer`]/[`Value::Real`]; a parse failure is an
/// `Err(reason)` the editor shows inline, so the preview never opens with an
/// un-bindable value. Everything else is [`Value::Text`].
pub fn coerce_edit_value(
    text: &str,
    decl_type: Option<&str>,
) -> std::result::Result<Value, String> {
    if text.is_empty() {
        return Ok(Value::Null);
    }
    if decl_type.is_some_and(is_int_type) {
        return text
            .trim()
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| format!("‘{text}’ is not a valid integer"));
    }
    if decl_type.is_some_and(is_real_type) {
        return text
            .trim()
            .parse::<f64>()
            .map(Value::Real)
            .map_err(|_| format!("‘{text}’ is not a valid number"));
    }
    // An exact numeric is still validated as a number - a typo must be caught at
    // the editor, not by the engine after the preview - but it is *carried* as
    // text, because `f64` cannot hold what the column can. See
    // [`is_exact_numeric_type`].
    if decl_type.is_some_and(is_exact_numeric_type) {
        let trimmed = text.trim();
        if trimmed.parse::<f64>().is_err() {
            return Err(format!("‘{text}’ is not a valid number"));
        }
        return Ok(Value::Text(Arc::from(trimmed)));
    }
    Ok(Value::Text(Arc::from(text)))
}

/// A query execution plan (EXPLAIN). A small, fully-materialized tree
/// the driver builds from the engine's *native* EXPLAIN output (SQLite's
/// `EXPLAIN QUERY PLAN` rows, Postgres's indented text plan, MySQL's `FORMAT=TREE`)
/// so the UI renders it readably without any engine knowledge. A plan is
/// inherently tiny and bounded, so unlike a result set it is held whole; the
/// "never materialize" rule targets row data, not a fixed-size plan.
///
/// Metrics stay *engine-named* on purpose: SQLite reports none, Postgres reports
/// `cost`/`rows`/`width` (plus `actual time`/`rows`/`loops` under ANALYZE), MySQL
/// its own. We render whatever the engine names rather than inventing a false
/// cross-engine cost unit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryPlan {
    /// The plan's root operations, usually one. A SQLite plan is a shallow tree
    /// of steps; a Postgres/MySQL plan nests.
    pub nodes: Vec<PlanNode>,
    /// The engine's verbatim EXPLAIN text: the "Copy plan" payload and the
    /// guaranteed fallback the UI shows when `nodes` is empty (an exotic plan the
    /// structural parse couldn't tree).
    pub raw: String,
    /// `true` iff this came from `EXPLAIN ANALYZE`; actual-row/time metrics are
    /// present and the UI may tint the costliest node. SQLite has no ANALYZE.
    pub analyzed: bool,
}

/// One operation in a [`QueryPlan`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// The operation line, e.g. `"Seq Scan on users"` / `"SEARCH t USING INDEX ix"`.
    pub label: String,
    /// Engine extras attached to this node (a filter, index condition, join key),
    /// joined for display. `None` when the node carries none.
    pub detail: Option<String>,
    /// Engine-named metrics rendered as-is: `("cost", "0.00..18.50")`,
    /// `("rows", "850")`, `("actual time", "0.011..0.012")`. Empty for SQLite.
    pub metrics: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// A leaf node with just a label: the building block the parsers grow.
    pub fn leaf(label: impl Into<String>) -> PlanNode {
        PlanNode {
            label: label.into(),
            detail: None,
            metrics: Vec::new(),
            children: Vec::new(),
        }
    }
}

/// Whether a declared type names an integer column across the three engines
/// (`INTEGER`, `bigint`, `int(11)`, `serial`, …). Deliberately a whitelist of
/// the base token: `interval`/`point` must not match.
fn is_int_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "int"
            | "integer"
            | "int2"
            | "int4"
            | "int8"
            | "tinyint"
            | "smallint"
            | "mediumint"
            | "bigint"
            | "serial"
            | "smallserial"
            | "bigserial"
    )
}

/// Whether a declared type is an **approximate** (binary floating point) numeric
/// column, for edit-value coercion (parse to [`Value::Real`]). Best-effort across
/// the engines.
///
/// `numeric`/`decimal` are deliberately absent: they are *exact* types, and `f64`
/// is not. Coercing them here silently rewrote every edited value that did not
/// happen to be representable in binary floating point - a price of `19.99` became
/// `19.989999999999998`, and a 38-digit identifier lost its tail - with the engine
/// storing the rounded number without complaint. [`is_exact_numeric_type`] routes
/// them to [`Value::Text`] instead, which every driver binds or casts back to the
/// column's own type with no round trip through a float.
fn is_real_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(base, "real" | "double" | "float" | "float4" | "float8")
}

/// Whether a declared type is an exact fixed-point numeric column, whose value can
/// hold more precision than an `f64` and must not pass through one. See
/// [`is_real_type`].
fn is_exact_numeric_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "numeric" | "decimal" | "dec" | "money" | "smallmoney" | "number" | "bigdecimal"
    ) || base.starts_with("decimal")
}

/// One bounded window of rows pulled from a streaming cursor. The streaming path
/// never materializes a whole result; it yields these fixed-size windows.
#[derive(Debug, Clone, Default)]
pub struct RowWindow {
    pub rows: Vec<Vec<Value>>,
    /// `true` once this window reaches the end of the result (no more fetches).
    pub exhausted: bool,
}

/// One random-access page of a result, fetched by `(offset, limit)`. Backs the
/// result grid's load-on-scroll: a bounded window buffer requests the pages
/// around the viewport and evicts the rest, so memory stays flat over a
/// multi-million-row result. Columns ride along (stable, but cheap to repeat).
#[derive(Debug, Clone, Default)]
pub struct ResultPage {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

/// A streamed-export target format. The driver writes rows straight to disk
/// row-by-row, never materializing the whole result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    /// A themed, self-contained HTML report: a standalone document (inline CSS,
    /// light/dark via `prefers-color-scheme`) opened in the system browser, not a
    /// data interchange file. Streamed row-by-row like the others.
    Html,
    /// A stream of `INSERT INTO "table" (...) VALUES (...);` statements, one per
    /// row. The table name is derived from the destination file stem; identifiers
    /// and string literals use portable ANSI quoting (double / single quotes).
    Sql,
    /// An Excel workbook (`.xlsx`): one sheet, inline strings, no styles. Written
    /// as a ZIP of uncompressed parts with the sheet spooled to disk, so a large
    /// result never lands in memory. The file is bigger than the CSV of the same
    /// data (nothing is compressed) and stops at Excel's 1,048,576-row limit,
    /// reporting the truncation rather than dropping rows silently.
    Xlsx,
}

/// How one statement of a script run ended.
///
/// A script reports per statement rather than as one number, because the whole
/// point of running several statements together is knowing *which* one failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// Ran and changed `affected` rows (0 for DDL and for a read).
    Ok { affected: u64 },
    /// Ran and returned rows. The count is deliberately absent: counting would
    /// mean draining the result, which is exactly the whole-result
    /// materialization the streaming invariant forbids.
    Rows,
    /// The engine rejected it. Carries the message so the log can name the cause
    /// beside the statement that caused it.
    Failed { error: String },
    /// Not attempted, because an earlier statement failed under
    /// [`ScriptStop::OnError`].
    Skipped,
}

impl ScriptOutcome {
    /// Whether this outcome should stop a script that stops on error.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// What a script does when a statement fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptStop {
    /// Stop at the first failure, leaving the rest [`ScriptOutcome::Skipped`].
    /// The default: a migration whose third statement failed rarely wants its
    /// fourth applied on top.
    OnError,
    /// Run every statement regardless, reporting each. For a script of
    /// independent statements where one bad line should not block the rest.
    Never,
}

/// One statement's line in a script run's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptStep {
    /// Position in the script, 0-based, so the UI can say "statement 3 of 12".
    pub index: usize,
    /// A short single-line form of the statement, for the log. Truncated at the
    /// source rather than in the UI so a megabyte-long generated `INSERT` never
    /// crosses the channel just to be shortened on arrival.
    pub summary: String,
    pub outcome: ScriptOutcome,
}

/// How wide a script step's `summary` is cut. Long enough to identify the
/// statement, short enough that a thousand-step script's log stays bounded.
pub const SCRIPT_SUMMARY_MAX: usize = 120;

/// Cut `sql` down to a single-line [`ScriptStep::summary`]: collapse all
/// whitespace runs to one space, then clip to [`SCRIPT_SUMMARY_MAX`] on a char
/// boundary with an ellipsis.
pub fn script_summary(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len().min(SCRIPT_SUMMARY_MAX + 1));
    let mut space = false;
    for ch in sql.trim().chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if out.chars().count() >= SCRIPT_SUMMARY_MAX {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// What a finished export actually wrote.
///
/// Its reason for existing is [`shortfall`](Self::shortfall): an export that
/// could not hold the whole result must say so. Returning a bare row count would
/// make truncation indistinguishable from a result that happened to be that
/// size, which is the one behaviour an export must never have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    /// Data rows (or keys) actually written to the file.
    pub rows: u64,
    /// `None` when the file holds everything that was asked for.
    pub shortfall: Option<ExportShortfall>,
}

impl ExportOutcome {
    /// A complete export of `rows` rows.
    pub fn complete(rows: u64) -> ExportOutcome {
        ExportOutcome {
            rows,
            shortfall: None,
        }
    }
}

/// Why an export holds less than was asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportShortfall {
    /// The format's own limit stopped it short: XLSX's 1,048,576-row sheet. The
    /// source has more rows than the file holds.
    RowLimit,
    /// Keys that are not in the file: one the chosen format cannot represent (a
    /// Redis Commands export of a binary value, which only the DUMP format
    /// round-trips), one too large to serialise whole, or one deleted while the
    /// export ran.
    SkippedKeys(u64),
    /// Documents carried fields outside the tabular export's column list. The
    /// columns come from a *sample* of a schemaless collection, so a later
    /// document can always hold a field the sample never saw; the rows are still
    /// written, minus those fields.
    UnmappedFields(u64),
    /// Redis lists longer than the export's read window were written only as far
    /// as that window reached. A list pages from its ends only, so reading past
    /// the window costs a deep `LRANGE` whose price grows with the offset -
    /// the file is short, and saying so is the whole point of this arm.
    TruncatedLists { keys: u64, window: usize },
}

impl ExportShortfall {
    /// The sentence the export toast appends, so every surface words the same
    /// shortfall identically.
    pub fn note(&self) -> String {
        match self {
            ExportShortfall::RowLimit => {
                "stopped at Excel's sheet limit; the result has more rows".to_string()
            }
            ExportShortfall::SkippedKeys(n) => format!(
                "{n} key(s) are not in the file: a binary value this format cannot carry (use \
                 the DUMP format for those), a key too large to serialise whole, or one deleted \
                 while the export ran."
            ),
            ExportShortfall::UnmappedFields(n) => format!(
                "{n} document(s) held fields outside the sampled columns, and those fields are \
                 not in the file. Export as JSON or NDJSON to keep every field."
            ),
            ExportShortfall::TruncatedLists { keys, window } => format!(
                "{keys} list(s) were longer than {window} elements and are in the file only up \
                 to that point. Use the DUMP format, which copies a list whole."
            ),
        }
    }
}

/// A streamed-import source format, the read-side mirror of [`ExportFormat`]. The
/// reader yields one row of raw text cells at a time, never materializing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Csv,
    /// Newline-delimited JSON: one JSON object per line.
    Jsonl,
    /// A single top-level JSON array of objects (`[ {…}, {…} ]`), read one element
    /// at a time so the whole file is never materialized.
    JsonArray,
}

/// One column mapping for a data import / copy: which **source** cell index feeds
/// which **target** column (carrying the column's best-effort declared type, used to
/// coerce the text cell and to cast the bound parameter). Built UI-side from the
/// file header + the target table's columns; the dispatch import loop reads it to
/// project each source row into target-column order.
// Serializable so a saved [`transfer::TransferPlan`] can pin an explicit
// projection; nothing on the live path depends on serde.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ColumnMap {
    pub source: usize,
    pub column: String,
    pub decl_type: Option<String>,
}

/// Per-query knobs carried UI → service → driver.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// Max rows per fetched window. Read by the streaming cursor (`open_cursor`)
    /// to size its row channel.
    pub window: usize,
    /// Abort a single streamed fetch that stalls longer than this; guards a
    /// runaway query that computes a huge intermediate before yielding row 1.
    /// Enforced by the *service*, not the driver: the dispatch loop races the
    /// cursor's `next_window` against this deadline and fires the engine cancel on
    /// expiry (see `drive_fetch`). The windowed `OpenResult` path uses the global
    /// `statement_timeout` instead. `None` = no cap.
    pub timeout: Option<std::time::Duration>,
    /// Read every cell at **full fidelity**, never the display fat-cell cap. The
    /// interactive cursor caps long text/blobs for paint performance
    /// (`Value::Capped`), which is right for the grid but **data loss** for a copy:
    /// the table-copy read sets this so a long `TEXT`/blob round-trips byte-exact
    /// into the target (the same invariant export holds via `PageCap::Full`). The
    /// default is `false`; the grid's streaming path stays capped.
    pub full_fidelity: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            window: 1000,
            timeout: None,
            full_fidelity: false,
        }
    }
}

/// How a table copy writes into its target ([`crate`]'s `CopyToTable`). Append is
/// the default "add these rows"; `TruncateInsert` clears the target first (behind
/// the destructive confirm): "refresh this table from the source". Upsert/merge is
/// deliberately **not** here: it needs a per-engine conflict-key seam and a key
/// picker, which is the on-ramp to the sync machinery table-copy exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum CopyMode {
    /// Insert the source rows into the target as-is (the target keeps its rows).
    Append,
    /// Clear the target (`DELETE FROM`) before inserting: a full refresh.
    TruncateInsert,
}

/// The error type that crosses every RED layer boundary.
#[derive(Debug, thiserror::Error)]
pub enum RedError {
    #[error("connection failed: {0}")]
    Connect(String),
    /// A connect attempt the user must fix before retrying makes sense: bad
    /// credentials, a missing database, an unknown role. Distinct from
    /// [`RedError::Connect`] (a transient/network failure) so the UI can stop the
    /// backoff loop and prompt for an edit instead of retrying forever.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// An SSH jump host whose key isn't in `~/.ssh/known_hosts`. Distinct from
    /// [`RedError::Auth`] so the UI can offer "trust this host & retry" instead of
    /// a dead end: it carries the fingerprint to show and the OpenSSH-encoded key
    /// to append to `known_hosts` on accept.
    #[error("unknown SSH host key for {host}")]
    SshHostUnknown {
        host: String,
        port: u16,
        fingerprint: String,
        key: String,
    },
    /// The connection died under an operation that had already been established:
    /// the server closed an idle connection, or the link was reset (a NAT/firewall
    /// timeout, the machine sleeping, a server restart). Distinct from
    /// [`RedError::Driver`] because it says nothing about the work itself - the
    /// statement never ran, or was cut off mid-flight - so a caller can re-dial and
    /// retry instead of reporting a failure the user cannot act on.
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("query failed: {0}")]
    Query(String),
    /// The query names no database and the session has none bound, so the engine
    /// refused it (MySQL error 1046, `ER_NO_DB_ERROR`). Distinct from
    /// [`RedError::Query`] because it is not a defect in the SQL: the user need
    /// only pick a namespace, so the UI answers with a picker instead of a dead
    /// end. `label` is the engine's word for the level ("Database"/"Schema", from
    /// [`NamespaceCaps::label`]) so the message reads right per engine.
    #[error("no {label} selected for this query")]
    NamespaceRequired { label: &'static str },
    #[error("driver error: {0}")]
    Driver(String),
    /// A fetch was aborted out-of-band (user cancel). Distinct from a failure so
    /// the service can emit a clean "cancelled" rather than an error.
    #[error("query cancelled")]
    Interrupted,
    /// A fetch exceeded its configured timeout.
    #[error("query timed out")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, RedError>;

/// The self-updater's lifecycle, surfaced from the backend to the UI titlebar
/// pill + About tab. macOS-only today;
/// on other platforms the updater never advances past `Unknown`. A run is
/// "stage on disk, apply on restart": the new bundle is fully swapped over the
/// installed app *before* `ReadyToRestart`, so a restart is just a relaunch.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum UpdateState {
    /// No check has completed this session (the initial state, and while updates
    /// are disabled).
    #[default]
    Unknown,
    /// A check is in flight.
    Checking,
    /// The running build is the latest published release.
    UpToDate { current: String },
    /// A newer release is downloading/staging over the installed app. `pct` is a
    /// coarse 0–100 progress hint.
    Downloading { version: String, pct: u8 },
    /// A newer build is fully staged; clicking the pill relaunches into it.
    ReadyToRestart { version: String },
    /// The check or staging failed; `reason` is user-facing.
    Failed { reason: String },
    /// A newer release exists but RED can't self-swap (not running from a
    /// writable `/Applications/RED.app`: a dev build, Homebrew, a read-only
    /// volume). `url` links the GitHub release for a manual download.
    Unsupported { version: String, url: String },
}

#[cfg(test)]
mod script_tests {
    use super::*;

    /// A summary is one line: every whitespace run (including the newlines a
    /// formatted statement is full of) collapses to a single space.
    #[test]
    fn summary_collapses_whitespace_to_one_line() {
        assert_eq!(
            script_summary("SELECT *\n  FROM   users\n  WHERE id = 1"),
            "SELECT * FROM users WHERE id = 1"
        );
        assert_eq!(
            script_summary("   \n  UPDATE t SET x=1  \n "),
            "UPDATE t SET x=1"
        );
        assert_eq!(script_summary(""), "");
    }

    /// A generated statement far past the cap is clipped, so a script's log stays
    /// bounded no matter what the buffer holds.
    #[test]
    fn summary_clips_a_long_statement() {
        let long = format!("INSERT INTO t VALUES {}", "x".repeat(500));
        let summary = script_summary(&long);
        assert_eq!(summary.chars().count(), SCRIPT_SUMMARY_MAX + 1);
        assert!(summary.ends_with('…'));
    }

    /// Clipping counts characters, not bytes, so a multi-byte statement is never
    /// cut mid-character (which would not even be a `String`).
    #[test]
    fn summary_clips_on_a_char_boundary() {
        let long = "é".repeat(500);
        let summary = script_summary(&long);
        assert_eq!(summary.chars().count(), SCRIPT_SUMMARY_MAX + 1);
    }

    /// Only a failure stops a script that stops on error; a read among the
    /// statements is a normal outcome.
    #[test]
    fn only_failures_stop_a_script() {
        assert!(
            ScriptOutcome::Failed {
                error: "boom".into()
            }
            .is_failure()
        );
        assert!(!ScriptOutcome::Ok { affected: 0 }.is_failure());
        assert!(!ScriptOutcome::Rows.is_failure());
        assert!(!ScriptOutcome::Skipped.is_failure());
    }
}

#[cfg(test)]
#[cfg(test)]
mod value_size_tests {
    use super::*;

    /// A resident result window holds one `Value` per cell (rows × columns), so the
    /// enum's width is paid on every cell — including the common `Integer`/`Text`.
    /// Keep it at the `String`/`Vec` width by boxing the cold `Capped` variant; if
    /// someone inlines a fat field again this guard fails rather than silently
    /// widening every cell. `CappedCell` itself is deliberately larger (40 bytes).
    #[test]
    fn value_stays_pointer_width() {
        assert!(
            std::mem::size_of::<Value>() <= 4 * std::mem::size_of::<usize>(),
            "Value grew to {} bytes; a fat variant is inlined — box it",
            std::mem::size_of::<Value>()
        );
        assert!(std::mem::size_of::<CappedCell>() > std::mem::size_of::<Value>());
    }
}

#[cfg(test)]
mod tx_tests {
    use super::*;

    /// Only the engines whose drivers implement `begin_sandbox` claim
    /// transactions. ClickHouse has no `BEGIN`, so offering the affordance could
    /// only ever produce a refusal.
    #[test]
    fn only_transactional_engines_claim_transactions() {
        assert!(DbKind::Postgres.supports_transactions());
        assert!(DbKind::Mysql.supports_transactions());
        assert!(DbKind::Sqlite.supports_transactions());
        assert!(!DbKind::Clickhouse.supports_transactions());
    }

    /// The non-SQL seams have no SQL session to hold a transaction on, so they
    /// must never advertise one either.
    #[test]
    fn non_sql_seams_have_no_transactions() {
        assert!(!DbKind::Redis.supports_transactions());
        assert!(!DbKind::Mongo.supports_transactions());
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn cv(column: &str, value: Value) -> ColumnValue {
        ColumnValue {
            column: column.into(),
            value,
            decl_type: None,
        }
    }

    #[test]
    fn preview_renders_readable_statements() {
        let table = TableRef {
            schema: Some("main".into()),
            name: "users".into(),
        };
        let update = EditOp::Update {
            table: table.clone(),
            keys: vec![cv("id", Value::Integer(7))],
            set: vec![cv("name", Value::Text("O'Brien".into()))],
        };
        // Identifiers double-quoted, the value single-quoted with the quote doubled.
        assert_eq!(
            update.preview_sql(),
            "UPDATE \"main\".\"users\" SET \"name\" = 'O''Brien' WHERE \"id\" = 7"
        );
        assert_eq!(update.verb(), "Update");

        let del = EditOp::Delete {
            table: table.clone(),
            keys: vec![cv("id", Value::Integer(7))],
        };
        assert_eq!(
            del.preview_sql(),
            "DELETE FROM \"main\".\"users\" WHERE \"id\" = 7"
        );

        // A composite identity is a conjunction; a null member compares with
        // `IS NULL`, since `= NULL` would match nothing.
        let composite = EditOp::Delete {
            table: table.clone(),
            keys: vec![
                cv("tenant", Value::Text("acme".into())),
                cv("id", Value::Integer(7)),
                cv("region", Value::Null),
            ],
        };
        assert_eq!(
            composite.preview_sql(),
            "DELETE FROM \"main\".\"users\" \
             WHERE \"tenant\" = 'acme' AND \"id\" = 7 AND \"region\" IS NULL"
        );

        let ins = EditOp::Insert {
            table,
            values: vec![cv("id", Value::Integer(2)), cv("name", Value::Null)],
        };
        assert_eq!(
            ins.preview_sql(),
            "INSERT INTO \"main\".\"users\" (\"id\", \"name\") VALUES (2, NULL)"
        );
    }

    #[test]
    fn coercion_follows_declared_type() {
        // Empty input clears the cell regardless of type.
        assert_eq!(coerce_edit_value("", Some("text")), Ok(Value::Null));
        // Integer / real columns parse; everything else is text.
        assert_eq!(
            coerce_edit_value("42", Some("integer")),
            Ok(Value::Integer(42))
        );
        assert_eq!(
            coerce_edit_value("3.5", Some("double precision")),
            Ok(Value::Real(3.5))
        );
        assert_eq!(
            coerce_edit_value("hi", Some("varchar(20)")),
            Ok(Value::Text("hi".into()))
        );
        // An untyped column keeps the text verbatim.
        assert_eq!(coerce_edit_value("x", None), Ok(Value::Text("x".into())));
        // A non-numeric value on an integer column is a coercion error (no preview).
        assert!(coerce_edit_value("abc", Some("int")).is_err());
    }

    /// An exact numeric column holds more precision than an `f64`, so routing an
    /// edit through one silently rewrote the value: `19.99` stored as
    /// `19.989999999999998`, a long identifier truncated to 17 digits. It stays
    /// text, and the driver casts it back to the column's own type.
    #[test]
    fn an_exact_numeric_edit_keeps_every_digit() {
        for decl in ["numeric", "numeric(38,10)", "decimal(10,2)", "DEC", "money"] {
            assert_eq!(
                coerce_edit_value("19.99", Some(decl)),
                Ok(Value::Text("19.99".into())),
                "{decl} must not round-trip through f64"
            );
        }
        // The digits an `f64` cannot represent survive verbatim.
        let long = "123456789012345678901234567890.123456789";
        assert_eq!(
            coerce_edit_value(long, Some("numeric(39,9)")),
            Ok(Value::Text(long.into()))
        );
        // Still validated: a typo is caught in the editor, not by the engine.
        assert!(coerce_edit_value("nineteen", Some("numeric")).is_err());
        // Surrounding whitespace is trimmed, as it is for the other numeric arms.
        assert_eq!(
            coerce_edit_value("  1.5  ", Some("decimal")),
            Ok(Value::Text("1.5".into()))
        );
    }
}
