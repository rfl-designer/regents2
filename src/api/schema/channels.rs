//! Regents channels: `channel.post`, `channel.read` and the
//! `channel.message_posted` event (ADR-0004).

use serde::{Deserialize, Serialize};

use crate::cli::channel_bindings as bindings;
use crate::cli::projection::{cli_params, Field};

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct ChannelPostParams => ChannelPostProjection {
        /// Workspace of the channel: its public ID or the stable ID from
        /// `workspace.toml`. Absent means the active workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None)
            .input(super::tabs::workspace_option("WORKSPACE_ID").order(0)),
        /// Channel name (slug), for example `geral`.
        pub channel: String => Field::new(String::new()).input(bindings::channel_name()),
        pub body: String => Field::new(String::new()).input(bindings::message_body()),
        /// Message of the same channel that this message replies to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reply_to: Option<u64> => Field::new(None)
            .input(bindings::reply_to().order(1)),
        /// Thread that posts the message. `regents channel post` sends it
        /// when a thread CLI runs it. A message from a thread opens no new
        /// thread (delegation comes later).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub origin_thread_id: Option<String> => Field::new(Option::<String>::None),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct ChannelReadParams => ChannelReadProjection {
        /// Workspace of the channel: its public ID or the stable ID from
        /// `workspace.toml`. Absent means the active workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None)
            .input(super::tabs::workspace_option("WORKSPACE_ID").order(0)),
        /// Channel name (slug), for example `geral`.
        pub channel: String => Field::new(String::new()).input(bindings::channel_name()),
        /// Only messages older than this message ID.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub before: Option<u64> => Field::new(None)
            .input(bindings::before().order(2)),
        /// At most this many messages, the most recent ones. Absent means all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<u32> => Field::new(None).input(bindings::limit().order(1)),
    }
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelAuthorKind {
    Human,
    Agent,
    System,
    /// A kind that this client does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChannelAuthorInfo {
    pub kind: ChannelAuthorKind,
    /// The user name for a human, the agent name for an agent.
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChannelMessageInfo {
    /// Increases with each message of the channel.
    pub id: u64,
    pub channel: String,
    pub author: ChannelAuthorInfo,
    pub body: String,
    /// Message of the same channel that this message replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
    /// Thread that produced the message (agent messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_thread_id: Option<String>,
    /// Names of the agents that the message mentions. Absent means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// RFC 3339 time in UTC, in whole seconds.
    pub created_at: String,
}
