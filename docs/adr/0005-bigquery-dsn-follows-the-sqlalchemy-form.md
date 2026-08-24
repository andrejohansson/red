# The BigQuery DSN follows the sqlalchemy-bigquery URL form

BigQuery has no official connection-string format. The two established
conventions are the Simba JDBC/ODBC form (semicolon key/value:
`jdbc:bigquery://…;ProjectId=…;OAuthType=0;OAuthPvtKeyPath=…;DefaultDataset=…`,
which DBeaver and DataGrip use) and `sqlalchemy-bigquery`'s URL form
(`bigquery://<project>/<dataset>?credentials_path=…`). RED follows the
**SQLAlchemy form**: it is the only one that is URL-shaped, and
`ConnectionConfig::dsn()` / `parse_conn_str` compose and parse URLs.

This is why the project occupies `ConnectionConfig::host`. It is not a squeeze
into a spare field — the project is the connection's authority, the "where", and
the convention independently puts it in the authority slot. The Dataset is
`database`, which is already the Namespace. `credentials_path` is the one new
field, named after the query parameter.

## Consequences

- The connection form needs a per-engine label for the host field ("Project"),
  the same way `NamespaceCaps::label` already renames the namespace level per
  engine.
- `location=` is accepted on parse although RED does not need it (location is
  read back from `jobReference.location`): pasted DSNs carry it, and silently
  dropping a parameter someone set is worse than honouring it.
- A three-segment path (`bigquery://project/location/dataset`, used by some Go
  drivers) is **rejected explicitly**. Under the two-segment reading it would
  silently bind the dataset to the location — a wrong dataset with no error.
- `credentials_base64` is not supported. It exists to inline a key into a CI env
  var; honouring it in a TOML connection file would contradict the rule that
  credentials never land in `connections.toml`.
- The `tls` flag is meaningless (BigQuery is always HTTPS), so the form hides the
  toggle rather than offering one that does nothing.
- Importing a BigQuery connection from DBeaver/DBGate/DataGrip means translating
  the JDBC form's `ProjectId` / `OAuthPvtKeyPath` / `DefaultDataset` / `OAuthType`
  into this shape. Deferred; it is mechanical but needs its own fixtures.
