# BigQuery is an insert target, not an in-grid edit target

BigQuery can honour RED's guarded edit contract on paper: DML is unrestricted,
`BEGIN TRANSACTION`/`COMMIT` are real, query parameters keep values bound rather
than interpolated, and `num_dml_affected_rows` makes the "exactly one row"
assertion checkable. It is still switched off, because on BigQuery **editing one
cell is a full-table scan** — an `UPDATE … WHERE id = 5` against an
unpartitioned 1 TiB table scans 1 TiB and bills roughly $6. So
`write_caps` is `insert: true`, `guarded_edit: false`, `best_effort_edit: false`.

The split follows the economics rather than the capability. Bulk insert is cheap
and is the case people actually want — it makes BigQuery a real copy/migration
target. Point editing is a high-frequency, low-value write pattern that this
engine is architecturally hostile to, and two further limits confirm it: rows
land in a streaming buffer where they cannot be updated or deleted for up to ~90
minutes, and mutating DML is capped at roughly 20 concurrent statements per
table.

## Consequences

- `edit_caps` is overridden to report `EditMode::None` with a `note` giving the
  reason (a declared primary key is **not enforced** on BigQuery, and a point
  update scans the table). The user learns why rather than meeting a dead menu
  item.
- Draft-row insert stays available, since `WriteCaps::insert` governs it
  independently of row editing — a new row needs neither an identity nor a
  rollback.
- `create_index` and `add_foreign_key` are logged skips: BigQuery has only
  `CREATE SEARCH INDEX`, and an `ADD CONSTRAINT … NOT ENFORCED` would be
  metadata theatre.
- `clear_table` uses `TRUNCATE TABLE`, **deviating** from the trait doc's
  prescribed `DELETE`. That prescription exists because MySQL's `TRUNCATE`
  auto-commits and resets auto-increment; BigQuery has no auto-increment, and
  here `TRUNCATE` is a free metadata operation while `DELETE FROM t WHERE TRUE`
  (a bare `DELETE` is rejected outright) is a billed full-table scan.
