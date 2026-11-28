//! Regents threads and agent definitions: `thread.list`, `thread.open`,
//! `thread.accept`, `thread.fork` and
//! `agent.definition.list` (ADR-0004). `agent.*` without `definition` is
//! herdr's family for agent processes in panes.

use serde::{Deserialize, Serialize};

use crate::cli::projection::{cli_params, Field};

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct ThreadListParams => ThreadListProjection {
        /// Workspace of the threads: its public ID or the stable ID from
        /// `workspace.toml`. Absent means the active workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None)
            .input(super::tabs::workspace_option("WORKSPACE_ID").order(0)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct ThreadOpenParams => ThreadOpenProjection {
        /// Workspace of the thread: its public ID or the stable ID from
        /// `workspace.toml`. Absent means the active workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None)
            .input(super::tabs::workspace_option("WORKSPACE_ID").order(0)),
        /// Thread ID, for example `t1`.
        pub thread_id: String => Field::new(String::new())
            .input(crate::cli::thread_bindings::thread_id()),
    }
}

cli_params! {
    /// `thread.accept` and `thread.fork`: an active thread decides about the
    /// mention that waits for it (#17). The CLI reads the thread and its
    /// credential from the environment of the thread.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct ThreadDecisionParams => ThreadDecisionProjection {
        /// Workspace of the thread: its public ID or the stable ID from
        /// `workspace.toml`. Absent means the active workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None)
            .input(super::tabs::workspace_option("WORKSPACE_ID").order(0)),
        /// The thread that decides, from `REGENTS_THREAD_ID`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub thread_id: Option<String> => Field::new(Option::<String>::None),
        /// Credential of `thread_id`, from `REGENTS_THREAD_TOKEN`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub thread_token: Option<String> => Field::new(Option::<String>::None),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct AgentDefinitionListParams => AgentDefinitionListProjection {
        /// Workspace of the agents: its public ID or the stable ID from
        /// `workspace.toml`. Absent means the active workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None)
            .input(super::tabs::workspace_option("WORKSPACE_ID").order(0)),
    }
}

/// The life cycle state of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Queued,
    /// The agent CLI runs.
    Active,
    /// The agent finished; its CLI process ended and the session stays.
    Done,
    /// The server restarted while the thread was active.
    Interrupted,
    /// The CLI ended before it did any work, or did not start.
    Failed,
    /// A state that this client does not know.
    #[serde(other)]
    Unknown,
}

/// What opened a thread.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ThreadOriginKind {
    /// A mention in a channel.
    #[default]
    Channel,
    /// A direct conversation.
    Direct,
    /// An origin that this client does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadInfo {
    /// Unique in the workspace, for example `t1`.
    pub id: String,
    /// Name of the agent that the thread runs.
    pub agent: String,
    pub cli: AgentCliKind,
    /// What opened the thread.
    #[serde(default)]
    pub origin: ThreadOriginKind,
    /// Channel of the mention that opened the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Message of the mention that opened the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_message_id: Option<u64>,
    /// Thread whose agent mentioned this agent. Absent when a human did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_thread_id: Option<String>,
    /// Length of the delegation chain before this thread: 0 when a human
    /// opened it.
    #[serde(default)]
    pub depth: u32,
    /// Where the thread runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<AgentIsolation>,
    pub state: ThreadState,
    /// Detected state of the agent CLI while the thread is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<super::AgentStatus>,
    /// Why the thread ended, when it did not end done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// Session of the agent CLI, to resume the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Turn of the CLI session, from 0: each threaded reply that resumes
    /// the thread adds one.
    #[serde(default)]
    pub turn: u32,
    /// Place of a queued thread in the thread queue of its workspace, from
    /// 1. Absent for the other states and from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    /// Pane that runs the CLI while the thread is active. The pane is in
    /// no tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// RFC 3339 times in UTC, in whole seconds.
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentCliKind {
    Claude,
    Codex,
    Agy,
    /// A CLI that this client does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentIsolation {
    /// Threads run in the workspace folder.
    Shared,
    /// Each thread runs in its own git worktree.
    Worktree,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentApproval {
    /// The CLI asks for permission as it does by default.
    Default,
    /// The CLI runs without asking.
    Auto,
    #[serde(other)]
    Unknown,
}

/// An agent of the workspace: a definition, not a process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentDefinitionInfo {
    /// The mention target: `@name`.
    pub name: String,
    pub cli: AgentCliKind,
    /// Absent means the default model of the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub isolation: AgentIsolation,
    pub approval: AgentApproval,
    /// The Chief of Staff, which cannot be removed.
    pub reserved: bool,
}
