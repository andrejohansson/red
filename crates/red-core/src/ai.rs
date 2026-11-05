//! The AI assistant's access policy and its activity timeline.
//!
//! Two related vocabularies. The **policy** ([`AiTier`], [`AiLimits`],
//! [`AiPolicy`]) is what every gate in `red-service` reads to decide what the
//! assistant may reach; it lives here rather than beside those gates because the
//! settings layer, the connection list and the UI all have to speak it too. The
//! **timeline** ([`ActivityNode`] and friends) is what a turn emits as it works,
//! so the panel can show what the agent did rather than only what it said.

/// How much of the connected database the AI assistant's tools may reach. The
/// tier is enforced where the MCP tool catalog is *constructed*: a tool above
/// the tier simply isn't offered, so the model (on either the API-key or the
/// subscription/ACP backend) can't call something the tier withholds. It is a
/// least-privilege ladder: each rung adds to the one below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum AiTier {
    /// No DB tools at all; the assistant chats without database grounding.
    Off,
    /// Structure only: `list_schema` + `describe_table`. The model sees tables,
    /// columns, types, and keys but never reads a row of data.
    Schema,
    /// The full read catalog: adds `run_select` and `explain`, subject to the
    /// resource guards in [`AiLimits`]. Honors a connection's `read_only` posture.
    /// The shipped default.
    #[default]
    Read,
    /// Read **plus** the gated write tool (`propose_write`): a single
    /// INSERT/UPDATE/DELETE that requires explicit, per-statement user approval and
    /// is blocked on a read-only connection and for destructive shapes (DDL,
    /// unqualified UPDATE/DELETE). Opt-in only: set globally (`[ai] tier = "write"`)
    /// or per-connection (`ai_tier = "write"`); never a default.
    Write,
}

impl AiTier {
    /// Whether the named tool is exposed at this tier. The single source of truth
    /// for catalog membership, so both backends gate identically.
    pub fn allows_tool(self, tool: &str) -> bool {
        match self {
            AiTier::Off => false,
            // Structure-only tools, both backends. For Redis (KV) these read
            // metadata (types/TTL/sizes/server info) but never a key's value.
            AiTier::Schema => matches!(
                tool,
                "list_schema"
                    | "describe_table"
                    // Structure, not data: the declared FK graph and an object's
                    // own definition are catalog reads, never a row.
                    | "relationship_map"
                    | "object_ddl"
                    // Retrieval from what the user already ran or saved. Available
                    // at Schema on purpose: this is *user-authored* SQL, not row
                    // data. A WHERE clause can embed a literal, so history can leak
                    // a value — which is why both stores are `0o600`. If that trade
                    // is unacceptable for a deployment, Schema is the wrong tier and
                    // the right answer is `off`.
                    | "search_query_history"
                    | "list_saved_queries"
                    | "read_saved_query"
                    | "kv_recent_keys"
                    | "kv_server_info"
                    | "kv_scan_keys"
                    | "kv_key_info"
                    // Key *names* segmented into templates: no value is read.
                    | "kv_key_schema"
                    // MongoDB (doc) metadata tools: catalog + inferred schema, no
                    // document reads.
                    | "doc_server_info"
                    | "list_collections"
                    | "describe_collection"
            ),
            AiTier::Read => matches!(
                tool,
                "list_schema"
                    | "describe_table"
                    | "relationship_map"
                    | "object_ddl"
                    | "search_query_history"
                    | "list_saved_queries"
                    | "read_saved_query"
                    | "kv_recent_keys"
                    | "run_select"
                    // Continues a `run_select`, so it returns the same class of
                    // data and sits at the same tier.
                    | "fetch_more"
                    | "search_data"
                    | "explain"
                    | "profile_table"
                    | "health_report"
                    | "server_sessions"
                    | "diff_schema"
                    | "diff_data"
                    // Composes explain + describe_table and emits candidate DDL
                    // as text; creating it is `create_index`'s job, at Write.
                    | "suggest_index"
                    // Streams a read to a file for the user. A read, and offered
                    // by all three seams with per-seam arguments.
                    | "export_result"
                    | "generate_report"
                    | "open_query"
                    | "save_query"
                    // Drafts the connection's knowledge file for the user to
                    // review. Read tier is the floor on purpose: at Schema the
                    // agent cannot sample a single value, so its "glossary" would
                    // be column-name inference — the failure mode the knowledge
                    // file exists to fix.
                    | "save_knowledge"
                    // Delegation grants no new capability — a subagent runs a
                    // read-only subset of this same tier — so it rides with Read.
                    | "spawn_subagent"
                    // Redis (KV) read tools: the metadata tools from Schema plus
                    // value/sample/diagnostic reads. All read-only.
                    | "kv_server_info"
                    | "kv_scan_keys"
                    | "kv_key_info"
                    | "kv_key_schema"
                    | "kv_get_value"
                    | "kv_read_collection"
                    // RedisJSON reads: one path's value, and the paths+types
                    // map that reads no values at all.
                    | "kv_json_get"
                    | "kv_json_shape"
                    | "kv_stream_groups"
                    | "kv_biggest_keys"
                    | "kv_analyze"
                    | "kv_slowlog"
                    | "kv_client_list"
                    | "kv_config_get"
                    | "kv_keyspace_notifications"
                    // MongoDB (doc) read tools: the metadata tools from Schema plus
                    // document/sample/analytical reads. All read-only.
                    | "doc_server_info"
                    | "list_collections"
                    | "describe_collection"
                    // Samples field *values* to test each reference, so it is a
                    // read tool rather than a Schema one despite mapping structure.
                    | "doc_reference_map"
                    | "profile_collection"
                    | "sample_documents"
                    | "get_document"
                    | "find"
                    | "aggregate"
                    | "count"
                    | "distinct"
                    | "explain_query"
                    | "index_advice"
                    | "audit_collection"
                    | "doc_current_op"
            ),
            // Write inherits the full read catalog and adds the gated write tools:
            // SQL (a single statement or a transactional changeset) and Redis
            // (gated key mutations). Every one rides the per-call approval gate.
            AiTier::Write => {
                matches!(
                    tool,
                    "propose_write"
                        | "propose_changeset"
                        // Not a data write, but a server-state one: stopping a
                        // session rolls back its transaction, so it rides the
                        // same tier and the same per-call approval.
                        | "kill_session"
                        // The one DDL the agent may run: additive and reversible,
                        // unlike the DROP/TRUNCATE/ALTER that stay blocked.
                        | "create_index"
                        | "kv_set"
                        | "kv_json_set"
                        | "kv_expire"
                        | "kv_delete"
                        | "kv_rename"
                        | "kv_copy_key"
                        | "kv_config_set"
                        | "kv_client_kill"
                        // Read-only by allowlist, gated as a write on purpose.
                        | "kv_command"
                        // MongoDB (doc) gated writes.
                        | "propose_doc_write"
                        | "propose_index"
                        | "propose_collection_op"
                        | "doc_kill_op"
                ) || AiTier::Read.allows_tool(tool)
            }
        }
    }

    /// A short label for status lines and logs.
    pub fn label(self) -> &'static str {
        match self {
            AiTier::Off => "off",
            AiTier::Schema => "schema",
            AiTier::Read => "read",
            AiTier::Write => "write",
        }
    }

    /// Parse a settings string. Recognized tiers map directly; an **unrecognized**
    /// value fails **closed** to [`AiTier::Off`] rather than to a permissive tier;
    /// a typo like `"readonly"` or `"scema"` when locking the assistant down must
    /// not silently grant row-data (`read`) access. `write` is accepted both
    /// globally (`[ai] tier`) and per-connection (`ai_tier`); it only ever grants
    /// the write tool on a writable connection, gated by approval.
    pub fn parse(s: &str) -> AiTier {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => AiTier::Off,
            "schema" => AiTier::Schema,
            "read" => AiTier::Read,
            "write" => AiTier::Write,
            _ => AiTier::Off,
        }
    }
}

/// Resource guards on the `read` tier: defense in depth so neither backend can
/// make the assistant read 1M rows by accident or hang the session thread. They
/// are enforced server-side in the tool layer, mirroring the windowed-cursor and
/// fat-cell caps the human-facing paths already carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiLimits {
    /// Hard ceiling on rows one `run_select` may return; a larger requested
    /// `LIMIT` is clamped and the truncation reported back to the model.
    pub max_rows: usize,
    /// Per-tool-call statement timeout. `0` disables it.
    pub statement_timeout_ms: u64,
    /// Cap on the size of one tool result handed back to the model; a larger
    /// result is truncated (and the truncation noted) so context can't balloon.
    pub max_result_bytes: usize,
    /// Cap on tool calls per conversation, bounding a runaway agent loop. `0`
    /// disables the cap.
    pub max_tool_calls: usize,
    /// Ceiling on the tokens one model reply may generate. Unlike the other
    /// guards this one is a *budget*, not a safety rail: too low and a long
    /// answer or a generated report is cut off mid-sentence.
    ///
    /// Current models allow far more than the default, but output tokens are the
    /// expensive ones, so the default is roomy rather than maximal and the
    /// ceiling is the user's to raise.
    pub max_output_tokens: u32,
}

impl Default for AiLimits {
    fn default() -> Self {
        Self {
            max_rows: 1000,
            statement_timeout_ms: 15_000,
            max_result_bytes: 256 * 1024,
            max_tool_calls: 100,
            max_output_tokens: 16_384,
        }
    }
}

/// The resolved AI access policy for one turn: the master switch, the access
/// tier, and the resource guards. Built by layering a connection's optional
/// overrides over the global `[ai]` settings (see [`Self::with_overrides`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiPolicy {
    /// The master switch. When `false` the assistant is a true no-op: no panel
    /// entry, no tools, and, critically, no MCP server or agent process.
    pub enabled: bool,
    pub tier: AiTier,
    pub limits: AiLimits,
    /// The connection's read-only posture, carried into the tool layer so the write
    /// tool is withheld (and rejected, defense in depth) on a read-only connection;
    /// the same guard the human write path is held to. Authoritative (set from the
    /// session), not the UI-supplied `AiContext.read_only`.
    pub read_only: bool,
    /// Whether to count the rows a proposed write would affect and show them with
    /// the approval prompt. Only ever consulted at [`AiTier::Write`], since that
    /// is the only tier where a write can be proposed at all.
    pub preview_writes: bool,
    /// How long an agent sandbox transaction may sit unresolved before RED rolls
    /// it back. An open transaction holds locks, so a user who walks away
    /// mid-review can block production writes; rolling back is the only defensible
    /// expiry, since committing unasked is not a thing RED does.
    pub sandbox_timeout_secs: u64,
}

impl Default for AiPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            tier: AiTier::Read,
            limits: AiLimits::default(),
            read_only: false,
            preview_writes: true,
            sandbox_timeout_secs: 120,
        }
    }
}

impl AiPolicy {
    /// Layer a connection's optional overrides over this (global) policy. A set
    /// override tightens (or loosens) the field for that connection; an unset one
    /// inherits the global value, so an unconfigured connection gets the global
    /// default, and a sensitive one can be pinned `off`/`schema` without touching
    /// the global setting.
    pub fn with_overrides(self, enabled: Option<bool>, tier: Option<AiTier>) -> AiPolicy {
        AiPolicy {
            enabled: enabled.unwrap_or(self.enabled),
            tier: tier.unwrap_or(self.tier),
            limits: self.limits,
            read_only: self.read_only,
            preview_writes: self.preview_writes,
            sandbox_timeout_secs: self.sandbox_timeout_secs,
        }
    }
}

/// A stable id for a node in an assistant turn's activity timeline. On the
/// direct-provider path it is the provider's `tool_use` id; on the ACP path it is
/// the agent's `tool_call_id`. Opaque; only used to correlate a node's start with
/// its later status updates and to attach children to their parent. A newtype (not
/// a bare `String`) so it can't be transposed with the other string ids that flow
/// through the delta stream; `serde(transparent)` keeps its persisted form a plain
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct ActivityId(String);

impl ActivityId {
    /// Borrow the underlying id string (for UI element keys, formatting, matching).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ActivityId {
    fn from(s: String) -> Self {
        ActivityId(s)
    }
}

impl From<&str> for ActivityId {
    fn from(s: &str) -> Self {
        ActivityId(s.to_string())
    }
}

impl std::fmt::Display for ActivityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one [`ActivityNode`] represents. The agent's work is a tree of these:
/// tool calls, delegated subagents, and proposed writes. New kinds extend the
/// agent's legible surface without touching the delta plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "kind", rename_all = "snake_case"))]
pub enum ActivityKind {
    /// A tool call: the tool `name` plus a one-line summary of its arguments (the
    /// SQL's first line, the table name), shown so the trace reads without expanding.
    Tool {
        name: String,
        #[cfg_attr(feature = "serde", serde(default))]
        args_summary: Option<String>,
    },
    /// A delegated subagent working on `task`; its own tool calls are this node's
    /// `children` (Phase 1). Kept flat until the ACP path nests them.
    Subagent { task: String },
    /// A proposed or executed write statement, surfaced verbatim for review.
    Write { sql: String },
    /// A standalone HTML report the `generate_report` tool wrote to `path`. Rendered
    /// as a card with an "Open" button rather than auto-opened, so the report stays
    /// in the transcript and the user chooses when to open it in their browser.
    Report {
        path: String,
        #[cfg_attr(feature = "serde", serde(default))]
        title: Option<String>,
    },
    /// `dropped` earlier tool results were replaced with a one-line stub to keep
    /// the conversation inside the model's context window.
    ///
    /// Shown rather than done quietly: silent context loss is how an agent starts
    /// contradicting itself with no visible cause, and this node persists with the
    /// conversation so a later reader can see where the detail went.
    Compacted { dropped: usize },
}

/// The lifecycle of an [`ActivityNode`]. Drives the status glyph and whether a
/// spinner shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ActivityStatus {
    /// Created but not yet running (e.g. a write awaiting approval).
    Pending,
    /// Executing now.
    Running,
    /// Finished successfully.
    Ok,
    /// Finished successfully, but the result carries a caveat the reader should
    /// see: a query that ran fine and may answer a different question than it
    /// looks like it asks. Distinct from [`Failed`](Self::Failed) because nothing
    /// went wrong, and distinct from [`Ok`](Self::Ok) because a glance at the
    /// timeline should show that something was flagged.
    Warned,
    /// Finished with an error (`detail` carries the message).
    Failed,
    /// The user denied it; it never ran.
    Denied,
}

/// One node in an assistant turn's activity timeline — a tool call, a subagent, or
/// a proposed write. Nodes nest via `children`: a subagent's inner tool calls are
/// its children, which is what lets the panel draw a delegation as one collapsible
/// card rather than a flat run of unrelated tool lines.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ActivityNode {
    pub id: ActivityId,
    pub kind: ActivityKind,
    pub status: ActivityStatus,
    /// This node's per-turn **source number**, when it produced data the answer
    /// could be citing.
    ///
    /// The prose says "revenue was $4.2M"; the timeline knows which call returned
    /// that number; nothing connected the two. This is the join key: the tool
    /// result is labelled `[source N]` for the model, the node carries the same
    /// `N`, and a `[N]` in the answer resolves back here.
    ///
    /// `None` for calls that return no data (`open_query`, `save_query`,
    /// `generate_report`), for writes, and for subagent nodes -- only a subagent's
    /// final report crosses back to the parent, so the parent cannot cite the
    /// child's individual queries and must not pretend to.
    #[cfg_attr(feature = "serde", serde(default))]
    pub source_ordinal: Option<u32>,
    /// A one-line result summary once known: a row count, a byte size, or the
    /// error message on failure. `None` until the node completes.
    #[cfg_attr(feature = "serde", serde(default))]
    pub detail: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub children: Vec<ActivityNode>,
}

impl ActivityNode {
    /// A fresh `Running` node with no result and no children.
    pub fn running(id: impl Into<ActivityId>, kind: ActivityKind) -> Self {
        ActivityNode {
            id: id.into(),
            kind,
            status: ActivityStatus::Running,
            source_ordinal: None,
            detail: None,
            children: Vec::new(),
        }
    }
}

/// Where one plan step stands, mirrored from the agent's own checklist (ACP
/// `PlanEntryStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// One entry in the agent's plan checklist, shown at the top of the turn so the
/// user sees the intended steps and watches them tick off.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanStep {
    pub title: String,
    pub status: PlanStepStatus,
}

#[cfg(test)]
mod ai_policy_tests {
    use super::*;
    use crate::{DbKind, ObjectKind};

    #[test]
    fn tier_ladder_gates_tools() {
        // off → nothing; schema → structure only; read → the full catalog.
        assert!(!AiTier::Off.allows_tool("list_schema"));
        assert!(AiTier::Schema.allows_tool("list_schema"));
        assert!(AiTier::Schema.allows_tool("describe_table"));
        assert!(!AiTier::Schema.allows_tool("run_select"));
        assert!(!AiTier::Schema.allows_tool("explain"));
        assert!(AiTier::Read.allows_tool("run_select"));
        assert!(AiTier::Read.allows_tool("explain"));
        assert!(AiTier::Read.allows_tool("generate_report"));
        assert!(AiTier::Read.allows_tool("open_query"));
        // The write tool is gated to the Write tier, and Write inherits the read
        // catalog on top of it.
        assert!(!AiTier::Read.allows_tool("propose_write"));
        assert!(AiTier::Write.allows_tool("propose_write"));
        assert!(AiTier::Write.allows_tool("run_select"));
        // Unknown tools are never allowed at any tier.
        assert!(!AiTier::Read.allows_tool("frobnicate"));
        assert!(!AiTier::Write.allows_tool("frobnicate"));
    }

    #[test]
    fn tier_parse_recognizes_known_tiers_and_fails_closed() {
        assert_eq!(AiTier::parse("off"), AiTier::Off);
        assert_eq!(AiTier::parse(" Schema "), AiTier::Schema);
        assert_eq!(AiTier::parse("READ"), AiTier::Read);
        assert_eq!(AiTier::parse("write"), AiTier::Write);
        // A typo or empty string fails CLOSED (Off), never to a permissive tier:
        // locking the assistant down must not silently grant read access.
        assert_eq!(AiTier::parse("nonsense"), AiTier::Off);
        assert_eq!(AiTier::parse("readonly"), AiTier::Off);
        assert_eq!(AiTier::parse(""), AiTier::Off);
    }

    #[test]
    fn overrides_layer_over_global() {
        let global = AiPolicy {
            enabled: true,
            tier: AiTier::Read,
            limits: AiLimits::default(),
            read_only: false,
            preview_writes: true,
            sandbox_timeout_secs: 120,
        };
        // Unset overrides inherit the global policy verbatim.
        assert_eq!(global.with_overrides(None, None), global);
        // A connection can tighten the tier and flip the master switch without
        // touching the global policy; limits stay global.
        let tightened = global.with_overrides(Some(false), Some(AiTier::Schema));
        assert!(!tightened.enabled);
        assert_eq!(tightened.tier, AiTier::Schema);
        assert_eq!(tightened.limits, global.limits);
    }

    /// The relation tier is exactly the kinds with columns. This is the predicate
    /// the browse/completion/ER paths widen to, so a kind sliding into it is a
    /// behaviour change (a `SELECT * FROM <trigger>`), not a cosmetic one.
    #[test]
    fn only_column_bearing_kinds_are_relations() {
        for kind in [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
        ] {
            assert!(kind.is_relation(), "{kind:?} has columns");
            assert!(!kind.is_lazy(), "{kind:?} rides the connect-time skeleton");
        }
        for kind in [
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Trigger,
            ObjectKind::Sequence,
            ObjectKind::Type,
        ] {
            assert!(!kind.is_relation(), "{kind:?} has no columns");
            assert!(kind.is_lazy(), "{kind:?} loads on expand");
        }
    }

    /// Every engine lists its relations before its lazy kinds and repeats none.
    /// The tree draws groups in this order and keys them by kind, so a duplicate
    /// would render two identical group rows fighting over one expansion state.
    #[test]
    fn object_kinds_are_ordered_relations_first_and_unique() {
        for kind in [
            DbKind::Postgres,
            DbKind::Mysql,
            DbKind::Sqlite,
            DbKind::Clickhouse,
            DbKind::Redis,
            DbKind::Mongo,
        ] {
            let kinds = kind.object_kinds();
            let mut seen = Vec::new();
            let mut lazy_started = false;
            for &k in kinds {
                assert!(!seen.contains(&k), "{kind:?} lists {k:?} twice");
                seen.push(k);
                if k.is_lazy() {
                    lazy_started = true;
                } else {
                    assert!(
                        !lazy_started,
                        "{kind:?} puts the relation {k:?} after a lazy kind"
                    );
                }
            }
            // A SQL engine always has tables; the two non-SQL seams have nothing
            // in this model at all and must stay empty so no group is drawn.
            if matches!(kind, DbKind::Redis | DbKind::Mongo) {
                assert!(kinds.is_empty(), "{kind:?} populates no schema tree");
            } else {
                assert!(kinds.contains(&ObjectKind::Table), "{kind:?} has tables");
            }
        }
    }

    /// The wire token is unique per kind: it names a group across the
    /// `Command`/`Event` boundary, so a collision would route a reply to the
    /// wrong group node.
    #[test]
    fn object_kind_wire_tokens_are_distinct() {
        let all = [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Trigger,
            ObjectKind::Sequence,
            ObjectKind::Type,
        ];
        let mut tokens: Vec<&str> = all.iter().map(|k| k.as_str()).collect();
        tokens.sort_unstable();
        let before = tokens.len();
        tokens.dedup();
        assert_eq!(tokens.len(), before, "wire tokens collide");
    }
}
