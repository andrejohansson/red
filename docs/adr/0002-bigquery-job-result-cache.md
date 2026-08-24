# The BigQuery driver caches jobs by SQL text, for ~60 seconds

`DatabaseDriver`'s read seams are stateless: `count`, `fetch_page`, and
`key_bounds` each take a SQL string, not a shared result handle, and the browse
open probe fires all three concurrently before the user scrolls. On every other
engine a re-read is merely cheap; on BigQuery each one is a **job billed per byte
scanned**, so opening one tab on a large table and scrolling would bill a full
scan per page.

BigQuery's `jobs.getQueryResults` accepts `startIndex` + `maxResults` over a
*completed* job's result set at no additional scan cost, which is exactly what
`fetch_page(offset, limit)` wants — so the driver keeps a bounded
`(dataset, sql) → job id` cache and turns repeat reads of the same result into
free random access into the job it already ran.

## Consequences

- The TTL is a **correctness** parameter, not a performance one. RED's refresh
  path re-opens a result with identical SQL, so a long TTL would silently serve
  stale rows with nothing in the UI to indicate it. ~60s is long enough for the
  open probe and scrolling to share one job, short enough that any deliberate
  human refresh re-runs the query. Wrong in the safe direction costs one extra
  scan; wrong in the other direction shows false data.
- BigQuery's own 24h query cache does not substitute for this: `LIMIT`/`OFFSET`
  makes each page a textually distinct query and therefore a cache miss, and its
  hits are not guaranteed at all for wildcard tables or non-deterministic
  functions.
- `open_cursor` does not use the cache. A streaming read holds its own job and
  walks `pageToken`, and out-of-band cancel is `jobs.cancel` — structurally the
  same second-request trick as ClickHouse's `KILL QUERY WHERE query_id`.
