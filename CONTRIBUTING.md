# Contributing to RED

Thanks for your interest in RED (*Roughly Enough Data*), a fast, native database
explorer in pure Rust on GPUI. This guide covers setup, the checks a change has
to pass, and how to open a pull request.

## Project values

RED prioritises speed, low memory, and good behaviour on large result sets over
feature-completeness. Changes are weighed against those values first. The
canonical coding guide is [`docs/conventions.md`](docs/conventions.md); read it
before your first change.

## Prerequisites

- **Rust** ≥ 1.96 (the workspace `rust-version`). Install via [rustup](https://rustup.rs).
- **macOS:** a full **Xcode** toolchain. GPUI compiles Metal shaders, which the
  Command Line Tools alone can't do. If `xcode-select -p` points at
  `CommandLineTools`, set:

  ```sh
  export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
  ```

- **[`just`](https://github.com/casey/just)** (optional but recommended):
  `brew install just`. It wraps the common tasks; run `just` to list them.

## Build & run

A fresh clone builds with no extra setup. Flint (the shared UI library) is a
pinned **git dependency**, so Cargo fetches it for you:

```sh
cargo run -p red        # or: just run
```

## The pre-push gate

Every change must be green under format, lint, and tests before it's pushed:

```sh
just check              # = fmt + lint + test
```

or the raw commands:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test --workspace
```

CI runs the same three checks on every pull request; a PR can't merge until they
pass.

The Linux and Windows builds are *not* on that path — a cold cache makes them a
full GPUI compile, which dominated PR wall-clock. They run weekly (and on demand
from the Actions tab) as
[`cross-platform.yml`](.github/workflows/cross-platform.yml). If your change
touches platform-specific code, kick that workflow by hand rather than waiting
for the release build to find it.

## Working on Flint and RED together

Flint is pinned by git rev in [`Cargo.toml`](Cargo.toml) so outside contributors
build without a sibling checkout. When you need to change a Flint component *and*
RED at the same time, check Flint out next to RED and point Cargo at your local
copy with a patch in the **workspace** `Cargo.toml` (do not commit this):

```toml
[patch."https://github.com/vojir-mikulas/flint"]
flint = { path = "../flint" }
```

New shared components are built **gallery-first in Flint** with a generic,
domain-free API. Spike the usage in RED first so the API is right, then push it
down into Flint (`just gallery` runs Flint's component gallery).

### The single-`gpui` contract

RED and Flint both pin `gpui` to the same Zed revision. Cargo only unifies two
git deps when their URL and rev match, so a mismatch silently pulls in a second,
incompatible `gpui`. Bumping the rev (or the Flint pin) is a coordinated change
across both repos, not something to do casually in a feature PR.

## Architecture in brief

- **`red`**: the GPUI application binary; renders all UI on the main thread, and
  hosts the headless CLI (`red query`, `exec`, …).
- **`red-core`**: shared domain types (`Value`, `QueryResult`, `RedError`, …)
  and the SQL/KV/document vocabularies, with no UI or runtime knowledge.
- **`red-driver`**: the three engine seams and their implementations —
  `DatabaseDriver` (SQLite, PostgreSQL, MySQL/MariaDB, ClickHouse), `KvDriver`
  (Redis/Valkey) and `DocDriver` (MongoDB). The *only* seam to a database engine.
- **`red-service`**: the Tokio backend thread and the `Command`/`Event` bridge,
  plus the AI tool gate and the MCP/ACP hosts.
- **`red-config`**: the saved-connection list, the OS keychain, and the
  query/saved-query/plan stores. UI-free, so the CLI reads the same files.
- **`red-ai`**: the Claude Messages API client (streaming, tool use).
- **`red-acp`**: the Agent Client Protocol client, for a Claude subscription
  agent run as a subprocess.

The UI never blocks on the backend: it sends `Command`s and observes `Event`s
over channels. The UI speaks `red-core` types and Commands/Events, **never**
driver internals.

## Conventions worth calling out

These come up most in review (full list in [`docs/conventions.md`](docs/conventions.md)):

- **Comments explain *why*, not *what***, and stay free of milestone/ticket
  references. Keep planning context out of the code.
- **`thiserror` in libraries, `anyhow` at the app edge.** Use `tracing` for
  diagnostics; no `println!`/`eprintln!` in shipped paths.
- **Build the UI with Flint** and semantic theme tokens (`cx.theme().bg_app`),
  not raw colors.
- **Stream large results.** Never materialize a whole result set by default;
  results flow through a windowed cursor behind `DatabaseDriver`.
- **Safe by default.** Confirm destructive queries; honour read-only
  connections and query timeouts.

## Pull requests

1. Branch off `main`.
2. Keep the change focused; match the style of the code around it.
3. Run `just check`; it must be green.
4. Write a clear PR description: what changed and why. Conventional-commit-style
   subjects (`feat:`, `fix:`, `refactor:`, `chore:`) are appreciated.

## License

By contributing, you agree your contributions are licensed under
**GPL-3.0-or-later**, the project's license (see [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE)).
