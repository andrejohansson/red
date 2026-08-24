# RED

A native database explorer. RED's domain is *reading and editing data across
engines that disagree about almost everything* — so the language here is
deliberately engine-neutral: each term names the RED concept, and notes how each
engine happens to spell it.

## Language

**Engine**:
A database technology RED can talk to (PostgreSQL, SQLite, MySQL, ClickHouse,
Redis, MongoDB, BigQuery). Named by `DbKind`.
_Avoid_: Database (ambiguous with Namespace), backend, dialect, vendor

**Driver**:
The implementation that speaks one Engine's protocol. The only seam between RED
and an Engine.
_Avoid_: Adapter, client, connector

**Connection**:
A saved, named target the user connects to — credentials, transport, and the
Engine, plus the safety posture (read-only, environment). One Connection binds
exactly one Engine.
_Avoid_: Session (that's the live one), datasource, profile

**Namespace**:
The level of an Engine's hierarchy that RED draws as the schema tree's top level
and that an *unqualified* name in a query binds against. A schema on PostgreSQL,
a database on MySQL and ClickHouse, a **dataset** on BigQuery.
_Avoid_: Schema, database (both are one Engine's spelling of it)

**Project** (BigQuery):
The GCP billing and access boundary that contains Datasets. Above Namespace in
BigQuery's hierarchy, and RED has no general concept for that level, so a
BigQuery Connection pins exactly one Project.
_Avoid_: Account, tenant, organization

**Dataset** (BigQuery):
BigQuery's spelling of Namespace — a named container of tables within one
Project.
_Avoid_: Schema, database

**Credentials path** (BigQuery):
The filesystem location of a service-account key file on a BigQuery Connection.
Empty means Application Default Credentials. RED stores the *path*; the key
material stays on disk and never enters `connections.toml` or the keyring.
_Avoid_: Key, secret, service account (that's the identity, not the file)

**Job** (BigQuery):
One submitted unit of BigQuery work, billed by bytes scanned. RED's unit of
caching and cancellation: a result's rows are re-read from the Job that produced
them rather than by re-running its SQL.
_Avoid_: Query (that's the SQL text), request, statement
