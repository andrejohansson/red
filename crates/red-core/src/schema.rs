//! The catalog: what a database *contains*, as the schema tree, the result grid
//! and the copy/migration paths need to see it.
//!
//! Two halves. The **introspection** types ([`SchemaMeta`], [`ObjectMeta`],
//! [`TableDetail`], [`ColumnMeta`], [`IndexMeta`], [`ForeignKeyMeta`]) are what a
//! driver reports from the engine's own catalog. The **navigation** types
//! ([`KeySpec`], [`FkEdge`], [`FkJoin`]) are what RED derives from that so a
//! result can be paged by keyset rather than by `OFFSET`, and so a foreign key can
//! be followed in either direction.

use super::is_int_type;

/// What a schema object is.
///
/// Split into two tiers by [`ObjectKind::is_relation`]. **Relations** (table,
/// view, materialized view) have columns, can be selected from, and are what the
/// connect-time tree skeleton carries. **Programmatic objects** (routines,
/// triggers, sequences, types) have no columns and are loaded lazily when their
/// group node is expanded, so widening this enum did not make connecting slower
///.
///
/// Which of these an engine has at all is [`DbKind::object_kinds`](crate::DbKind::object_kinds); a kind absent
/// from that list is never queried for and never drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
    Trigger,
    Sequence,
    /// A user-defined type: a Postgres enum or composite. Engines without one
    /// simply never report it.
    Type,
}

impl ObjectKind {
    /// Has columns: can be described, selected from, browsed, diffed, and drawn
    /// in an ER diagram.
    ///
    /// This is the predicate the read-side call sites want. It is deliberately
    /// **not** what a write target should ask: a view is a relation but is not
    /// insertable, so the copy/migrate paths keep spelling
    /// `matches!(kind, ObjectKind::Table)` themselves. Widening a read filter is
    /// additive; widening a write filter silently offers a procedure as an
    /// INSERT target.
    pub const fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MaterializedView)
    }

    /// Loaded on expand rather than at connect. The inverse of
    /// [`is_relation`](Self::is_relation), named for the call site that cares
    /// (the tree's group nodes) rather than for the negation.
    pub const fn is_lazy(self) -> bool {
        !self.is_relation()
    }

    /// Whether an object of this kind is replaced *wholesale* — dropped and created
    /// again from one full `CREATE` statement — rather than altered in place. This is
    /// the predicate that makes an editable definition tractable: the edited text *is*
    /// the whole object, so applying it needs no comparison of before against after,
    /// and therefore no SQL parser (`crate::sql` is a lexer by design). A table is the
    /// counterexample and is deliberately excluded: changing one means `ALTER`, which
    /// means diffing two shapes — a structured editor over the catalog, which is its
    /// own feature. Three kinds are droppable but
    /// still excluded, because the drop is not harmless: a materialized view has stored
    /// rows to lose, a sequence has column defaults drawing from its counter, and a
    /// type cannot be dropped while a column still uses it.
    pub const fn is_replaceable(self) -> bool {
        matches!(
            self,
            Self::View | Self::Function | Self::Procedure | Self::Trigger
        )
    }

    /// The plural group label the schema tree draws this kind's node with.
    pub const fn group_label(self) -> &'static str {
        match self {
            Self::Table => "Tables",
            Self::View => "Views",
            Self::MaterializedView => "Materialized views",
            Self::Function => "Functions",
            Self::Procedure => "Procedures",
            Self::Trigger => "Triggers",
            Self::Sequence => "Sequences",
            Self::Type => "Types",
        }
    }

    /// Parse a token produced by [`as_str`](Self::as_str). Used by the drivers'
    /// count queries, which label each row with the kind it counted rather than
    /// running one query per kind.
    pub fn from_token(token: &str) -> Option<Self> {
        Some(match token {
            "table" => Self::Table,
            "view" => Self::View,
            "matview" => Self::MaterializedView,
            "function" => Self::Function,
            "procedure" => Self::Procedure,
            "trigger" => Self::Trigger,
            "sequence" => Self::Sequence,
            "type" => Self::Type,
            _ => return None,
        })
    }

    /// A stable wire/persistence token, so a group can be named across the
    /// `Command`/`Event` boundary without leaking the Rust variant spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "matview",
            Self::Function => "function",
            Self::Procedure => "procedure",
            Self::Trigger => "trigger",
            Self::Sequence => "sequence",
            Self::Type => "type",
        }
    }
}

/// A namespace of objects, the top level of the schema tree. For SQLite this is
/// a database from `PRAGMA database_list` (`main` / `temp` / an attached DB); for
/// Postgres it's a real schema. One level so both engines fit the same tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMeta {
    pub name: String,
    /// Names + kinds only: the cheap tree skeleton, loaded on connect. Column
    /// detail is pulled per table via [`TableDetail`] on expand.
    pub objects: Vec<ObjectMeta>,
}

/// One table or view in a [`SchemaMeta`], just enough to draw the tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub name: String,
    pub kind: ObjectKind,
}

/// On-demand detail for one table/view: its columns, foreign keys, and indexes.
/// Loaded lazily when the user expands the object, never as part of the skeleton.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDetail {
    pub columns: Vec<ColumnMeta>,
    pub foreign_keys: Vec<ForeignKeyMeta>,
    pub indexes: Vec<IndexMeta>,
}

/// One column of a table/view. Richer than result-set [`Column`](crate::Column): it carries the
/// schema facts the tree shows (nullability, primary-key membership, default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Declared type, best-effort (`None` for an untyped/computed column).
    pub type_name: Option<String>,
    pub not_null: bool,
    pub primary_key: bool,
    pub default: Option<String>,
    /// The column auto-numbers (SQLite `INTEGER PRIMARY KEY` rowid alias, Postgres
    /// `serial`/identity, MySQL `AUTO_INCREMENT`). Detected by `describe_table` and
    /// re-emitted per-dialect by the migration's `create_table` so the target table
    /// keeps auto-numbering future inserts. `false` for result-derived columns (a
    /// query result carries no such flag) and read-only engines.
    pub auto_increment: bool,
}

/// A foreign-key edge from a local column to a referenced table/column. The tree
/// derives a column's FK badge by matching `column` against the table's columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMeta {
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// One foreign-key edge of the connection-wide relation graph. Unlike
/// the per-table [`ForeignKeyMeta`], this carries both endpoints' namespaces and is
/// usable in either direction: *forward* (`from_table` points out) backs "go to the
/// referenced row", *reverse* (`to_table` is pointed at) backs "show referencing
/// rows". `columns` pairs each local column with its referenced column in key order;
/// `len > 1` is a composite key. Loaded once per connection via
/// [`DatabaseDriver::foreign_keys`](../red_driver/trait.DatabaseDriver.html) and
/// indexed both ways by the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkEdge {
    pub from_schema: Option<String>,
    pub from_table: String,
    pub to_schema: Option<String>,
    pub to_table: String,
    /// `(from_column, to_column)` pairs, in key order.
    pub columns: Vec<(String, String)>,
}

/// The base subquery's alias in an inline-FK-expansion wrap (see [`FkJoin`] and the
/// driver's `join_wrap`): the result exposes `_red_base.*` first, so every base
/// column keeps its position and name. Shared so the UI's join builder and the
/// driver's SQL wrapper agree on the same identifier for a first hop's parent.
pub const BASE_ALIAS: &str = "_red_base";

/// One inline foreign-key *column expansion*: a `LEFT JOIN` that pulls
/// selected columns of a referenced table into a browse as extra, dotted-aliased
/// columns. The service folds an ordered `Vec<FkJoin>` into the result's base SQL
/// (see `join_wrap`), so each hop decorates the page without changing its row count:
/// the join target is always a *unique* key, so a `LEFT JOIN` matches ≤1 row.
///
/// Joins are ordered outer→inner: a deeper hop's [`parent_alias`](Self::parent_alias)
/// names an *earlier* join's [`alias`](Self::alias) (or the base, `_red_base`), so a
/// chain like `tier → cascade → placement` references only already-declared aliases.
/// Aliases are caller-assigned, simple identifiers (`_red_j0`, `_red_j1`, …) so they
/// never need quoting or length-shortening; the meaningful dotted names live in the
/// output [`select`](Self::select) aliases (e.g. `tier_id.name`), which is how the
/// grid keys a joined cell back to its tree column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkJoin {
    /// This join's table alias: a simple, unique identifier (`_red_j0`, …).
    pub alias: String,
    /// The alias this join's `ON` reads its local columns from: an earlier join's
    /// [`alias`](Self::alias), or the base subquery alias (`_red_base`) for a first hop.
    pub parent_alias: String,
    /// `(parent_column, target_column)` equalities forming the `ON` clause, in key
    /// order; usually one pair (a single-column FK), `len > 1` for a composite key.
    pub on: Vec<(String, String)>,
    /// The referenced table's namespace, when the engine qualifies by schema.
    pub to_schema: Option<String>,
    /// The referenced table.
    pub to_table: String,
    /// `(target_column, output_alias)` columns to select from the joined table. The
    /// output alias is the dotted tree path (`tier_id.name`), unique across the result.
    pub select: Vec<(String, String)>,
}

/// An index over one or more columns of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// How a result's rows are keyed for seek (keyset) pagination: an ordered,
/// effectively-unique tuple of columns. A plain table browse is a single key
/// column (the PK / unique index, see [`KeySpec::from_detail`]); a header-click
/// sorted browse is `(sort_col, pk)` with the PK as tiebreaker (see
/// [`KeySpec::sorted`]). Arbitrary editor SQL has no key and pages by `OFFSET`.
///
/// The seek is always a single row-value comparison `(c1, …) </> (…)`, so every
/// column in the tuple shares one [`direction`](Self::direction). RED
/// only offers a single-column header sort, so the tiebreaker inherits the lead's
/// direction and a mixed-direction tuple never arises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    /// The lead key column's name, as it appears in the result set: the sort
    /// column for a sorted browse, or the PK for a plain browse.
    pub column: String,
    /// The lead column's kind. Drives key-space interpolation (only the lead
    /// matters): an `Int` lead supports fraction jumps, `Other` degrades to
    /// `OFFSET` for far jumps.
    pub kind: KeyKind,
    /// The lead column's declared type (best-effort), so a driver that pins bind
    /// types can cast the keyset cursor back to the column's type. Without it a
    /// text-decoded cursor (a uuid/timestamp/numeric key) binds as `text` and the
    /// seek comparison `col > $1::text` has no operator (Postgres 42883).
    pub column_type: Option<String>,
    /// The PK tiebreaker, appended after [`column`](Self::column) when sorting by
    /// a non-PK column so rows sharing a `column` value order deterministically.
    /// `None` for a plain browse (the lead column is itself the unique key).
    pub tiebreak: Option<String>,
    /// The tiebreaker column's declared type, paired with [`tiebreak`](Self::tiebreak)
    /// for the same cursor-cast reason as [`column_type`](Self::column_type).
    pub tiebreak_type: Option<String>,
    /// The sort direction of the lead column (header click); `Asc` for a plain
    /// browse. The tiebreaker shares this direction.
    pub direction: SortDirection,
}

/// Whether the key is numerically interpolable. `Int` keys support key-space
/// seek (jump to a fraction via `min + f·(max − min)`); `Other` keys still get
/// keyset scroll but fall back to `OFFSET` for far jumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Int,
    Other,
}

/// A sort direction. Replaces the `descending: bool` that flowed through
/// [`KeySpec`], the sort spec, and the four drivers' seek builders — where a bare
/// bool invited transposition and read as noise at the `if descending { "DESC" }`
/// call sites. Note the two distinct roles it plays: a key's *own* ordering
/// (`KeySpec::direction`) and the *scroll* direction of a page fetch; the keyset
/// seek composes them with [`reversed_when`](Self::reversed_when).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDirection {
    #[default]
    Asc,
    Desc,
}

impl SortDirection {
    /// Build from the legacy `descending` flag (a header click / scroll flag).
    pub fn from_descending(descending: bool) -> Self {
        if descending { Self::Desc } else { Self::Asc }
    }

    /// Whether this sorts descending.
    pub fn is_descending(self) -> bool {
        matches!(self, Self::Desc)
    }

    /// The SQL keyword for this direction.
    pub fn sql(self) -> &'static str {
        if self.is_descending() { "DESC" } else { "ASC" }
    }

    /// This direction, reversed when `flip` is set. The keyset seek composes a
    /// key's own direction with the up/down scroll direction — the old
    /// `key.descending ^ scroll_descending`.
    pub fn reversed_when(self, flip: bool) -> Self {
        Self::from_descending(self.is_descending() ^ flip)
    }
}

impl KeySpec {
    /// A single-column, ascending key (a plain table browse; also the building
    /// block the conformance battery seeds with).
    pub fn single(column: impl Into<String>, kind: KeyKind) -> KeySpec {
        KeySpec {
            column: column.into(),
            kind,
            column_type: None,
            tiebreak: None,
            tiebreak_type: None,
            direction: SortDirection::Asc,
        }
    }

    /// The lead column's declared type for the seek cursor cast.
    pub fn with_column_type(mut self, type_name: Option<String>) -> KeySpec {
        self.column_type = type_name;
        self
    }

    /// The seek columns in order: the lead column then the tiebreaker, if any.
    /// Drivers quote these for the `ORDER BY` and the row-value comparison.
    pub fn column_names(&self) -> Vec<&str> {
        let mut cols = vec![self.column.as_str()];
        if let Some(t) = &self.tiebreak {
            cols.push(t.as_str());
        }
        cols
    }

    /// The seek columns' declared types, index-aligned with [`column_names`](Self::column_names),
    /// for a driver that casts the bound cursor back to each column's type.
    pub fn column_types(&self) -> Vec<Option<&str>> {
        let mut types = vec![self.column_type.as_deref()];
        if self.tiebreak.is_some() {
            types.push(self.tiebreak_type.as_deref());
        }
        types
    }

    /// Resolve a table's seek key from its introspected detail: a single-column
    /// primary key, or failing that a single-column unique not-null index.
    /// `None` (composite or nullable key, or no key at all) means the result
    /// isn't keyset-eligible and pages by `OFFSET`.
    pub fn from_detail(detail: &TableDetail) -> Option<KeySpec> {
        let column = resolve_key_column(detail)?;
        // SQLite reports `INTEGER PRIMARY KEY` (the rowid alias, never null) as
        // nullable, so an integer key passes without `not_null`; any other
        // nullable key disqualifies keyset (NULLs don't order reliably).
        let kind = key_kind(column);
        (column.not_null || kind == KeyKind::Int).then(|| {
            KeySpec::single(column.name.clone(), kind).with_column_type(column.type_name.clone())
        })
    }

    /// Resolve the composite seek key for a header-click sort by `sort_col`: the
    /// sort column led, with the table's PK appended as a tiebreaker so equal
    /// `sort_col` rows page deterministically. `None` (→ `OFFSET` fallback) when
    /// the table has no usable PK, when `sort_col` isn't a real column of the
    /// table (an expression/alias), or when it's nullable (NULLs don't order
    /// reliably across engines). Sorting by the PK itself collapses to the plain
    /// single-column key, just carrying the direction.
    pub fn sorted(
        detail: &TableDetail,
        sort_col: &str,
        direction: SortDirection,
    ) -> Option<KeySpec> {
        let pk = resolve_key_column(detail)?;
        let lead = detail.columns.iter().find(|c| c.name == sort_col)?;
        if lead.name == pk.name {
            return Some(KeySpec {
                column: pk.name.clone(),
                kind: key_kind(pk),
                column_type: pk.type_name.clone(),
                tiebreak: None,
                tiebreak_type: None,
                direction,
            });
        }
        // A nullable non-PK lead disqualifies keyset, unconditionally. The Int
        // exemption `from_detail` carries is for SQLite's `INTEGER PRIMARY KEY`
        // reporting `notnull=0` — a *key* column that can't actually be null —
        // and that case lands in the PK branch above. An ordinary nullable INT
        // lead (any FK column) genuinely holds NULLs, and a seek bound of NULL
        // compares as neither `>` nor `<` anything: the scroll silently stops
        // with rows missing (NULLs-first engines) or never reaches the NULL run
        // (Postgres, NULLS LAST) while the count includes it.
        let kind = key_kind(lead);
        lead.not_null.then(|| KeySpec {
            column: lead.name.clone(),
            kind,
            column_type: lead.type_name.clone(),
            tiebreak: Some(pk.name.clone()),
            tiebreak_type: pk.type_name.clone(),
            direction,
        })
    }
}

/// The PK / unique-not-null-index column a table's keyset key is built from, if
/// any. Shared by [`KeySpec::from_detail`] (the key itself) and
/// [`KeySpec::sorted`] (the tiebreaker).
fn resolve_key_column(detail: &TableDetail) -> Option<&ColumnMeta> {
    let pks: Vec<&ColumnMeta> = detail.columns.iter().filter(|c| c.primary_key).collect();
    match pks.as_slice() {
        [single] => Some(*single),
        [] => detail
            .indexes
            .iter()
            .filter(|i| i.unique && i.columns.len() == 1)
            .find_map(|i| {
                detail
                    .columns
                    .iter()
                    .find(|c| c.name == i.columns[0] && c.not_null)
            }),
        _ => None,
    }
}

/// Whether a column is numerically interpolable (drives key-space fraction jumps).
fn key_kind(column: &ColumnMeta) -> KeyKind {
    if column.type_name.as_deref().is_some_and(is_int_type) {
        KeyKind::Int
    } else {
        KeyKind::Other
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    fn col(name: &str, type_name: &str, not_null: bool, primary_key: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: Some(type_name.into()),
            not_null,
            primary_key,
            default: None,
            auto_increment: false,
        }
    }

    #[test]
    fn single_int_pk_is_the_key() {
        // SQLite-style: INTEGER PRIMARY KEY reports notnull = 0 but still counts.
        let detail = TableDetail {
            columns: vec![
                col("id", "INTEGER", false, true),
                col("name", "TEXT", true, false),
            ],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column, "id");
        assert_eq!(key.kind, KeyKind::Int);
    }

    #[test]
    fn text_pk_needs_not_null() {
        let nullable = TableDetail {
            columns: vec![col("id", "TEXT", false, true)],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&nullable).is_none());

        let not_null = TableDetail {
            columns: vec![col("id", "TEXT", true, true)],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&not_null).unwrap();
        assert_eq!(key.kind, KeyKind::Other);
    }

    #[test]
    fn composite_pk_is_ineligible() {
        let detail = TableDetail {
            columns: vec![
                col("a", "INTEGER", true, true),
                col("b", "INTEGER", true, true),
            ],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&detail).is_none());
    }

    #[test]
    fn unique_not_null_index_substitutes_for_a_pk() {
        let detail = TableDetail {
            columns: vec![
                col("email", "varchar", true, false),
                col("name", "text", false, false),
            ],
            indexes: vec![IndexMeta {
                name: "uq_email".into(),
                unique: true,
                columns: vec!["email".into()],
            }],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column, "email");
        assert_eq!(key.kind, KeyKind::Other);
    }

    #[test]
    fn no_key_at_all_is_ineligible() {
        let detail = TableDetail {
            columns: vec![col("x", "INTEGER", true, false)],
            ..Default::default()
        };
        assert!(KeySpec::from_detail(&detail).is_none());
    }

    #[test]
    fn key_carries_column_types_for_the_cursor_cast() {
        // A uuid PK: the key must carry "uuid" so the driver casts the keyset
        // cursor back to it (`$1::text::"uuid"`) instead of binding bare text;
        // otherwise `id > $1::text` is a 42883 "no operator" error on Postgres.
        let detail = TableDetail {
            columns: vec![
                col("id", "uuid", true, true),
                col("created_at", "timestamptz", true, false),
            ],
            ..Default::default()
        };
        let key = KeySpec::from_detail(&detail).unwrap();
        assert_eq!(key.column_type.as_deref(), Some("uuid"));
        assert_eq!(key.column_types(), vec![Some("uuid")]);

        // A header-click sort by a non-PK column leads with that column's type and
        // appends the PK as a typed tiebreaker; both need their cast.
        let sorted = KeySpec::sorted(&detail, "created_at", SortDirection::Desc).unwrap();
        assert_eq!(sorted.column_type.as_deref(), Some("timestamptz"));
        assert_eq!(sorted.tiebreak.as_deref(), Some("id"));
        assert_eq!(sorted.tiebreak_type.as_deref(), Some("uuid"));
        assert_eq!(
            sorted.column_types(),
            vec![Some("timestamptz"), Some("uuid")]
        );
    }

    #[test]
    fn int_detection_is_a_whitelist() {
        assert!(is_int_type("INTEGER"));
        assert!(is_int_type("bigint"));
        assert!(is_int_type("int(11) unsigned"));
        assert!(is_int_type("serial"));
        assert!(!is_int_type("interval"));
        assert!(!is_int_type("point"));
        assert!(!is_int_type("text"));
    }
}
