# Query cost is a guarded resource on BigQuery

RED's existing guards all police *danger*: `read_only`, `ConnEnv`-graded
destructive confirms, `AiLimits` bounding what the assistant can pull. BigQuery
adds an axis none of them cover — a non-destructive `SELECT *` that bills $40. It
is guarded in two tiers, both driven by BigQuery's `dryRun`, which returns
`totalBytesProcessed` without running anything and at no cost.

**A hard ceiling.** Every job carries `maximumBytesBilled` from a settings key,
and the default is **set, not unlimited** (~100 GB). A query over the cap fails
before it runs, with an error naming both the cap and the estimate. A
conservative default is deliberate rather than matching every other engine's
unlimited behaviour: "safe by default" applies to money as readily as to
`DROP TABLE`, and enforcing it at the engine — rather than by RED remembering to
check — is the posture `read_only` already takes.

**A confirmation below it.** When the dry-run estimate exceeds a lower
`confirm_bytes` threshold (~20 GB), the query is not executed; the service emits
a cost-confirm event and stops. Approving re-sends the query with a flag that
skips the gate.

The threshold is set high on purpose. `sql/risk.rs` puts the reason plainly: "a
prompt that fires on ordinary work trains the reflex that dismisses it, and
pressures the user into switching the rail off entirely." A cost prompt that
fires daily is worse than none.

## Consequences

- The three tiers read as one model: at or under `confirm_bytes` it runs; between
  the two it prompts; over `maximumBytesBilled` the engine refuses.
- **Approval is a re-send, not a resume.** The dry run happens inside the
  driver's execute path, so the service never holds a suspended command and the
  UI keeps its existing "decide, then send" posture. The cost is one extra HTTP
  call in the driver's already-authenticated context, not a UI↔service round
  trip.
- The gate covers user-initiated execution only, never the browse open probe's
  internal legs.
- `explain(analyze: false)` is the same dry run, which honours the trait doc's
  promise that plain `explain` "never executes the statement". `analyze: true`
  runs the job and maps `statistics.query.queryPlan` stages into `PlanNode`s.
  BigQuery has no textual `EXPLAIN`, so this is also the only plan available —
  and it means the query-plan panel reports bytes-to-be-scanned with no new UI.
- The confirm dialog itself needs no new interaction pattern. It slots beside the
  existing `CountMatching` row-count preflight and `AssessSql` review as a third
  token-matched enrichment. The one difference is *why* it opens: those enrich a
  dialog already opened by local grading, whereas cost cannot be graded locally
  at all — identical SQL scans 4 KB or 4 TiB — so here the estimate is what
  decides whether to prompt.
- `QueryOptions`' statement timeout maps to `jobTimeoutMs`, so query timeouts
  work through the mechanism already in place.
