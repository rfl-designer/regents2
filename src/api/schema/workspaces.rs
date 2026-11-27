use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::AgentStatus;
use crate::cli::projection::{cli_params, Field};
use crate::cli::workspace_bindings as bindings;

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceCreateParams => WorkspaceCreateProjection {
        /// Workspace whose focused pane supplies the `follow` cwd policy.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source_workspace_id: Option<String> => Field::new(None::<String>),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String> => Field::new(None)
            .input(bindings::optional_text("cwd", "PATH").path().order(0)),
        #[serde(default)]
        pub focus: bool => Field::new(false)
            .input(bindings::boolean("focus", true).order(3))
            .input(bindings::boolean("no-focus", false).order(4)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub label: Option<String> => Field::new(None)
            .input(bindings::optional_text("label", "TEXT").order(1)),
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        pub env: HashMap<String, String> => Field::new(HashMap::new())
            .input(bindings::environment().order(2)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceCloseParams => WorkspaceCloseProjection {
        pub workspace_id: String => Field::new(String::new()).input(bindings::workspace_id()),
        #[serde(default, skip_serializing_if = "super::is_false")]
        pub close_group: bool => Field::new(false)
            .input(bindings::boolean("group", true).hidden()),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceRenameParams => WorkspaceRenameProjection {
        pub workspace_id: String => Field::new(String::new())
            .input(bindings::workspace_id().value_name("WORKSPACE_ID")),
        pub label: String => Field::new(String::new()).input(bindings::label()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMoveParams {
    pub workspace_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceMoveBlockParams {
    pub workspace_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_workspace_id: Option<String>,
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceReportMetadataParams => WorkspaceMetadataProjection {
        pub workspace_id: String => Field::new(String::new())
            .input(bindings::workspace_id().value_name("WORKSPACE_ID")),
        pub source: String => Field::new(String::new()).input(bindings::source())
            .validate(|source| bindings::validate_source(source)),
        #[schemars(schema_with = "super::common::metadata_token_patch_schema")]
        pub tokens: HashMap<String, Option<String>> => Field::new(HashMap::new())
            .input(bindings::token()).input(bindings::clear_token())
            .validate(bindings::validate_tokens),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub seq: Option<u64> => Field::new(None).input(bindings::seq()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(range(min = 1, max = 86_400_000))]
        pub ttl_ms: Option<u64> => Field::new(None).input(bindings::ttl_ms()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schemars(schema_with = "super::common::metadata_token_values_schema")]
    pub tokens: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorkspaceWorktreeInfo>,
    /// Workspace folder that gives this workspace its identity (Regents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<WorkspaceFolderInfo>,
    /// Regents legacy workspace: it has no folder, so it has no agents,
    /// channels or wiki until `workspace.link` binds one. Absent means false.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub legacy: bool,
    /// Regents disconnected workspace: its folder is no longer at the known
    /// path. `workspace.relink` binds it to the new path. Absent means false.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub disconnected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceFolderInfo {
    /// Stable ID from the folder's `.regents/workspace.toml`.
    pub id: String,
    pub path: String,
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceAddParams => WorkspaceAddProjection {
        /// Absolute path of the folder on the server host.
        pub path: String => Field::new(String::new()).input(bindings::folder_path()),
        #[serde(default)]
        pub focus: bool => Field::new(false)
            .input(bindings::boolean("focus", true).order(1))
            .input(bindings::boolean("no-focus", false).order(2)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceLinkParams => WorkspaceLinkProjection {
        /// Legacy workspace to bind.
        pub workspace_id: String => Field::new(String::new())
            .input(bindings::workspace_id().value_name("WORKSPACE_ID")),
        /// Absolute path of the folder on the server host.
        pub path: String => Field::new(String::new()).input(bindings::folder_path_at(1)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceRelinkParams => WorkspaceRelinkProjection {
        /// Stable ID of the disconnected workspace, from `workspace.toml`.
        pub id: String => Field::new(String::new()).input(bindings::stable_workspace_id()),
        /// Absolute path of the new folder on the server host. Its
        /// `workspace.toml` must have the same ID.
        pub path: String => Field::new(String::new()).input(bindings::folder_path_at(1)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct WorkspaceRemoveParams => WorkspaceRemoveProjection {
        /// Stable ID of the registered workspace, from `workspace.toml`. The
        /// folder stays on disk.
        pub id: String => Field::new(String::new()).input(bindings::stable_workspace_id()),
    }
}

/// An open workspace without a folder (Regents legacy workspace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LegacyWorkspaceInfo {
    pub workspace_id: String,
    pub label: String,
}

/// A folder in the global workspace registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegisteredWorkspaceInfo {
    pub path: String,
    /// ID from `.regents/workspace.toml`; absent when the file cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Open workspace bound to this folder, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The folder is no longer at `path`; `id` is the last known ID.
    /// Absent means false.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub disconnected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceWorktreeInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
}
