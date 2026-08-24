# Deferred work

Things deliberately left undone, with enough context to pick them up later.
Add an entry here instead of a silent `TODO` when a feature ships partially.

## Native macOS menu bar

The menu bar (`crates/red/src/menu.rs`, mounted in `main.rs` via `cx.set_menus`)
shipped with the bold app menu (About, Settings…, Services, Quit) plus
**Edit / View / Query / Tabs / Help**, every item wired to an action that already
existed. The pieces below were intentionally skipped to keep that change small
and low-risk.

### Multi-window / "New Window"

No **File** menu and no second window yet. This is the largest architectural
piece. Picking it up means:

- Extract the `cx.open_window(…)` block in `main.rs` into an `open_main_window(…)`
  helper, add a `NewWindow` (⌘N) action, and generalise the existing
  `on_window_closed` "last window quits" logic.
- Decide between a shared backend service and one `red_service` per window.
  Shared is the likely answer (windows are views onto the same sessions), and
  the same decision affects future multi-connection work, so settle both
  together.
- A **File** menu would then carry: New Window ⌘N · Open Connection… ⌘O ·
  Open Recent ▶ · Close Window ⇧⌘W. None of it exists yet.

Note: `NewConnection` (⌘N, welcome-screen "add a connection") already exists and
is unrelated; it opens the connection *form*, not a window. The deferred ⌘N is
the window one, so the two will need to be reconciled when multi-window lands.

### Open Recent + dynamic menus

The menu tree is a **static snapshot** built once at startup. There is no
`refresh_menus(cx)` helper and no `cx.set_menus` re-call on state changes, so:

- **Open Recent** (an MRU submenu of recent connections) is not present. It needs
  a persisted MRU list (slot into the `settings.toml` system, updated whenever a
  connection opens) plus a rebuilt `MenuItem::submenu` and a `set_menus` re-call
  when the list changes.
- Any `.checked()` menu state (e.g. a ✓ on "Toggle Sidebar" when the sidebar is
  shown) would likewise need the refresh helper.

`build_menus()` is already side-effect-free and cheap precisely so it can be
re-called once a `refresh_menus` helper exists.

**Settings…** is wired (⌘,) but opens the in-app settings *panel*
(`open_settings`) rather than `settings.toml` in an external editor; the panel
is the richer existing surface. `About RED` opens the same panel on its About
tab.

### `.app` bundle so the title reads "RED"

macOS forces the bold app-menu title to the process/bundle name. When RED runs
as a bare binary, the app menu reads as the lowercase binary name, not **RED**.
Fixing it means shipping a `.app` bundle with an `Info.plist`
(`CFBundleName = RED`, bundle id, icon), e.g. `cargo-bundle` config under
`[package.metadata.bundle]`. The menu works without it; only the title
cosmetics are affected.

### Edit menu: Undo / Redo

Flint's `TextInput` and `CodeEditor` have **no undo stack**, so Undo/Redo menu
items would be dead. Rather than shipping items that do nothing, they're left
out. They can be added once Flint grows undo support (paired with
`OsAction::Undo/Redo`).

### Edit menu: SQL editor clipboard (partial)

The Edit menu's Cut/Copy/Paste/Select All pair Flint's **`TextInput`** clipboard
actions with `OsAction`s, so they drive the connection-form text fields. Flint's
**`CodeEditor`** (the SQL editor) declares a *separate* set of clipboard actions,
so a single menu item can't reach both. The SQL editor's own ⌘C/⌘V keystrokes
still work, but the menu items don't target it. Unifying the two clipboard
action sets in Flint, so one menu item serves both surfaces, is the clean fix.

## BigQuery: unverified premises

The BigQuery driver's design (see `docs/adr/0001`–`0005`) rests on several claims
about BigQuery's *own* behaviour rather than about RED's code. The in-process mock
HTTP tests cannot check any of them — the fixtures encode the same assumptions the
code does, so they agree by construction. The env-gated live suite
(`RED_TEST_BIGQUERY_URL`) is the only thing that can, and **it has never been
run**: there is no test project yet, so those tests skip.

Each premise below is load-bearing for the ADR named beside it. Treat them as
unverified until the live suite runs against a real project.

- **`COUNT(*)` bills 0 bytes through RED's wrapping.** BigQuery answers
  `COUNT(*)` from metadata, but RED asks it as
  `SELECT count(*) FROM (SELECT * FROM t) AS _red`. The optimizer should prune to
  zero columns; if it does not, every browse open pays for a full scan. (ADR-0002)
- **`startIndex` re-reads a completed job without re-billing.** The whole job
  cache exists for this. If a `getQueryResults` read against a cached job bills a
  scan, offset paging is no cheaper than re-querying. (ADR-0002)
- **A ~60s job cache does not interact badly with the 24h temp-table lifetime.**
  (ADR-0002)
- **`maximumBytesBilled` rejects before scanning, not after.** A cap enforced
  after the fact is an alert, not a guard. (ADR-0004)
- **`num_dml_affected_rows` is populated for a parameterized `INSERT`.** The
  insert path reports affected rows from it. (ADR-0003)
- **A declared `PRIMARY KEY` really is unenforced**, i.e. duplicates are
  accepted. This is the stated reason in-grid editing is off; if BigQuery began
  enforcing it, ADR-0003's reasoning would need revisiting (though the
  full-table-scan cost argument stands on its own). (ADR-0003)

The live suite should assert these directly, so a failure names the premise and
the ADR that depends on it rather than surfacing as a puzzling driver bug.

## BigQuery: gaps in the shipped driver

Things the driver leaves undone rather than guesses at. Each is a **gap**, not an
absence: BigQuery can do these, so a future reader should not conclude from the
code that the engine cannot.

### The foreign-key graph is empty

`foreign_keys` returns no edges, so FK click-through and the relation tree are
absent on BigQuery, and `describe_table` reports no per-table foreign keys.

BigQuery *does* have declarable (unenforced) foreign keys, exposed across
`TABLE_CONSTRAINTS`, `KEY_COLUMN_USAGE` and `CONSTRAINT_COLUMN_USAGE`. The read
spans three views whose region-level availability could not be verified against a
live project, and a relation graph that is subtly wrong sends click-through to the
wrong table — a worse outcome than the documented empty degradation the trait
already allows. Pick this up with the live suite, not before.

### `execute_batch` is sequential, not atomic

Every other SQL driver runs a statement batch in one transaction. BigQuery's arm
runs one job per statement, so a failure part-way leaves the earlier statements
committed. The trait doc explicitly permits this ("an engine without
multi-statement transactions runs them sequentially; it documents that") but the
reason here is different: BigQuery *has* `BEGIN TRANSACTION`, and a
multi-statement script would be genuinely atomic — it just reports a single
`numDmlAffectedRows` for the whole job, so the per-statement counts this method
promises could not be returned without fabricating a split.

Per-statement jobs inside a BigQuery **session** would give both atomicity and
real counts. That is also what `begin_sandbox` needs, so the two should land
together.

### Multi-chunk `insert_rows` is not atomic either

A bulk insert wider than the parameter cap is sub-chunked into several jobs. A
failure part-way leaves the earlier chunks committed, so a retry appends
duplicates rather than starting clean. Same fix as above (a session), same
reason it is deferred.

### No `health` report, and no BigQuery connection importer

`health` falls through to the trait default, which reports that the engine has
none. `INFORMATION_SCHEMA.TABLE_STORAGE` could back a real one.

The DBeaver / DBGate / DataGrip importers set `credentials_path` empty for every
connection: translating the Simba JDBC form (`ProjectId`, `OAuthPvtKeyPath`,
`DefaultDataset`, `OAuthType`) into RED's shape is mechanical but needs its own
fixtures. See `docs/adr/0005`.
