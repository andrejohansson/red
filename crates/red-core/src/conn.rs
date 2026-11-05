//! What a connection *is*: which engine, where, over what transport, and how
//! RED talks about it.
//!
//! [`ConnectionConfig`] is the saved shape; [`ConnectionConfig::dsn`] composes the
//! driver-facing string from it on demand, and [`ConnectionConfig::parse_conn_str`]
//! goes the other way for a pasted URL. The two must stay exact inverses - an
//! Atlas `mongodb+srv` string that does not survive the round trip is a connection
//! that cannot be made - which is why they live beside each other.
//!
//! **No secret is ever serialized here.** `password`, the SSH password and
//! passphrase, and the proxy password are all `serde(skip)`; they live in the OS
//! keychain, keyed by connection id, and are materialized transiently for a dial.
//! Every type that holds one has a hand-written `Debug` that redacts it, so a
//! stray `{config:?}` cannot spill one into a log.

use std::fmt;

use super::{AiTier, ObjectKind, SessionCaps};

/// Which database engine a connection targets. Drives driver selection and,
/// via [`DbKind::all`]/the metadata accessors, how the connection form renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DbKind {
    Postgres,
    #[default]
    Sqlite,
    Mysql,
    /// ClickHouse: the OLAP engine, reached over its HTTP interface. A writable
    /// connection is an INSERT / copy / migration *target* and can edit rows in the
    /// grid, but under the **best-effort** contract rather than the guarded one:
    /// `UPDATE`/`DELETE` are async `ALTER … UPDATE` mutations with no transaction or
    /// rollback over a non-unique sort key, so each op is preflight-counted and
    /// reported for itself (see [`DbKind::write_caps`] and [`RowEditCaps`](crate::RowEditCaps)).
    Clickhouse,
    /// Redis/Valkey: a key-value store, not SQL-shaped at all. Reached through the
    /// parallel `KvDriver` seam (`red-driver`'s `redis_kv` module), not
    /// `DatabaseDriver` —.
    Redis,
    /// MongoDB: a document store, neither SQL- nor Redis-shaped. Reached through the
    /// third `DocDriver` seam (`red-driver`'s `doc` module), a `server → databases →
    /// collections → documents` hierarchy of nested BSON trees —.
    Mongo,
    /// Google BigQuery: a serverless OLAP warehouse reached over its REST API.
    /// Structurally the odd one out -- no host, no port, and no user/password (a
    /// bearer token from ADC or a service-account key), and its hierarchy is one
    /// level deeper than [`TableRef`](crate::TableRef) models, so a connection pins
    /// one *project* and the **dataset** is the [`namespace`](DbKind::namespace_caps).
    ///
    /// Every read is billed by bytes scanned, which is why this is the only engine
    /// whose queries are cost-gated (see [`DbKind::estimates_query_cost`]) and the
    /// only one that pages by offset over a cached result rather than by keyset
    /// seek (see [`DbKind::supports_keyset_paging`]).
    Bigquery,
}

impl DbKind {
    /// Every engine, in the order the connection form lists them. Adding a driver
    /// means adding a variant here and an arm to each accessor below; the form is
    /// data-driven off these, not a hand-maintained list.
    pub const fn all() -> &'static [DbKind] {
        &[
            DbKind::Postgres,
            DbKind::Sqlite,
            DbKind::Mysql,
            DbKind::Clickhouse,
            DbKind::Redis,
            DbKind::Mongo,
            DbKind::Bigquery,
        ]
    }

    /// File-based engines (SQLite) take a single path, not host/port/user/pass.
    /// Whether the engine has a multi-statement transaction the user can hold
    /// open across several commands.
    ///
    /// ClickHouse does not: it has no `BEGIN`, so the driver's `begin_sandbox`
    /// returns `None` and a manual transaction can never be opened on it. This
    /// mirrors that so the UI can withhold the affordance rather than offer one
    /// that always fails. Redis and MongoDB are not SQL sessions at all.
    pub const fn supports_transactions(self) -> bool {
        matches!(self, Self::Sqlite | Self::Postgres | Self::Mysql)
    }

    /// Whether a browse on this engine should page by **keyset seek** rather than
    /// by offset. True everywhere the seek's premise holds: that an indexed
    /// `WHERE key > x ORDER BY key LIMIT n` is cheaper than an `OFFSET` walk, so
    /// it costs the same at row 200 or 46,000,000.
    ///
    /// BigQuery inverts that premise and so opts out. It has no indexes, and a
    /// keyset seek there is a *new job over a new scan* for every page, while
    /// `fetch_page` re-reads an already-materialized job result at no additional
    /// scan cost -- so offset paging is both faster and cheaper, the exact reverse
    /// of every other engine.
    ///
    /// A [`DbKind`] descriptor rather than a driver method because the decision is
    /// made in [`KeySpec::from_detail`](crate::KeySpec::from_detail), which is pure
    /// and sees only a [`TableDetail`](crate::TableDetail). Without it, BigQuery's
    /// *unenforced* `PRIMARY KEY` declarations would silently switch keyset paging
    /// back on.
    pub const fn supports_keyset_paging(self) -> bool {
        !matches!(self, Self::Bigquery)
    }

    /// Whether this engine can price a statement *before* running it, so the UI
    /// can show what a query will cost and the service can gate an expensive one
    /// behind a confirmation.
    ///
    /// Only BigQuery bills by bytes scanned, and only BigQuery has the `dryRun`
    /// that answers for free. A descriptor rather than a `== DbKind::Bigquery`
    /// check for the same reason [`write_caps`](Self::write_caps) is one: the UI
    /// holds a [`DbKind`], never a driver.
    pub const fn estimates_query_cost(self) -> bool {
        matches!(self, Self::Bigquery)
    }

    /// What the connection form calls the field holding a connection's authority --
    /// the "where" of it. "Host" for everything with a hostname; **"Project"** for
    /// BigQuery, whose authority is a GCP project id and which has no host at all.
    ///
    /// The sibling of [`NamespaceCaps::label`], which renames the namespace level
    /// per engine for the same reason: the form is data-driven off these
    /// descriptors, not a hand-maintained list.
    pub const fn host_label(self) -> &'static str {
        match self {
            Self::Bigquery => "Project",
            _ => "Host",
        }
    }

    /// Quote one identifier for this engine's dialect.
    ///
    /// Lives here, and is **exhaustive on purpose**, because the app generates
    /// runnable SQL in several places that cannot reach a driver — the schema tree's
    /// "browse this table", the workspace's saved query, the schema-diff script, the
    /// headless `copy`. Each of those had grown its own copy of this match with a
    /// `_ =>` ANSI-double-quote fallback, and a wildcard is exactly the wrong shape:
    /// adding BigQuery silently produced `SELECT * FROM "ds"."tbl"`, which GoogleSQL
    /// reads as a *string literal* and rejects with `Unexpected string literal`. The
    /// compiler said nothing, because the wildcard already covered it.
    ///
    /// So there is one match, no wildcard, and the next engine added breaks the build
    /// here until someone decides how it quotes. The drivers keep their own
    /// quoting — they are the seam to the engine and are already tested against it —
    /// but nothing in the UI needs a second implementation.
    ///
    /// Escaping differs per engine and the differences matter: MySQL and BigQuery
    /// use backticks where the others use double quotes, ClickHouse honours
    /// backslash escapes *inside* its quotes (so a name ending in `\` would break
    /// out), and BigQuery has no escape for a backtick at all — such an identifier
    /// cannot exist there, so stripping is lossless for anything real.
    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            DbKind::Mysql => format!("`{}`", ident.replace('`', "``")),
            DbKind::Bigquery => format!("`{}`", ident.replace('`', "")),
            DbKind::Clickhouse => {
                format!("\"{}\"", ident.replace('\\', "\\\\").replace('"', "\"\""))
            }
            DbKind::Postgres | DbKind::Sqlite => {
                format!("\"{}\"", ident.replace('"', "\"\""))
            }
            // Neither is SQL-shaped, so neither has an identifier to quote and
            // neither can reach a SQL builder (`write_caps().insert` is false, which
            // the copy/migrate pickers filter on). The ANSI form is a harmless answer
            // rather than a meaningful one.
            DbKind::Redis | DbKind::Mongo => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    pub const fn is_file(self) -> bool {
        matches!(self, DbKind::Sqlite)
    }

    /// The conventional default port, or `None` for file engines. ClickHouse's
    /// default is its HTTP interface port (`8123`), which the driver speaks.
    pub const fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Postgres => Some(5432),
            DbKind::Mysql => Some(3306),
            DbKind::Clickhouse => Some(8123),
            DbKind::Redis => Some(6379),
            DbKind::Mongo => Some(27017),
            // Not an omission: BigQuery is reached over HTTPS at a fixed Google
            // endpoint, so there is no port for a user to get wrong. The first
            // *network* engine with none (SQLite's `None` is a file path).
            DbKind::Bigquery => None,
            DbKind::Sqlite => None,
        }
    }

    /// The URL scheme used when composing a connection string for the driver.
    pub const fn url_scheme(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::Mysql => "mysql",
            DbKind::Sqlite => "sqlite",
            DbKind::Clickhouse => "clickhouse",
            DbKind::Redis => "redis",
            DbKind::Mongo => "mongodb",
            DbKind::Bigquery => "bigquery",
        }
    }

    /// Map a connection-string scheme (`postgresql`, `mariadb`, `file`, …) onto an
    /// engine, for parsing a pasted DSN.
    pub fn from_scheme(scheme: &str) -> Option<DbKind> {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(DbKind::Postgres),
            "mysql" | "mariadb" => Some(DbKind::Mysql),
            "sqlite" | "sqlite3" | "file" => Some(DbKind::Sqlite),
            "clickhouse" | "clickhouse-http" | "ch" | "clickhouses" => Some(DbKind::Clickhouse),
            // The TLS schemes (`rediss`/`clickhouses`) parse to the same engine;
            // `parse_conn_str` reads the `tls` bit from the scheme separately.
            "redis" | "rediss" => Some(DbKind::Redis),
            // Both the plain and the SRV/DNS-seedlist spelling map to one engine;
            // `mongodb+srv` implies TLS, which the driver reads off the scheme.
            "mongodb" | "mongodb+srv" => Some(DbKind::Mongo),
            // `sqlalchemy-bigquery`'s spelling, the de facto URL form (see
            // `docs/adr/0005`); `bq` is the CLI's shorthand.
            "bigquery" | "bq" => Some(DbKind::Bigquery),
            _ => None,
        }
    }

    /// Whether a URL scheme denotes a TLS connection (`rediss`/`clickhouses`),
    /// so a pasted secure DSN pre-checks the form's TLS toggle.
    fn scheme_is_tls(scheme: &str) -> bool {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "rediss" | "clickhouses"
        )
    }

    /// What write operations this engine can honor, *independent* of the
    /// connection's `read_only` flag (a read-only connection refuses every write
    /// regardless). Both the UI (which only holds a [`DbKind`] + `read_only`, never
    /// the driver) and the driver consult this so the affordances they offer and the
    /// operations they'll actually run agree, replacing the scattered
    /// `== DbKind::Clickhouse` checks that used to disagree with the read-only gate.
    pub const fn write_caps(self) -> WriteCaps {
        match self {
            // The relational engines: transactional, PK-guarded in-grid editing and
            // bulk insert / copy-and-migrate target.
            DbKind::Postgres | DbKind::Sqlite | DbKind::Mysql => WriteCaps {
                insert: true,
                guarded_edit: true,
                best_effort_edit: false,
            },
            // ClickHouse (OLAP): a full write target, but under a different contract.
            // Inserts are ordinary; `UPDATE`/`DELETE` are asynchronous, non-atomic
            // mutations over a non-unique sort key, so they can't honor the guarded
            // (one transaction, exactly one row, rollback on failure) promise and ride
            // the best-effort seam instead -- preflight-counted, per-op reported, and
            // always confirmed. Which *tables* that applies to is a further question
            // only the catalog answers; see [`RowEditCaps`].
            DbKind::Clickhouse => WriteCaps {
                insert: true,
                guarded_edit: false,
                best_effort_edit: true,
            },
            // Redis: no write path exists yet (R0/R1 are read-only browsing).
            // R3 adds SET/HSET/EXPIRE/DEL through the
            // KvDriver seam, not through this SQL-shaped capability set at all —
            // this stays all-`false` so any UI affordance still gated on
            // `write_caps` (rather than the connection kind) stays hidden.
            DbKind::Redis => WriteCaps {
                insert: false,
                guarded_edit: false,
                best_effort_edit: false,
            },
            // MongoDB: like Redis, every write rides its own seam (`DocDriver`), not
            // this SQL capability set, so any affordance gated on `write_caps` (rather
            // than the connection kind) stays hidden.
            DbKind::Mongo => WriteCaps {
                insert: false,
                guarded_edit: false,
                best_effort_edit: false,
            },
            // BigQuery: an insert / copy / migration **target**, never an in-grid
            // edit target. It could honour the guarded contract on paper (real
            // transactions, bound parameters, `num_dml_affected_rows`) and is
            // switched off anyway, because editing one cell there is a full-table
            // scan: an `UPDATE ... WHERE id = 5` over an unpartitioned 1 TiB table
            // scans 1 TiB. See `docs/adr/0003`.
            DbKind::Bigquery => WriteCaps {
                insert: true,
                guarded_edit: false,
                best_effort_edit: false,
            },
        }
    }

    /// Whether this engine applies row edits as **asynchronous background
    /// mutations** the user can watch and cancel (see [`MutationInfo`](crate::MutationInfo)). Drives the
    /// Mutations panel, which would be an empty box on an engine whose writes are
    /// finished by the time the statement returns.
    ///
    /// A descriptor rather than a `== DbKind::Clickhouse` check, for the same reason
    /// [`write_caps`](Self::write_caps) is one: the UI holds a [`DbKind`], never a
    /// driver, and scattered engine comparisons are what used to disagree with each
    /// other.
    pub const fn tracks_mutations(self) -> bool {
        matches!(self, DbKind::Clickhouse)
    }

    /// Whether this engine can answer a live
    /// [`ServerSnapshot`](crate::server::ServerSnapshot): memory, throughput,
    /// connections, uptime. Gates the Server panel's Overview view and, with it,
    /// whether the panel is offered at all on an engine that has no sessions to
    /// list.
    ///
    /// True everywhere there is a server process to ask. SQLite is a file opened
    /// in-process: there is nothing to be out of memory, nothing to be busy, and
    /// nothing to have been up since Tuesday.
    ///
    /// BigQuery has no server to describe either, but
    /// [`ServerSnapshot`](crate::server::ServerSnapshot) carries an open list of
    /// metrics rather than fixed memory/uptime fields, so it reports what is
    /// meaningful for a serverless warehouse instead: bytes billed over the last
    /// day, slot-ms consumed, and jobs in flight.
    pub const fn reports_metrics(self) -> bool {
        !matches!(self, DbKind::Sqlite)
    }

    /// What this engine lets RED see and stop about the server's other sessions
    /// (see [`ServerSession`](crate::ServerSession)). Gates the Server panel and its kill affordances.
    pub const fn session_caps(self) -> SessionCaps {
        match self {
            // `pg_stat_activity` plus a real lock manager: the full picture.
            DbKind::Postgres => SessionCaps {
                supported: true,
                can_cancel: true,
                can_terminate: true,
                has_wait_graph: true,
            },
            // `information_schema.processlist` + `KILL QUERY` / `KILL`. The wait
            // graph needs performance_schema, which is present by default on
            // MySQL 5.7+ and degrades to an empty `blocked_by` when it is not.
            DbKind::Mysql => SessionCaps {
                supported: true,
                can_cancel: true,
                can_terminate: true,
                has_wait_graph: true,
            },
            // `system.processes` + `KILL QUERY`. There is no session to terminate
            // separately from its query (HTTP: one request, one session), and no
            // lock manager to build a wait graph from.
            DbKind::Clickhouse => SessionCaps {
                supported: true,
                can_cancel: true,
                can_terminate: false,
                has_wait_graph: false,
            },
            // A file, opened in-process. There is no server and no other session.
            DbKind::Sqlite => SessionCaps {
                supported: false,
                can_cancel: false,
                can_terminate: false,
                has_wait_graph: false,
            },
            // `CLIENT LIST` + `CLIENT KILL ID`. There is no query-level cancel
            // (a Redis command is atomic and already running) and no lock
            // manager to build a wait graph from. The Monitor tab keeps the
            // richer per-client view; this is the shared-panel arm.
            DbKind::Redis => SessionCaps {
                supported: true,
                can_cancel: false,
                can_terminate: true,
                has_wait_graph: false,
            },
            // `$currentOp` + `killOp`. `killOp` stops the operation rather than
            // the connection, which is nearer "cancel" than "terminate"; it is
            // reported as terminate because that is the rung the confirm modal
            // already grades, and because an interrupted multi-document write is
            // not rolled back, so it deserves the heavier prompt.
            DbKind::Mongo => SessionCaps {
                supported: true,
                can_cancel: false,
                can_terminate: true,
                has_wait_graph: false,
            },
            // `INFORMATION_SCHEMA.JOBS_BY_PROJECT` + `jobs.cancel`. The same shape
            // as ClickHouse: one request is one job, so there is no session to
            // terminate apart from its query, and no lock manager to build a wait
            // graph from.
            DbKind::Bigquery => SessionCaps {
                supported: true,
                can_cancel: true,
                can_terminate: false,
                has_wait_graph: false,
            },
        }
    }

    /// Which [`ObjectKind`]s this engine has, in the order the schema tree draws
    /// their group nodes. A kind absent here is never queried for and never
    /// drawn, so SQLite grows no empty "Procedures" group.
    ///
    /// Relations come first (they are in the connect-time skeleton and are what a
    /// user is usually looking for); the lazily-loaded programmatic kinds follow.
    /// Another descriptor rather than an engine comparison, for the same reason
    /// [`write_caps`](Self::write_caps) is one.
    pub const fn object_kinds(self) -> &'static [ObjectKind] {
        match self {
            // The richest catalog: everything below exists, and `prokind`
            // separates functions from procedures on PG 11+.
            DbKind::Postgres => &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
                ObjectKind::Function,
                ObjectKind::Procedure,
                ObjectKind::Trigger,
                ObjectKind::Sequence,
                ObjectKind::Type,
            ],
            // No materialized views and no user-defined types. Sequences are a
            // MariaDB-only feature, so the group is offered and simply comes back
            // empty on MySQL (`information_schema.sequences` is absent there,
            // which the driver treats as "none", not as an error).
            DbKind::Mysql => &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::Function,
                ObjectKind::Procedure,
                ObjectKind::Trigger,
                ObjectKind::Sequence,
            ],
            // `sqlite_master` knows tables, views, triggers, and indexes; indexes
            // already live inside `TableDetail`, so they are not a tree kind.
            DbKind::Sqlite => &[ObjectKind::Table, ObjectKind::View, ObjectKind::Trigger],
            // Materialized views are a table engine; UDFs live in
            // `system.functions`. No triggers, sequences, or user types.
            DbKind::Clickhouse => &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
                ObjectKind::Function,
            ],
            // `INFORMATION_SCHEMA.ROUTINES.routine_type` separates functions from
            // procedures. No triggers, no sequences, no user-defined types.
            DbKind::Bigquery => &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
                ObjectKind::Function,
                ObjectKind::Procedure,
            ],
            // Neither engine is SQL-shaped and neither populates a `SchemaMeta`
            // through this path at all.
            DbKind::Redis | DbKind::Mongo => &[],
        }
    }

    /// How this engine resolves the names in an *unqualified* query — the
    /// namespace a bare `FROM users` binds against — and whether RED can rebind
    /// it per query. The counterpart to [`DbKind::write_caps`]: the UI holds only
    /// a [`DbKind`], never a driver, so it reads this to decide whether to offer a
    /// namespace picker at all and what to call it.
    ///
    /// The tree's top level *is* this namespace for every engine (see
    /// [`SchemaMeta`](crate::SchemaMeta)), so a picker is just the tree's own list.
    pub const fn namespace_caps(self) -> NamespaceCaps {
        match self {
            // A MySQL connection can browse every database on the server, and its
            // database segment is optional (see the connection form), so an
            // unqualified query has no target at all until one is bound: the
            // engine answers error 1046 (`ER_NO_DB_ERROR`). Rebound per query with
            // `USE`, which is why this is `settable`.
            DbKind::Mysql => NamespaceCaps {
                settable: true,
                required: true,
                label: "Database",
            },
            // Like MySQL, a ClickHouse connection sees every database — but an
            // unbound query silently resolves against `default` instead of
            // erroring, so `required` is false while the picker still matters:
            // querying the wrong database is a worse failure than being told.
            DbKind::Clickhouse => NamespaceCaps {
                settable: true,
                required: false,
                label: "Database",
            },
            // Postgres binds its database at connect (it cannot be switched on a live
            // connection); the rebindable namespace is the *schema*, via `search_path`.
            // Not settable yet: a `SET search_path` persists on a pooled/streaming
            // client and would leak across tabs, so it needs the per-client tracking
            // that MySQL's re-acquire-per-operation pooling makes unnecessary.
            DbKind::Postgres => NamespaceCaps {
                settable: false,
                required: false,
                label: "Schema",
            },
            // SQLite's namespaces are `main`/`temp` plus whatever is attached;
            // `main` is fixed for the lifetime of the connection and unqualified
            // names resolve against it, so there is nothing to pick.
            DbKind::Sqlite => NamespaceCaps {
                settable: false,
                required: false,
                label: "Database",
            },
            // A BigQuery job names its `defaultDataset` per request, so rebinding
            // costs nothing at all -- the same property that makes ClickHouse's
            // `scoped` free. Unlike ClickHouse there is no silent fallback to a
            // `default`: an unqualified name with no default dataset is a hard
            // error, so this is `required` like MySQL's 1046.
            DbKind::Bigquery => NamespaceCaps {
                settable: true,
                required: true,
                label: "Dataset",
            },
            // Neither engine is SQL-shaped, so "the namespace an unqualified query
            // binds against" has no meaning: Redis addresses a logical DB index on
            // the connection, MongoDB names its database per request over
            // `DocDriver`. Both stay unsettable so any UI gated on this descriptor
            // (rather than on the connection kind) stays hidden.
            DbKind::Redis | DbKind::Mongo => NamespaceCaps {
                settable: false,
                required: false,
                label: "Database",
            },
        }
    }
}

/// How an engine resolves unqualified names, and whether RED can rebind that per
/// query (see [`DbKind::namespace_caps`]). A cheap `Copy` descriptor the UI reads
/// to gate the namespace picker without knowing anything about a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceCaps {
    /// RED can bind a namespace per query on this engine, so the UI may offer a
    /// picker. When false the namespace is fixed at connect and the picker is
    /// hidden entirely.
    pub settable: bool,
    /// Running with no namespace bound is an error the *engine* raises rather
    /// than a silent default. Drives the "you must pick one" affordance.
    pub required: bool,
    /// What to call this level in the UI: engines whose tree top level is a
    /// database say "Database", Postgres (whose top level is a schema) says
    /// "Schema".
    pub label: &'static str,
}

/// Whether a DSN string requests TLS — by scheme (`rediss`/`clickhouses`) or a
/// recognized query flag (`sslmode=<anything but disable>`, `require_ssl=true`,
/// `ssl=true`, `tls=true`). The single source of truth for "is this a TLS DSN",
/// shared by [`ConnectionConfig::parse_conn_str`] and the engine drivers'
/// cleartext-refusal guard, so the two can't disagree on which spellings count
/// (a driver matching only `sslmode=require` would silently connect in
/// cleartext for `?ssl=true` or `?sslmode=prefer`).
pub fn dsn_requests_tls(dsn: &str) -> bool {
    let Some((scheme, rest)) = dsn.trim().split_once("://") else {
        return false;
    };
    if DbKind::scheme_is_tls(scheme) {
        return true;
    }
    let q = rest
        .split(['?', '#'])
        .nth(1)
        .unwrap_or("")
        .to_ascii_lowercase();
    (q.contains("sslmode=") && !q.contains("sslmode=disable"))
        || q.contains("require_ssl=true")
        || q.contains("ssl=true")
        || q.contains("tls=true")
}

/// The write operations an engine can honor, independent of a connection's
/// `read_only` flag (see [`DbKind::write_caps`]). A cheap `Copy` descriptor the UI
/// reads to gate the edit affordances and the copy/migration target pickers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteCaps {
    /// Bulk `INSERT` / data import / copy-and-migrate *target* is possible.
    pub insert: bool,
    /// Transactional, guarded, exactly-one-row in-grid `UPDATE`/`DELETE` (the
    /// relational edit contract) is possible.
    pub guarded_edit: bool,
    /// Best-effort, non-atomic in-grid `UPDATE`/`DELETE` (async mutations, no
    /// rollback, no one-row guarantee) is possible.
    pub best_effort_edit: bool,
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbKind::Sqlite => write!(f, "SQLite"),
            DbKind::Postgres => write!(f, "PostgreSQL"),
            DbKind::Mysql => write!(f, "MySQL/MariaDB"),
            DbKind::Clickhouse => write!(f, "ClickHouse"),
            DbKind::Redis => write!(f, "Redis"),
            DbKind::Mongo => write!(f, "MongoDB"),
            DbKind::Bigquery => write!(f, "BigQuery"),
        }
    }
}

/// How RED authenticates to an SSH jump host. Mirrors DataGrip's three modes: a
/// running ssh-agent, a password, or an OpenSSH private-key file (optionally
/// passphrase-protected). The secrets these need (the password, the key
/// passphrase) are **never** serialized; like a connection's DB password they
/// live in the OS keychain and are materialized only transiently.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "mode", rename_all = "snake_case"))]
pub enum SshAuth {
    /// Reuse a running agent (ssh-agent / Pageant). Carries no secret of its own.
    #[default]
    Agent,
    /// Password authentication; the password lives in the keychain.
    Password,
    /// An OpenSSH private key at `path`, decrypted with a keychain-stored
    /// passphrase when the key is encrypted.
    Key { path: String },
}

/// Reach the database *through* an SSH jump host (the `ssh -L` model). When a
/// [`ConnectionConfig`] carries one of these, the service opens a local port
/// forward and points the driver at it; the connection's `host`/`port` are the
/// target as seen *from the jump host*. Secrets (`password`, `passphrase`) are
/// keychain-backed and never persisted, hence the redacting `Debug` and the
/// `serde(skip)` on those fields.
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Password for [`SshAuth::Password`]; empty otherwise. Keychain-backed,
    /// never serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub password: String,
    /// Passphrase for an encrypted [`SshAuth::Key`]; empty otherwise.
    /// Keychain-backed, never serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub passphrase: String,
}

impl fmt::Debug for SshConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("password", &redact(&self.password))
            .field("passphrase", &redact(&self.passphrase))
            .finish()
    }
}

/// The kind of proxy a [`ProxyConfig`] speaks: a SOCKS5 proxy (RFC 1928, with
/// optional RFC 1929 user/pass auth) or an HTTP `CONNECT` proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProxyKind {
    #[default]
    Socks5,
    HttpConnect,
}

impl ProxyKind {
    /// Every kind, in the order a picker lists them.
    pub const fn all() -> &'static [ProxyKind] {
        &[ProxyKind::Socks5, ProxyKind::HttpConnect]
    }

    /// The picker label for this kind.
    pub const fn label(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "SOCKS5",
            ProxyKind::HttpConnect => "HTTP CONNECT",
        }
    }
}

/// Reach the database *through* a forward proxy (SOCKS5 or HTTP `CONNECT`). Like
/// [`SshConfig`], when a [`ConnectionConfig`] carries one of these the service
/// resolves a local forward and points the driver at `127.0.0.1:<port>`; the
/// connection's `host`/`port` are the target reached *through the proxy*. A simpler
/// forward than SSH: no host-key dance, just the proxy handshake. The auth
/// `password` is keychain-backed and never persisted, hence the redacting `Debug`
/// and the `serde(skip)`.
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// Optional proxy-auth username; empty means unauthenticated.
    pub user: String,
    /// Proxy-auth password; empty when unauthenticated. Keychain-backed, never
    /// serialized.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub password: String,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &redact(&self.password))
            .finish()
    }
}

/// Which deployment a connection points at.
///
/// A safety guard is only ever a compromise between catching mistakes and getting
/// in the way, and the right compromise is not the same on a scratch database as on
/// the one customers are using. Without this, the strictness that makes sense for
/// production has to be endured everywhere, which is how people end up switching the
/// guards off entirely, on every connection at once.
///
/// [`Unset`](Self::Unset) is the default and behaves exactly as no marker at all, so
/// this can be added to `ConnectionConfig` without changing what any saved
/// connection does. The app never infers a value silently; the connection form
/// *offers* one (a `localhost` host suggests [`Local`](Self::Local), a name
/// containing "prod" suggests [`Prod`](Self::Prod)) and the user accepts or ignores
/// it. Guessing "production" wrongly would be as annoying as guessing "local" would
/// be dangerous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum ConnEnv {
    /// No marker: the guards behave exactly as configured globally.
    #[default]
    Unset,
    /// A scratch or throwaway database, where a mistake costs nothing.
    Local,
    /// A shared development database.
    Dev,
    /// Pre-production. Treated as `Dev` for confirmations; the distinction is for
    /// the user's own labelling.
    Staging,
    /// Production. Confirmations get stricter and cannot be switched off from a
    /// dialog.
    Prod,
}

impl ConnEnv {
    /// A short label for the connection list and the run bar. Empty for
    /// [`Unset`](Self::Unset), which should render no badge at all.
    pub fn label(self) -> &'static str {
        match self {
            ConnEnv::Unset => "",
            ConnEnv::Local => "Local",
            ConnEnv::Dev => "Dev",
            ConnEnv::Staging => "Staging",
            ConnEnv::Prod => "Prod",
        }
    }

    /// The value the connection form should *offer* for a connection with this host
    /// and name, or `None` when nothing about it is suggestive.
    ///
    /// Only ever a suggestion. A loopback host is strong evidence of a scratch
    /// database; "prod" in the name is weaker but worth raising, and erring toward
    /// the stricter reading is the safe direction for a guess the user can decline.
    pub fn suggest(host: &str, name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        if name.contains("prod") && !name.contains("non-prod") && !name.contains("nonprod") {
            return Some(ConnEnv::Prod);
        }
        if name.contains("staging") || name.contains("stage") {
            return Some(ConnEnv::Staging);
        }
        let host = host.trim().to_ascii_lowercase();
        if matches!(
            host.as_str(),
            "" | "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"
        ) {
            return Some(ConnEnv::Local);
        }
        if name.contains("dev") {
            return Some(ConnEnv::Dev);
        }
        None
    }
}

/// A saved connection target. Stored as structured fields rather than one opaque
/// DSN so the form can offer both entry modes and so engines stay swappable; the
/// driver-facing connection string is composed on demand by [`Self::dsn`]. For a
/// file engine the path lives in `database`; `host`/`port`/`user`/`password` are
/// unused. `color` is a label-palette index (UI-defined). `read_only` reflects
/// RED's read-mostly safety posture (enforced by the driver). An optional
/// [`ssh`](Self::ssh) tunnels the whole connection through a jump host.
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConnectionConfig {
    pub name: String,
    pub kind: DbKind,
    #[cfg_attr(feature = "serde", serde(default))]
    pub host: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub port: Option<u16>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub user: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub password: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub database: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub color: u8,
    /// When unset (read/write), the connection allows writes: deliberate `UPDATE`s
    /// from the SQL editor and guarded, PK-keyed, previewed in-grid cell edits
    ///. Read-only is the safe default: the driver opens read-only and
    /// every write path is refused up front.
    #[cfg_attr(feature = "serde", serde(default))]
    pub read_only: bool,
    /// Encrypt the connection with TLS. For Redis this dials `rediss://`; for MySQL it
    /// requires SSL (`require_ssl`); for ClickHouse it uses HTTPS. (A pasted
    /// `rediss://`/`clickhouses://` or an `sslmode=require`/`require_ssl` DSN also sets
    /// this.) The connection form surfaces it as a checkbox —.md`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tls: bool,
    /// Which deployment this connection points at. Drives how strict the write
    /// guards are: see `ConfirmPolicy` in the app. Defaults to
    /// [`ConnEnv::Unset`], which changes nothing, so an existing
    /// `connections.toml` keeps its exact behaviour.
    #[cfg_attr(feature = "serde", serde(default))]
    pub env: ConnEnv,
    /// Per-connection AI master-switch override. `None` inherits the global
    /// `[ai] enabled`; `Some(false)` is a true kill switch for *this* connection
    /// (no panel, no tools, no agent), e.g. a production database.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ai_enabled: Option<bool>,
    /// Per-connection AI access-tier override. `None` inherits the global
    /// `[ai] tier`; set it to pin a sensitive connection to `off`/`schema`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ai_tier: Option<AiTier>,
    /// Optional SSH jump host. When set, the service tunnels the connection
    /// through it (see [`SshConfig`]); `None` connects directly.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ssh: Option<SshConfig>,
    /// Optional forward proxy (SOCKS5 / HTTP CONNECT). When set, the service
    /// resolves a local forward through it (see [`ProxyConfig`]); `None` connects
    /// directly. Mutually exclusive with [`ssh`](Self::ssh) in v1 (rejected up
    /// front by the connect path).
    #[cfg_attr(feature = "serde", serde(default))]
    pub proxy: Option<ProxyConfig>,
    /// Redis Sentinel master group name. When set (Redis only), `host`/`port`
    /// name a Sentinel and the driver resolves the current master via
    /// `SENTINEL get-master-addr-by-name` (see `redis_kv.rs`'s `resolve_sentinel`);
    /// [`Self::dsn`] carries it as the `?master=` query the driver reads. Empty =
    /// a direct (non-Sentinel) connection.
    #[cfg_attr(feature = "serde", serde(default))]
    pub sentinel_master: String,
    /// MongoDB only: the connection string is a **DNS seedlist**
    /// (`mongodb+srv://cluster.example.net/`) rather than a host list.
    ///
    /// It cannot be recovered from the other fields, which is why it is stored:
    /// the SRV form carries no port (the driver looks the replica-set members up
    /// in DNS), so rebuilding the DSN as `mongodb://host:27017` pointed at a name
    /// that does not resolve to a mongod at all. That is every Atlas connection
    /// string. Implies TLS, which the driver reads off the scheme.
    #[cfg_attr(feature = "serde", serde(default))]
    pub srv: bool,
    /// Path to a BigQuery service-account key file. Empty selects **Application
    /// Default Credentials** (the `gcloud auth application-default login` file, or
    /// the GCE/Cloud-Run metadata server), which is the mode that needs no secret
    /// of RED's own at all.
    ///
    /// A *path*, deliberately, and therefore safe to persist: the key material
    /// stays in the file and the driver reads it at connect, so the
    /// "credentials never land in `connections.toml`" rule holds without
    /// stretching the keychain to cover a filename. Carried in the DSN as
    /// `?credentials_path=` (see [`Self::dsn`]).
    #[cfg_attr(feature = "serde", serde(default))]
    pub credentials_path: String,
}

/// Render a secret for `Debug`: `<unset>` when empty, `<redacted>` otherwise, so
/// a stray `{config:?}` can never spill a credential into the logs.
pub(crate) fn redact(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

/// Hand-written so the password is **never** printed: a redacting `Debug` makes
/// the "secrets stay out of logs" rule a compile-time guarantee rather than a
/// convention a stray `{config:?}` could break. (`Serialize` for persistence goes
/// through the password-free `WriteConnection`, so the disk path is already safe.)
impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &redact(&self.password))
            .field("database", &self.database)
            .field("color", &self.color)
            .field("read_only", &self.read_only)
            .field("tls", &self.tls)
            .field("ai_enabled", &self.ai_enabled)
            .field("ai_tier", &self.ai_tier)
            .field("ssh", &self.ssh)
            .field("proxy", &self.proxy)
            .field("sentinel_master", &self.sentinel_master)
            // A path, not a secret -- printable, unlike `password`.
            .field("credentials_path", &self.credentials_path)
            .finish()
    }
}

impl ConnectionConfig {
    /// The host to dial for a network engine, defaulting to `localhost` when the
    /// field is left blank. A blank host is the common "just connect to my local
    /// server" case, so we fill it in rather than reject it, matching the
    /// convention of clients like `psql` and `redis-cli`.
    pub fn effective_host(&self) -> &str {
        if self.host.is_empty() {
            "localhost"
        } else {
            &self.host
        }
    }

    /// The connection string handed to the driver. File engines yield the bare
    /// path; network engines compose `scheme://user:pass@host:port/database`, with
    /// the userinfo and database percent-encoded so credentials with reserved
    /// characters survive the round-trip.
    pub fn dsn(&self) -> String {
        if self.kind.is_file() {
            return self.database.clone();
        }
        // TLS is spelled per engine: a dedicated scheme where the client keys
        // off it (`rediss`/`clickhouses`), or a query flag the client honors
        // (`sslmode=require` for Postgres, `require_ssl=true` for MySQL —
        // `mysql_async` reads that natively).
        let scheme = match (self.kind, self.tls) {
            (DbKind::Redis, true) => "rediss",
            (DbKind::Clickhouse, true) => "clickhouses",
            // The DNS-seedlist form is a different scheme, not a flag on the
            // ordinary one.
            _ if self.kind == DbKind::Mongo && self.srv => "mongodb+srv",
            _ => self.kind.url_scheme(),
        };
        let mut url = format!("{scheme}://");
        // A username is optional even with a password set: Redis's classic
        // `AUTH <password>` (pre-ACL) form is a password-only credential, which
        // its URI convention spells as `redis://:password@host`. Emitting
        // nothing here for that case would silently drop the password.
        if !self.user.is_empty() || !self.password.is_empty() {
            url.push_str(&encode(&self.user));
            if !self.password.is_empty() {
                url.push(':');
                url.push_str(&encode(&self.password));
            }
            url.push('@');
        }
        // BigQuery's authority is a GCP project id, not a host. `effective_host`'s
        // `localhost` fallback would read back as a project *named* "localhost" and
        // fail with a baffling message, so an unset project stays empty and the
        // driver is the one that reports it.
        if self.kind == DbKind::Bigquery {
            url.push_str(&self.host);
        } else {
            url.push_str(&bracket_host(self.effective_host()));
        }
        // A `mongodb+srv` URI must carry no port: the port comes from the SRV
        // records, and the driver rejects the URI outright if one is written.
        if let Some(port) = self.port.filter(|_| !self.srv) {
            url.push(':');
            url.push_str(&port.to_string());
        }
        url.push('/');
        url.push_str(&encode(&self.database));
        // Postgres/MySQL carry TLS as a query flag rather than a scheme.
        if self.tls {
            match self.kind {
                DbKind::Postgres => url.push_str("?sslmode=require"),
                DbKind::Mysql => url.push_str("?require_ssl=true"),
                // The `mongodb` crate reads `tls=true` from the URI query.
                DbKind::Mongo => url.push_str("?tls=true"),
                _ => {}
            }
        }
        // Redis Sentinel: `?master=<group>` tells the driver to treat host/port as
        // a Sentinel and resolve the master. Redis spells TLS via the scheme, so
        // this is always the sole query param (no `?`/`&` conflict).
        if self.kind == DbKind::Redis && !self.sentinel_master.is_empty() {
            url.push_str("?master=");
            url.push_str(&encode(&self.sentinel_master));
        }
        // BigQuery: the service-account key file, spelled as
        // `sqlalchemy-bigquery` spells it (see `docs/adr/0005`). Empty means ADC.
        // Always the sole query param -- BigQuery is always TLS and never Sentinel
        // -- so there is no `?`/`&` conflict to resolve.
        if self.kind == DbKind::Bigquery && !self.credentials_path.is_empty() {
            url.push_str("?credentials_path=");
            url.push_str(&encode(&self.credentials_path));
        }
        url
    }

    /// The DSN for reaching this database through a local forwarded port: the
    /// normal [`dsn`](Self::dsn) but with `host`/`port` swapped for the tunnel's
    /// local endpoint. Reuses `dsn`'s userinfo/database encoding and IPv6
    /// bracketing, so the only difference is where the driver dials. Meaningful
    /// only for network engines; an SSH tunnel never fronts a file engine.
    pub fn local_dsn(&self, host: &str, port: u16) -> String {
        ConnectionConfig {
            host: host.to_string(),
            port: Some(port),
            ssh: None,
            proxy: None,
            ..self.clone()
        }
        .dsn()
    }

    /// A short human label for the connection's target (the file path, or
    /// `user@host:port/database`), shown on cards and in the status bar.
    pub fn display_target(&self) -> String {
        if self.kind.is_file() {
            return self.database.clone();
        }
        // BigQuery has no user and no host; its own notation for the pair that
        // identifies a connection is `project.dataset`.
        if self.kind == DbKind::Bigquery {
            return if self.database.is_empty() {
                self.host.clone()
            } else {
                format!("{}.{}", self.host, self.database)
            };
        }
        let mut s = String::new();
        if !self.user.is_empty() {
            s.push_str(&self.user);
            s.push('@');
        }
        s.push_str(&bracket_host(self.effective_host()));
        if let Some(port) = self.port {
            s.push(':');
            s.push_str(&port.to_string());
        }
        if !self.database.is_empty() {
            s.push('/');
            s.push_str(&self.database);
        }
        s
    }

    /// Decompose a pasted connection string into engine + fields, best-effort.
    /// Returns `None` if there's no recognizable `scheme://…`. Query strings and
    /// fragments are ignored; RED keeps "roughly enough" of the URL to refill the
    /// form, not every libpq option.
    pub fn parse_conn_str(input: &str) -> Option<ParsedDsn> {
        let input = input.trim();
        let (scheme, rest) = input.split_once("://")?;
        let kind = DbKind::from_scheme(scheme).unwrap_or(DbKind::Postgres);
        // TLS is signalled either by the scheme (`rediss`/`clickhouses`) or by a
        // recognized query flag — read it before the query tail is dropped below.
        let tls = dsn_requests_tls(input);
        // The ?query tail, kept before it is dropped below: most params describe
        // transport and are consumed by `dsn_requests_tls`, but BigQuery's
        // `credentials_path` is *configuration* and has to survive the paste.
        // A `location=` rides along unread on purpose -- the driver learns a job's
        // location from `jobReference.location` rather than being told -- so a DSN
        // carrying one parses rather than failing.
        let query = rest.split(['?', '#']).nth(1).unwrap_or("");
        // Drop any ?query / #fragment tail.
        let rest = rest
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_start_matches('/');

        if kind.is_file() {
            // `sqlite:///abs/path` → authority empty, path is the file. Hand the
            // whole remainder back as the database/path.
            return Some(ParsedDsn {
                kind,
                database: format!("/{}", rest.trim_start_matches('/')),
                ..ParsedDsn::for_kind(kind)
            });
        }

        // authority[/database]
        let (authority, database) = match rest.split_once('/') {
            Some((a, db)) => (a, db.to_string()),
            None => (rest, String::new()),
        };
        // [user[:pass]@]host[:port]
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((user, pass)) => (decode(user), decode(pass)),
                None => (decode(u), String::new()),
            },
            None => (String::new(), String::new()),
        };
        // Bracketed IPv6 literal (`[::1]` / `[::1]:5432`): split the port off after
        // the closing bracket so a colon *inside* the address isn't mistaken for the
        // port separator, and store the host without its brackets.
        let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
            match rest.split_once(']') {
                Some((addr, tail)) => (
                    addr.to_string(),
                    tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()),
                ),
                None => (rest.to_string(), None),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()),
                None => (hostport.to_string(), None),
            }
        };
        // The seedlist form carries no port at all; inventing the default one
        // would be written back into the DSN and rejected by the driver.
        let srv = scheme.eq_ignore_ascii_case("mongodb+srv");
        // `bigquery://project/location/dataset`, the three-segment form some Go
        // drivers use, is rejected rather than read as two segments: that reading
        // binds the *dataset* to the location and yields a wrong dataset with no
        // error at all. Declining the paste is the recoverable failure.
        if kind == DbKind::Bigquery && database.contains('/') {
            return None;
        }
        Some(ParsedDsn {
            kind,
            host,
            port: port.or_else(|| (!srv).then(|| kind.default_port()).flatten()),
            user,
            password,
            database: decode(&database),
            // `mongodb+srv` implies TLS.
            tls: tls || srv,
            srv,
            credentials_path: query_param(query, "credentials_path").unwrap_or_default(),
        })
    }
}

/// One `key=value` from a DSN's query tail, percent-decoded; `None` when absent.
/// Keys compare case-insensitively, values are decoded with the same [`decode`]
/// the userinfo path uses, so a Windows key-file path survives the round trip.
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        k.eq_ignore_ascii_case(key).then(|| decode(v))
    })
}

/// The fields recovered from a pasted connection string by
/// [`ConnectionConfig::parse_conn_str`].
///
/// `Debug` is hand-written to **redact the password**, matching every sibling
/// config type: a derived `Debug` over a `pub password: String` would print the
/// secret the moment any call site logged a `ParsedDsn`, defeating the deliberate
/// no-plaintext discipline.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedDsn {
    pub kind: DbKind,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Whether the DSN signalled TLS (a `rediss`/`clickhouses` scheme or an
    /// `sslmode`/`require_ssl`/`ssl=true` query flag), used to pre-check the
    /// form's TLS toggle.
    pub tls: bool,
    /// Whether the DSN used MongoDB's DNS-seedlist scheme (`mongodb+srv`), which
    /// has to survive the round-trip or an Atlas string becomes unusable.
    pub srv: bool,
    /// BigQuery's `?credentials_path=` (see
    /// [`ConnectionConfig::credentials_path`]); empty for every other engine.
    pub credentials_path: String,
}

impl std::fmt::Debug for ParsedDsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedDsn")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("database", &self.database)
            .field("tls", &self.tls)
            .field("srv", &self.srv)
            .field("credentials_path", &self.credentials_path)
            .finish()
    }
}

impl ParsedDsn {
    fn for_kind(kind: DbKind) -> Self {
        ParsedDsn {
            kind,
            host: String::new(),
            port: kind.default_port(),
            user: String::new(),
            password: String::new(),
            database: String::new(),
            tls: false,
            srv: false,
            credentials_path: String::new(),
        }
    }
}

/// Wrap an IPv6 literal host in `[...]` so the following `:port` separator stays
/// unambiguous; any other host (hostname / IPv4 / already-bracketed) is returned
/// unchanged. Mirrors [`ConnectionConfig::parse_conn_str`]'s bracket handling.
fn bracket_host(host: &str) -> std::borrow::Cow<'_, str> {
    if host.contains(':') && !host.starts_with('[') {
        std::borrow::Cow::Owned(format!("[{host}]"))
    } else {
        std::borrow::Cow::Borrowed(host)
    }
}

/// Percent-encode the characters that would otherwise be parsed as URL syntax
/// inside a userinfo/database component. Deliberately small: not a general
/// RFC 3986 encoder, just enough to keep credentials intact.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'@' | b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'%' | b' ' => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
            // A non-ASCII byte must be %-escaped too, never pushed `as char`:
            // byte 0xC3 is the *char* U+00C3, which re-encodes as two bytes —
            // a password `pässword` would reach every driver as mojibake and
            // authentication would simply fail.
            b if !b.is_ascii() => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
            _ => out.push(b as char),
        }
    }
    out
}

/// Reverse of [`encode`]: decode `%XX` escapes, leaving anything malformed as-is.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Both digits are checked as ASCII hex *before* slicing: a `%` followed by
        // the lead byte of a multi-byte character (`%aé`) would otherwise slice
        // across a char boundary and panic, and this runs on the pasted-DSN path
        // in the connection form, where the input is whatever the user pressed.
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod namespace_caps_tests {
    use super::*;

    /// An engine that *errors* without a bound namespace must also let RED bind
    /// one. The pair `required && !settable` would be a dead end by construction:
    /// the query fails, and the UI has nothing to offer — exactly the bug this
    /// descriptor exists to prevent (MySQL's "No database selected").
    #[test]
    fn required_namespaces_are_always_settable() {
        for kind in DbKind::all() {
            let caps = kind.namespace_caps();
            assert!(
                !caps.required || caps.settable,
                "{kind} requires a namespace but cannot bind one, which is a dead end"
            );
        }
    }

    /// The label is a UI noun ("Database"/"Schema"), so it must be non-empty and
    /// capitalized — it is interpolated raw into the picker and its messages.
    #[test]
    fn namespace_labels_are_presentable_nouns() {
        for kind in DbKind::all() {
            let label = kind.namespace_caps().label;
            assert!(!label.is_empty(), "{kind} has a blank namespace label");
            assert!(
                label.starts_with(|c: char| c.is_ascii_uppercase()),
                "{kind} label {label:?} should be capitalized"
            );
        }
    }
}

#[cfg(test)]
mod conn_tests {
    use super::*;

    #[test]
    fn env_suggestion_reads_host_and_name() {
        // A loopback host is strong evidence of a scratch database. A blank host is
        // the same thing: a file engine, or a form not filled in yet.
        assert_eq!(ConnEnv::suggest("localhost", "app"), Some(ConnEnv::Local));
        assert_eq!(ConnEnv::suggest("127.0.0.1", "app"), Some(ConnEnv::Local));
        assert_eq!(ConnEnv::suggest("", "scratch.db"), Some(ConnEnv::Local));
        // The name wins over the host: a tunnelled production database is still
        // production, and that is the reading that errs safely.
        assert_eq!(
            ConnEnv::suggest("localhost", "prod replica"),
            Some(ConnEnv::Prod)
        );
        assert_eq!(
            ConnEnv::suggest("db.internal", "API prod"),
            Some(ConnEnv::Prod)
        );
        // "non-prod" is not production, and would be an especially annoying thing to
        // guess wrong.
        assert_eq!(ConnEnv::suggest("db.internal", "non-prod"), None);
        assert_eq!(ConnEnv::suggest("db.internal", "nonprod api"), None);
        assert_eq!(
            ConnEnv::suggest("db.internal", "staging"),
            Some(ConnEnv::Staging)
        );
        assert_eq!(
            ConnEnv::suggest("db.internal", "dev box"),
            Some(ConnEnv::Dev)
        );
        // Nothing suggestive: say nothing rather than guess.
        assert_eq!(ConnEnv::suggest("db.internal", "analytics"), None);
    }

    #[test]
    fn dsn_composes_network_url() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "localhost".into(),
            port: Some(5432),
            user: "postgres".into(),
            password: "p@ss:word".into(),
            database: "analytics".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.dsn(),
            "postgres://postgres:p%40ss%3Aword@localhost:5432/analytics"
        );
    }

    /// A non-ASCII password must survive the DSN round trip byte for byte: each
    /// UTF-8 byte is %-escaped, and decoding restores the original text. The
    /// old `b as char` path re-encoded byte 0xC3 as two bytes — mojibake the
    /// server rejects as a wrong password.
    #[test]
    fn dsn_percent_encodes_non_ascii_credentials() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "localhost".into(),
            port: Some(5432),
            user: "renée".into(),
            password: "pässword".into(),
            database: "db".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.dsn(),
            "postgres://ren%C3%A9e:p%C3%A4ssword@localhost:5432/db"
        );
        assert_eq!(decode("p%C3%A4ssword"), "pässword");
        assert_eq!(decode(&encode("pässword")), "pässword");
    }

    #[test]
    fn dsn_composes_password_only_auth() {
        // Redis's classic `AUTH <password>` (pre-ACL, no username) must still
        // reach the driver: dropping it here would silently connect
        // unauthenticated instead of failing loudly.
        let cfg = ConnectionConfig {
            kind: DbKind::Redis,
            host: "localhost".into(),
            port: Some(6379),
            password: "hunter2".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "redis://:hunter2@localhost:6379/");
    }

    #[test]
    fn dsn_emits_sentinel_master_query_for_redis() {
        let cfg = ConnectionConfig {
            kind: DbKind::Redis,
            host: "sentinel.local".into(),
            port: Some(26379),
            sentinel_master: "mymaster".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "redis://sentinel.local:26379/?master=mymaster");
        // TLS composes: the scheme flips to rediss, master stays the sole query.
        let tls = ConnectionConfig {
            tls: true,
            ..cfg.clone()
        };
        assert_eq!(tls.dsn(), "rediss://sentinel.local:26379/?master=mymaster");
        // A non-Redis engine never emits it, even if the field is set.
        let pg = ConnectionConfig {
            kind: DbKind::Postgres,
            ..cfg
        };
        assert!(!pg.dsn().contains("master="));
    }

    #[test]
    fn blank_host_falls_back_to_localhost() {
        // Leaving the host empty is the common "connect to my local server" case;
        // both the DSN and the display label fill in `localhost` rather than
        // emitting an empty authority.
        let cfg = ConnectionConfig {
            kind: DbKind::Redis,
            host: String::new(),
            port: Some(6379),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "redis://localhost:6379/");
        assert_eq!(cfg.display_target(), "localhost:6379");
        assert_eq!(cfg.effective_host(), "localhost");
    }

    #[test]
    fn dsn_encodes_tls_per_engine() {
        let tls = |kind: DbKind| {
            ConnectionConfig {
                kind,
                host: "h".into(),
                port: Some(1),
                tls: true,
                ..Default::default()
            }
            .dsn()
        };
        assert_eq!(tls(DbKind::Redis), "rediss://h:1/");
        assert_eq!(tls(DbKind::Clickhouse), "clickhouses://h:1/");
        assert_eq!(tls(DbKind::Postgres), "postgres://h:1/?sslmode=require");
        assert_eq!(tls(DbKind::Mysql), "mysql://h:1/?require_ssl=true");
    }

    #[test]
    fn parse_detects_tls_from_scheme_and_query() {
        assert!(
            ConnectionConfig::parse_conn_str("rediss://h:6379/0")
                .unwrap()
                .tls
        );
        assert!(
            ConnectionConfig::parse_conn_str("clickhouses://h:8443/db")
                .unwrap()
                .tls
        );
        assert!(
            ConnectionConfig::parse_conn_str("postgres://h/db?sslmode=require")
                .unwrap()
                .tls
        );
        assert!(
            ConnectionConfig::parse_conn_str("mysql://h/db?require_ssl=true")
                .unwrap()
                .tls
        );
        // Plain schemes and an explicitly-disabled sslmode are not TLS.
        assert!(
            !ConnectionConfig::parse_conn_str("redis://h:6379/0")
                .unwrap()
                .tls
        );
        assert!(
            !ConnectionConfig::parse_conn_str("postgres://h/db?sslmode=disable")
                .unwrap()
                .tls
        );
    }

    #[test]
    fn tls_dsn_round_trips_through_parse() {
        // A TLS config's DSN, re-parsed, keeps the tls bit for each engine.
        for kind in [
            DbKind::Redis,
            DbKind::Clickhouse,
            DbKind::Postgres,
            DbKind::Mysql,
        ] {
            let cfg = ConnectionConfig {
                kind,
                host: "host".into(),
                port: Some(5),
                database: "db".into(),
                tls: true,
                ..Default::default()
            };
            let parsed = ConnectionConfig::parse_conn_str(&cfg.dsn()).unwrap();
            assert!(parsed.tls, "{kind:?} lost tls through parse");
            assert_eq!(parsed.kind, kind);
        }
    }

    #[test]
    fn dsn_for_file_is_the_path() {
        let cfg = ConnectionConfig {
            kind: DbKind::Sqlite,
            database: "/tmp/app.sqlite".into(),
            ..Default::default()
        };
        assert_eq!(cfg.dsn(), "/tmp/app.sqlite");
    }

    #[test]
    fn parse_network_dsn() {
        let p = ConnectionConfig::parse_conn_str("mysql://root:secret@db.host:3307/shop").unwrap();
        assert_eq!(p.kind, DbKind::Mysql);
        assert_eq!(p.host, "db.host");
        assert_eq!(p.port, Some(3307));
        assert_eq!(p.user, "root");
        assert_eq!(p.password, "secret");
        assert_eq!(p.database, "shop");
    }

    #[test]
    fn parse_dsn_defaults_port() {
        let p = ConnectionConfig::parse_conn_str("postgresql://localhost/mydb").unwrap();
        assert_eq!(p.kind, DbKind::Postgres);
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, Some(5432));
        assert!(p.user.is_empty());
        assert_eq!(p.database, "mydb");
    }

    #[test]
    fn parse_sqlite_dsn_keeps_path() {
        let p = ConnectionConfig::parse_conn_str("sqlite:///Users/me/data/app.db").unwrap();
        assert_eq!(p.kind, DbKind::Sqlite);
        assert_eq!(p.database, "/Users/me/data/app.db");
    }

    #[test]
    fn parse_rejects_non_url() {
        assert!(ConnectionConfig::parse_conn_str("not a url").is_none());
    }

    /// A malformed `%`-escape whose two-byte window cuts a multi-byte character
    /// used to slice off a char boundary and panic. This is the pasted-DSN path in
    /// the connection form, so the input is whatever the user typed.
    #[test]
    fn parse_survives_malformed_percent_escapes() {
        for dsn in [
            "postgres://user:%aé@host/db",
            "postgres://user:%é@host/db",
            "postgres://user:100%@host/db",
            "postgres://user:%zz@host/db",
        ] {
            let parsed = ConnectionConfig::parse_conn_str(dsn);
            assert!(parsed.is_some(), "{dsn} should parse, not panic");
        }
    }

    /// An Atlas connection string is a `mongodb+srv` DNS seedlist, and the
    /// seedlist form carries no port. Rebuilding it as `mongodb://host:27017`
    /// pointed at a name that resolves to no mongod at all - every Atlas
    /// connection failed to dial.
    #[test]
    fn a_mongodb_srv_dsn_round_trips() {
        let parsed = ConnectionConfig::parse_conn_str(
            "mongodb+srv://alice:pw@cluster0.abcd.mongodb.net/shop",
        )
        .expect("parses");
        assert_eq!(parsed.kind, DbKind::Mongo);
        assert!(parsed.srv, "the seedlist form has to survive the parse");
        assert!(parsed.tls, "mongodb+srv implies TLS");
        assert_eq!(parsed.port, None, "a seedlist URI carries no port");
        assert_eq!(parsed.host, "cluster0.abcd.mongodb.net");
        assert_eq!(parsed.database, "shop");

        let config = ConnectionConfig {
            kind: DbKind::Mongo,
            host: parsed.host.clone(),
            port: parsed.port,
            user: parsed.user.clone(),
            password: parsed.password.clone(),
            database: parsed.database.clone(),
            tls: parsed.tls,
            srv: parsed.srv,
            ..Default::default()
        };
        assert_eq!(
            config.dsn(),
            "mongodb+srv://alice:pw@cluster0.abcd.mongodb.net/shop?tls=true"
        );

        // The ordinary host-list form is unchanged, port and all.
        let plain = ConnectionConfig::parse_conn_str("mongodb://db:27018/shop").expect("parses");
        assert!(!plain.srv);
        assert_eq!(plain.port, Some(27018));
        let config = ConnectionConfig {
            kind: DbKind::Mongo,
            host: plain.host.clone(),
            port: plain.port,
            database: plain.database.clone(),
            ..Default::default()
        };
        assert_eq!(config.dsn(), "mongodb://db:27018/shop");
    }

    #[test]
    fn dsn_round_trips_through_parse() {
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "h".into(),
            port: Some(5432),
            user: "u".into(),
            password: "pw".into(),
            database: "d".into(),
            ..Default::default()
        };
        let p = ConnectionConfig::parse_conn_str(&cfg.dsn()).unwrap();
        assert_eq!(p.host, "h");
        assert_eq!(p.user, "u");
        assert_eq!(p.password, "pw");
        assert_eq!(p.database, "d");
    }

    #[test]
    fn local_dsn_swaps_only_host_and_port() {
        // The tunnel rewrite keeps userinfo/database (and their encoding) intact,
        // changing only where the driver dials.
        let cfg = ConnectionConfig {
            kind: DbKind::Postgres,
            host: "remote.internal".into(),
            port: Some(5432),
            user: "u".into(),
            password: "p@ss".into(),
            database: "shop".into(),
            ssh: Some(SshConfig {
                host: "bastion".into(),
                port: 22,
                user: "jump".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            cfg.local_dsn("127.0.0.1", 49152),
            "postgres://u:p%40ss@127.0.0.1:49152/shop"
        );
    }

    #[test]
    fn ssh_debug_redacts_secrets() {
        let ssh = SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "jump".into(),
            auth: SshAuth::Password,
            password: "hunter2".into(),
            passphrase: "swordfish".into(),
        };
        let shown = format!("{ssh:?}");
        assert!(!shown.contains("hunter2"), "ssh password leaked into Debug");
        assert!(
            !shown.contains("swordfish"),
            "ssh passphrase leaked into Debug"
        );
        assert!(shown.contains("<redacted>"));
    }
}
