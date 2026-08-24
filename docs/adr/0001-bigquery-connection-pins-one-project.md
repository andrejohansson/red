# A BigQuery connection pins exactly one project

BigQuery's hierarchy is project → dataset → table, but RED's `TableRef` and
`SchemaMeta` model exactly two levels and the schema tree's top level *is* the
Namespace. Rather than add a third level to `red-core` (touching all six existing
drivers and every call site for one engine's benefit), a BigQuery Connection pins
one project and the Dataset becomes the Namespace — the same shape as Postgres,
where the database is fixed at connect and the *schema* is the rebindable
Namespace.

## Consequences

- The schema tree shows one project's datasets. A user working across several
  projects makes several Connections.
- Cross-project queries still work, because they need no RED support: a
  fully-qualified `` `other-proj.ds.tbl` `` typed in the editor is passed through
  untouched. They are simply invisible to the tree.
- `list_objects` stays one `INFORMATION_SCHEMA.SCHEMATA` read, keeping its
  documented cheapness contract. Enumerating projects would have required the
  Resource Manager API — a different API with a different scope that org policy
  frequently denies — plus one catalog query per project.
