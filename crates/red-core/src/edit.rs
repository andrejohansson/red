//! What the grid may change about a table's existing rows.
//!
//! [`WriteCaps`](crate::WriteCaps) answers "can this *engine* edit at all";
//! [`RowEditCaps`] answers "can this *table*, and under which contract" - a
//! question only the catalog can settle, and one the UI has to know before it
//! offers a cell for editing.

use super::TableDetail;

/// What in-grid row editing *this table* supports, resolved per browse alongside its
/// seek key. [`WriteCaps`](crate::WriteCaps) answers "can this engine edit at all"; this answers "can
/// this table, and under which contract" -- questions only the catalog can settle. A
/// `MergeTree` ClickHouse table is editable where a `Memory` one, a view, or a
/// `Distributed` table is not, and none of that is derivable from [`DbKind`](crate::DbKind).
///
/// Deliberately about **rows that already exist**. Inserting is governed by
/// [`WriteCaps::insert`](crate::WriteCaps::insert) plus [`no_insert`](Self::no_insert): a draft row names no
/// existing row, so it needs neither an identity nor a rollback, and stays available
/// on a table whose [`mode`](Self::mode) is [`EditMode::None`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowEditCaps {
    pub mode: EditMode,
    /// The columns whose values, taken together, address a row for an `UPDATE`/
    /// `DELETE`. A relational primary key (composite included); on ClickHouse the
    /// sorting-key columns **first**, then the remaining comparable ones. Empty exactly
    /// when [`mode`](Self::mode) is [`EditMode::None`].
    pub identity: Vec<String>,
    /// Columns that can never be *updated*: the identity columns themselves, plus
    /// anything the engine computes (a ClickHouse `MATERIALIZED`/`ALIAS` column).
    pub no_update: Vec<String>,
    /// Columns that can never be *written on insert*. Distinct from
    /// [`no_update`](Self::no_update) on purpose: a sorting-key column must be set on
    /// a draft row but can never be updated afterwards, so it belongs to one list and
    /// not the other.
    pub no_insert: Vec<String>,
    /// One line the UI can show: why editing is unavailable when
    /// [`mode`](Self::mode) is [`EditMode::None`], or the caveat that applies when it
    /// is [`EditMode::BestEffort`]. `None` when there is nothing to say.
    pub note: Option<String>,
}

/// Which edit contract a table's rows can be changed under (see [`RowEditCaps`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditMode {
    /// Not editable: no usable row identity, or an engine/table that cannot mutate
    /// at all. [`RowEditCaps::note`] says which.
    #[default]
    None,
    /// The relational contract: one transaction, exactly one row per op, rolled back
    /// on any failure (see `DatabaseDriver::apply_edits`).
    Guarded,
    /// Best-effort: asynchronous, non-atomic, per-op outcomes, and a preflight count
    /// rather than an engine-enforced one-row guarantee (see
    /// `DatabaseDriver::apply_edits_best_effort`). ClickHouse.
    BestEffort,
}

impl RowEditCaps {
    /// The pure, catalog-free derivation for a relational engine: the primary key is
    /// the identity, and it is the one thing that can't be updated. Composite keys
    /// included -- addressing a row by its full key tuple is exactly as sound as by a
    /// single column. Falls back to a single-column `UNIQUE NOT NULL` index when the
    /// table declares no primary key (the same fallback the seek key uses), and
    /// reports [`EditMode::None`] with a reason when neither exists.
    pub fn guarded(detail: &TableDetail) -> RowEditCaps {
        let pk: Vec<String> = detail
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        let identity = if pk.is_empty() {
            detail
                .indexes
                .iter()
                .filter(|i| i.unique && i.columns.len() == 1)
                .find_map(|i| {
                    detail
                        .columns
                        .iter()
                        .find(|c| c.name == i.columns[0] && c.not_null)
                        .map(|c| vec![c.name.clone()])
                })
                .unwrap_or_default()
        } else {
            pk
        };
        if identity.is_empty() {
            return RowEditCaps {
                note: Some(
                    "no primary key or unique NOT NULL column: rows can't be addressed \
                     individually"
                        .to_string(),
                ),
                ..RowEditCaps::default()
            };
        }
        RowEditCaps {
            mode: EditMode::Guarded,
            no_update: identity.clone(),
            identity,
            no_insert: Vec::new(),
            note: None,
        }
    }

    /// Whether an existing row can be updated or deleted at all.
    pub fn editable(&self) -> bool {
        !matches!(self.mode, EditMode::None)
    }
}
