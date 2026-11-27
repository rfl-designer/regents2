//! Folder workspaces (Regents): `workspace.add`, `workspace.link`,
//! `workspace.registry.list`, the reopen of the last used folder and the
//! guard for commands that need a folder. The domain rules live in
//! `crate::regents`; this module binds a folder to the herdr workspace that
//! holds its layout.

use std::path::{Path, PathBuf};

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, LegacyWorkspaceInfo, RegisteredWorkspaceInfo,
    ResponseResult, WorkspaceAddParams, WorkspaceLinkParams,
};
use crate::app::App;
use crate::regents::folder::{self, FolderBinding, OpenedFolder};
use crate::regents::registry::{self, AddedFolder, Registry};

use super::responses::{encode_error, encode_success};

/// Command that opens a folder workspace.
const WORKSPACE_ADD_COMMAND: &str = "regents workspace add <folder>";

/// Why a command that needs a workspace folder cannot run on a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
// `Legacy` is built only by `App::folder_workspace`; see the note there.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum FolderWorkspaceError {
    NotFound(String),
    /// The workspace has no folder (Regents legacy workspace).
    Legacy(String),
}

impl FolderWorkspaceError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "workspace_not_found",
            Self::Legacy(_) => "workspace_legacy",
        }
    }
}

impl std::fmt::Display for FolderWorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "workspace {id} not found"),
            Self::Legacy(id) => write!(
                f,
                "workspace {id} is a legacy workspace; link it to a folder with \
                 `regents workspace link {id} <folder>`"
            ),
        }
    }
}

impl App {
    /// True for the Regents product: every workspace needs a folder, and a
    /// workspace without one is a legacy workspace. Herdr has no folders.
    pub(crate) fn has_folder_workspaces(&self) -> bool {
        self.workspace_registry_path.is_some()
    }

    /// Command that clients show when no workspace is open, or `None` when
    /// the server can create a workspace without a folder.
    pub(crate) fn workspace_add_command(&self) -> Option<String> {
        self.has_folder_workspaces()
            .then(|| WORKSPACE_ADD_COMMAND.to_owned())
    }

    /// Resolves `workspace_id` to a workspace that has a folder. Commands that
    /// use the workspace folder (channels, agents, wiki) call this first, so a
    /// legacy workspace fails with one clear error.
    // The channel, agent and wiki commands (tickets #6 onward) are the callers;
    // until they exist only the tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn folder_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<(usize, &FolderBinding), FolderWorkspaceError> {
        let index = self.resolve_workspace(workspace_id)?;
        match &self.state.workspaces[index].folder {
            Some(binding) => Ok((index, binding)),
            None => Err(FolderWorkspaceError::Legacy(
                self.public_workspace_id(index),
            )),
        }
    }

    fn resolve_workspace(&self, workspace_id: &str) -> Result<usize, FolderWorkspaceError> {
        self.parse_workspace_id(workspace_id)
            .filter(|index| *index < self.state.workspaces.len())
            .ok_or_else(|| FolderWorkspaceError::NotFound(workspace_id.to_owned()))
    }

    /// A Regents workspace without a folder. Herdr workspaces are never legacy.
    pub(crate) fn is_legacy_workspace(&self, index: usize) -> bool {
        self.has_folder_workspaces()
            && self
                .state
                .workspaces
                .get(index)
                .is_some_and(|workspace| workspace.folder.is_none())
    }

    /// Error response for a request that would open a workspace without a
    /// folder. Regents refuses it; herdr gets `None` and keeps its behavior.
    pub(in crate::app) fn folderless_workspace_refusal(&self, id: &str) -> Option<String> {
        self.has_folder_workspaces().then(|| {
            encode_error(
                id.to_owned(),
                "workspace_folder_required",
                format!(
                    "regents workspaces need a folder; open one with `{WORKSPACE_ADD_COMMAND}`"
                ),
            )
        })
    }

    /// Records a user close of folder workspaces: each folder becomes the most
    /// recently used one. When the close leaves no workspace, the reopen skips
    /// these folders, so Regents does not reopen what the user just closed.
    pub(in crate::app) fn record_closed_folder_workspaces(&mut self, closed: Vec<FolderBinding>) {
        if closed.is_empty() {
            return;
        }
        if let Some(registry_path) = self.workspace_registry_path.clone() {
            for binding in &closed {
                if let Err(err) = registry::mark_used(&registry_path, &binding.root) {
                    tracing::warn!(err = %err, "cannot record the last used workspace");
                }
            }
        }
        if self.state.workspaces.is_empty() {
            self.reopen_excluded_folders = closed.into_iter().map(|binding| binding.id).collect();
        }
    }

    /// Opens the most recently used registered folder that still has its
    /// manifest, is not open and was not just closed. Returns `None` when no
    /// folder can open; then Regents has no workspace.
    pub(crate) fn reopen_last_used_folder_workspace(&mut self) -> Option<usize> {
        let registry_path = self.workspace_registry_path.clone()?;
        let registry = match Registry::load(&registry_path) {
            Ok(registry) => registry,
            Err(err) => {
                tracing::warn!(err = %err, "cannot read the workspace registry");
                return None;
            }
        };
        for entry in registry.by_recent_use() {
            let manifest = match folder::read_manifest(&entry.path) {
                Ok(manifest) => manifest,
                Err(err) => {
                    tracing::info!(
                        path = %entry.path.display(),
                        err = %err,
                        "registered workspace folder cannot reopen"
                    );
                    continue;
                }
            };
            if self.reopen_excluded_folders.contains(&manifest.id)
                || self.workspace_index_for_folder(&manifest.id).is_some()
            {
                continue;
            }
            match self.create_workspace_with_options(entry.path.clone(), true) {
                Ok(index) => {
                    let reopened = AddedFolder {
                        root: entry.path.clone(),
                        folder: OpenedFolder {
                            manifest,
                            initialized: false,
                        },
                        already_registered: true,
                    };
                    self.bind_folder(index, &reopened);
                    if let Err(err) = registry::mark_used(&registry_path, &entry.path) {
                        tracing::warn!(err = %err, "cannot record the last used workspace");
                    }
                    self.emit_workspace_open_events(index);
                    return Some(index);
                }
                Err(err) => {
                    tracing::warn!(
                        path = %entry.path.display(),
                        err = %err,
                        "failed to reopen workspace folder"
                    );
                }
            }
        }
        None
    }

    pub(super) fn handle_workspace_add(
        &mut self,
        id: String,
        params: WorkspaceAddParams,
    ) -> String {
        let Some(registry_path) = self.workspace_registry_path.clone() else {
            return folder_workspaces_unsupported(id);
        };
        let path = match absolute_folder_path(&params.path) {
            Ok(path) => path,
            Err(message) => return encode_error(id, "invalid_params", message),
        };
        let added = match registry::add_folder(&registry_path, &path) {
            Ok(added) => added,
            Err(err) => return encode_error(id, err.code(), err.to_string()),
        };
        let index = match self.workspace_index_for_folder(&added.folder.manifest.id) {
            Some(index) => {
                if params.focus {
                    self.state.switch_workspace(index);
                }
                index
            }
            None => match self.create_workspace_with_options(added.root.clone(), params.focus) {
                Ok(index) => {
                    self.bind_folder(index, &added);
                    self.emit_workspace_open_events(index);
                    index
                }
                Err(err) => return encode_error(id, "workspace_create_failed", err.to_string()),
            },
        };
        encode_success(
            id,
            ResponseResult::WorkspaceAdded {
                workspace: self.workspace_info(index),
                initialized: added.folder.initialized,
                already_registered: added.already_registered,
            },
        )
    }

    /// Binds a legacy workspace to a folder: the folder receives the initial
    /// structure like `workspace.add`, and the panes and layout stay.
    pub(super) fn handle_workspace_link(
        &mut self,
        id: String,
        params: WorkspaceLinkParams,
    ) -> String {
        let Some(registry_path) = self.workspace_registry_path.clone() else {
            return folder_workspaces_unsupported(id);
        };
        let index = match self.resolve_workspace(&params.workspace_id) {
            Ok(index) => index,
            Err(err) => return encode_error(id, err.code(), err.to_string()),
        };
        let workspace_id = self.public_workspace_id(index);
        if let Some(binding) = &self.state.workspaces[index].folder {
            return encode_error(
                id,
                "workspace_not_legacy",
                format!(
                    "workspace {workspace_id} already has the folder {}",
                    binding.root.display()
                ),
            );
        }
        let path = match absolute_folder_path(&params.path) {
            Ok(path) => path,
            Err(message) => return encode_error(id, "invalid_params", message),
        };
        // Refuse before anything is written: one folder binds one workspace.
        if let Some(open) = folder::read_manifest(&path)
            .ok()
            .and_then(|manifest| self.workspace_index_for_folder(&manifest.id))
        {
            return encode_error(
                id,
                "workspace_folder_in_use",
                format!(
                    "{} is already open as workspace {}",
                    path.display(),
                    self.public_workspace_id(open)
                ),
            );
        }
        let added = match registry::add_folder(&registry_path, &path) {
            Ok(added) => added,
            Err(err) => return encode_error(id, err.code(), err.to_string()),
        };
        self.bind_folder(index, &added);
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceRenamed,
            data: EventData::WorkspaceRenamed {
                workspace_id,
                label: added.folder.manifest.name.clone(),
            },
        });
        encode_success(
            id,
            ResponseResult::WorkspaceLinked {
                workspace: self.workspace_info(index),
                initialized: added.folder.initialized,
                already_registered: added.already_registered,
            },
        )
    }

    pub(super) fn handle_workspace_registry_list(&mut self, id: String) -> String {
        let Some(registry_path) = self.workspace_registry_path.clone() else {
            return folder_workspaces_unsupported(id);
        };
        let registry = match Registry::load(&registry_path) {
            Ok(registry) => registry,
            Err(err) => return encode_error(id, "workspace_registry_unreadable", err.to_string()),
        };
        let workspaces = registry
            .entries()
            .iter()
            .map(|entry| {
                let manifest = folder::read_manifest(&entry.path).ok();
                let workspace_id = manifest
                    .as_ref()
                    .and_then(|manifest| self.workspace_index_for_folder(&manifest.id))
                    .map(|index| self.public_workspace_id(index));
                RegisteredWorkspaceInfo {
                    path: entry.path.display().to_string(),
                    id: manifest.as_ref().map(|manifest| manifest.id.clone()),
                    name: manifest.map(|manifest| manifest.name),
                    workspace_id,
                }
            })
            .collect();
        let legacy = (0..self.state.workspaces.len())
            .filter(|index| self.is_legacy_workspace(*index))
            .map(|index| {
                let info = self.workspace_info(index);
                LegacyWorkspaceInfo {
                    workspace_id: info.workspace_id,
                    label: info.label,
                }
            })
            .collect();
        encode_success(
            id,
            ResponseResult::RegisteredWorkspaceList { workspaces, legacy },
        )
    }

    /// Gives the workspace at `index` the identity and name of `added`.
    fn bind_folder(&mut self, index: usize, added: &AddedFolder) {
        let manifest = &added.folder.manifest;
        let workspace = &mut self.state.workspaces[index];
        workspace.folder = Some(FolderBinding {
            id: manifest.id.clone(),
            root: added.root.clone(),
        });
        workspace.set_custom_name(manifest.name.clone());
        // A workspace is open again: a later close starts a new exclusion.
        self.reopen_excluded_folders.clear();
        self.schedule_session_save();
    }

    fn workspace_index_for_folder(&self, folder_id: &str) -> Option<usize> {
        self.state.workspaces.iter().position(|workspace| {
            workspace
                .folder
                .as_ref()
                .is_some_and(|folder| folder.id == folder_id)
        })
    }
}

fn absolute_folder_path(path: &str) -> Result<PathBuf, String> {
    let expanded = crate::worktree::expand_tilde_path(path);
    if Path::new(&expanded).is_absolute() {
        Ok(expanded)
    } else {
        Err(format!("workspace path must be absolute: {path}"))
    }
}

pub(in crate::app) fn folder_info(
    binding: &FolderBinding,
) -> crate::api::schema::WorkspaceFolderInfo {
    crate::api::schema::WorkspaceFolderInfo {
        id: binding.id.clone(),
        path: binding.root.display().to_string(),
    }
}

fn folder_workspaces_unsupported(id: String) -> String {
    encode_error(
        id,
        "unsupported",
        "folder workspaces are available only in regents",
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::api::schema::{ErrorResponse, SuccessResponse, WorkspaceInfo};
    use std::path::PathBuf;

    struct Fixture {
        app: App,
        base: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir()
                .join(format!("regents-app-{name}-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&base).unwrap();
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                crate::app::AppPolicy::TEST,
                None,
                api_rx,
                crate::api::EventHub::default(),
            );
            app.state.default_shell = exiting_test_command().into();
            app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
            app.workspace_registry_path = Some(base.join("config").join("workspaces.toml"));
            Self { app, base }
        }

        fn folder(&self, name: &str) -> PathBuf {
            let path = self.base.join(name);
            std::fs::create_dir_all(&path).unwrap();
            crate::platform::canonical_path(&path).unwrap()
        }

        fn add(&mut self, path: &std::path::Path) -> (WorkspaceInfo, bool, bool) {
            let response = self.app.handle_workspace_add(
                "add".into(),
                WorkspaceAddParams {
                    path: path.display().to_string(),
                    focus: true,
                },
            );
            match serde_json::from_str::<SuccessResponse>(&response)
                .unwrap_or_else(|_| panic!("workspace.add failed: {response}"))
                .result
            {
                ResponseResult::WorkspaceAdded {
                    workspace,
                    initialized,
                    already_registered,
                } => (workspace, initialized, already_registered),
                other => panic!("unexpected result {other:?}"),
            }
        }

        /// Sends one API request as JSON and returns the JSON response.
        fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            let request: crate::api::schema::Request = serde_json::from_value(
                serde_json::json!({"id": "test", "method": method, "params": params}),
            )
            .unwrap();
            serde_json::from_str(&self.app.handle_api_request(request)).unwrap()
        }

        /// Opens a workspace without a folder, as an old session restores it.
        fn legacy_workspace(&mut self) -> String {
            let cwd = self.folder("legacy-cwd");
            let index = self.app.create_workspace_with_options(cwd, false).unwrap();
            self.app.public_workspace_id(index)
        }

        /// Removes every workspace without a user close, as when the last
        /// pane of each one exits.
        fn close_all(&mut self) {
            let all = (0..self.app.state.workspaces.len()).collect();
            self.app.state.close_workspaces(all);
            assert!(self.app.state.workspaces.is_empty());
        }

        /// Closes a workspace as the user does, through `workspace.close`.
        fn close(&mut self, workspace_id: &str) {
            let response = self.request(
                "workspace.close",
                serde_json::json!({"workspace_id": workspace_id}),
            );
            assert!(response.get("error").is_none(), "{response}");
        }

        fn open_folder_ids(&self) -> Vec<Option<String>> {
            self.app
                .state
                .workspaces
                .iter()
                .map(|workspace| workspace.folder.as_ref().map(|folder| folder.id.clone()))
                .collect()
        }

        fn registered(&mut self) -> Vec<RegisteredWorkspaceInfo> {
            let response = self.app.handle_workspace_registry_list("list".into());
            match serde_json::from_str::<SuccessResponse>(&response)
                .unwrap()
                .result
            {
                ResponseResult::RegisteredWorkspaceList { workspaces, .. } => workspaces,
                other => panic!("unexpected result {other:?}"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown_test_runtimes(&mut self.app);
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[tokio::test]
    async fn adding_an_empty_folder_opens_a_workspace_named_after_it() {
        let mut fixture = Fixture::new("empty");
        let folder = fixture.folder("projeto");

        let (workspace, initialized, already_registered) = fixture.add(&folder);

        assert!(initialized);
        assert!(!already_registered);
        assert_eq!(workspace.label, "projeto");
        let manifest = folder::read_manifest(&folder).unwrap();
        assert_eq!(
            workspace.folder,
            Some(crate::api::schema::WorkspaceFolderInfo {
                id: manifest.id.clone(),
                path: folder.display().to_string(),
            })
        );
        assert_eq!(
            fixture.registered(),
            vec![RegisteredWorkspaceInfo {
                path: folder.display().to_string(),
                id: Some(manifest.id),
                name: Some("projeto".into()),
                workspace_id: Some(workspace.workspace_id),
            }]
        );
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn adding_the_same_folder_twice_reuses_its_workspace() {
        let mut fixture = Fixture::new("twice");
        let folder = fixture.folder("projeto");
        let (first, _, _) = fixture.add(&folder);

        let (second, initialized, already_registered) = fixture.add(&folder);

        assert!(!initialized);
        assert!(already_registered);
        assert_eq!(second.workspace_id, first.workspace_id);
        assert_eq!(fixture.app.state.workspaces.len(), 1);
        assert_eq!(fixture.registered().len(), 1);
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn the_root_pane_cwd_does_not_change_the_workspace_identity() {
        let mut fixture = Fixture::new("cwd");
        let folder = fixture.folder("projeto");
        let elsewhere = fixture.folder("elsewhere");
        let (before, _, _) = fixture.add(&folder);
        shutdown_test_runtimes(&mut fixture.app);
        let workspace = &fixture.app.state.workspaces[0];
        let root = workspace.tabs[0].root_pane;
        let terminal_id = workspace.tabs[0].terminal_id(root).unwrap().clone();
        fixture
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .cwd = elsewhere;

        let (after, _, already_registered) = fixture.add(&folder);

        assert!(already_registered);
        assert_eq!(after.workspace_id, before.workspace_id);
        assert_eq!(after.folder, before.folder);
        assert_eq!(after.label, "projeto");
        assert_eq!(fixture.app.state.workspaces.len(), 1);
    }

    #[tokio::test]
    async fn two_folders_get_two_workspaces() {
        let mut fixture = Fixture::new("two");
        let first = fixture.folder("first");
        let second = fixture.folder("second");

        let (first, _, _) = fixture.add(&first);
        let (second, _, _) = fixture.add(&second);

        assert_ne!(first.workspace_id, second.workspace_id);
        assert_ne!(first.folder, second.folder);
        assert_eq!(fixture.registered().len(), 2);
        fixture.app.state.assert_invariants_for_test();
    }

    fn error_code(response: &serde_json::Value) -> &str {
        response["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("expected an error: {response}"))
    }

    #[tokio::test]
    async fn creating_a_workspace_without_a_folder_is_refused() {
        let mut fixture = Fixture::new("create");
        let cwd = fixture.folder("somewhere");

        let response = fixture.request(
            "workspace.create",
            serde_json::json!({"cwd": cwd, "focus": true}),
        );

        assert_eq!(error_code(&response), "workspace_folder_required");
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("workspace add"), "{message}");
        assert!(fixture.app.state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn herdr_still_creates_workspaces_without_a_folder() {
        let mut fixture = Fixture::new("herdr-create");
        fixture.app.workspace_registry_path = None;

        let response = fixture.request("workspace.create", serde_json::json!({"focus": true}));

        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(fixture.app.state.workspaces.len(), 1);
        assert_eq!(response["result"]["workspace"].get("legacy"), None);
    }

    #[tokio::test]
    async fn moving_a_pane_to_a_new_workspace_is_refused() {
        let mut fixture = Fixture::new("move");
        let folder = fixture.folder("projeto");
        let (workspace, _, _) = fixture.add(&folder);
        let root_pane = fixture.app.state.workspaces[0].tabs[0].root_pane;
        let pane_id = fixture.app.public_pane_id(0, root_pane).unwrap();

        let response = fixture.request(
            "pane.move",
            serde_json::json!({
                "pane_id": pane_id,
                "destination": {"type": "new_workspace"},
            }),
        );

        assert_eq!(error_code(&response), "workspace_folder_required");
        assert_eq!(fixture.app.state.workspaces.len(), 1);
        assert_eq!(fixture.app.public_workspace_id(0), workspace.workspace_id);
    }

    #[tokio::test]
    async fn creating_a_worktree_workspace_is_refused() {
        let mut fixture = Fixture::new("worktree");
        let request: crate::api::schema::Request = serde_json::from_value(
            serde_json::json!({"id": "wt", "method": "worktree.create", "params": {"path": "/tmp/x"}}),
        )
        .unwrap();
        let (respond_to, responses) = std::sync::mpsc::channel();

        assert!(fixture
            .app
            .handle_deferred_worktree_api_request(request, respond_to, true));

        let response: serde_json::Value =
            serde_json::from_str(&responses.try_recv().unwrap()).unwrap();
        assert_eq!(error_code(&response), "workspace_folder_required");
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("workspace add"));
        assert!(fixture.app.state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn closing_the_last_workspace_reopens_another_folder() {
        let mut fixture = Fixture::new("close-last");
        let first = fixture.folder("first");
        let second = fixture.folder("second");
        let (first_ws, _, _) = fixture.add(&first);
        let (second_ws, _, _) = fixture.add(&second);
        fixture.close(&first_ws.workspace_id);
        fixture.close(&second_ws.workspace_id);

        assert!(fixture.app.ensure_default_workspace());

        assert_eq!(
            fixture.open_folder_ids(),
            vec![Some(first_ws.folder.unwrap().id)]
        );
    }

    #[tokio::test]
    async fn closing_the_only_folder_workspace_keeps_zero_workspaces() {
        let mut fixture = Fixture::new("close-only");
        let only = fixture.folder("only");
        let (workspace, _, _) = fixture.add(&only);
        fixture.close(&workspace.workspace_id);

        assert!(!fixture.app.ensure_default_workspace());
        assert!(!fixture.app.ensure_default_workspace_for_new_client());

        assert!(fixture.app.state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn a_closed_workspace_counts_as_the_last_used_one() {
        let mut fixture = Fixture::new("close-order");
        let mut ids = Vec::new();
        for name in ["a", "b", "c"] {
            let folder = fixture.folder(name);
            let (workspace, _, _) = fixture.add(&folder);
            ids.push((workspace.workspace_id, workspace.folder.unwrap().id));
        }
        // Close in the reverse order of use: c, then b, then a (the last).
        for (workspace_id, _) in ids.iter().rev() {
            fixture.close(workspace_id);
        }

        assert!(fixture.app.ensure_default_workspace());

        assert_eq!(fixture.open_folder_ids(), vec![Some(ids[1].1.clone())]);
    }

    #[tokio::test]
    async fn the_snapshot_tells_clients_how_to_add_a_workspace() {
        let mut fixture = Fixture::new("hint");
        assert_eq!(
            fixture.app.workspace_add_command().as_deref(),
            Some("regents workspace add <folder>")
        );

        fixture.app.workspace_registry_path = None;

        assert_eq!(fixture.app.workspace_add_command(), None);
    }

    #[tokio::test]
    async fn with_no_workspace_open_the_last_used_folder_reopens() {
        let mut fixture = Fixture::new("reopen");
        let first = fixture.folder("first");
        let second = fixture.folder("second");
        fixture.add(&second);
        fixture.add(&first);
        let first_id = folder::read_manifest(&first).unwrap().id;
        fixture.close_all();

        assert!(fixture.app.ensure_default_workspace());

        assert_eq!(fixture.open_folder_ids(), vec![Some(first_id)]);
        assert_eq!(fixture.app.workspace_info(0).label, "first");
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn a_reopened_folder_becomes_the_last_used_one() {
        let mut fixture = Fixture::new("reopen-order");
        let first = fixture.folder("first");
        let second = fixture.folder("second");
        fixture.add(&first);
        let second_id = fixture.add(&second).0.folder.unwrap().id;
        fixture.close_all();
        let parked = fixture.base.join("parked");
        std::fs::rename(&second, &parked).unwrap();
        assert!(fixture.app.ensure_default_workspace());
        std::fs::rename(&parked, &second).unwrap();
        fixture.close_all();

        assert!(fixture.app.ensure_default_workspace());

        let first_id = folder::read_manifest(&first).unwrap().id;
        assert_ne!(first_id, second_id);
        assert_eq!(fixture.open_folder_ids(), vec![Some(first_id)]);
    }

    #[tokio::test]
    async fn a_missing_folder_is_skipped_when_reopening() {
        let mut fixture = Fixture::new("reopen-missing");
        let first = fixture.folder("first");
        let second = fixture.folder("second");
        fixture.add(&first);
        fixture.add(&second);
        let first_id = folder::read_manifest(&first).unwrap().id;
        fixture.close_all();
        std::fs::remove_dir_all(&second).unwrap();

        assert!(fixture.app.ensure_default_workspace());

        assert_eq!(fixture.open_folder_ids(), vec![Some(first_id)]);
    }

    #[tokio::test]
    async fn with_no_usable_folder_regents_keeps_zero_workspaces() {
        let mut fixture = Fixture::new("reopen-none");
        let gone = fixture.folder("gone");
        fixture.add(&gone);
        fixture.close_all();
        std::fs::remove_dir_all(&gone).unwrap();

        assert!(!fixture.app.ensure_default_workspace());
        assert!(!fixture.app.ensure_default_workspace());

        assert!(fixture.app.state.workspaces.is_empty());
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn an_empty_registry_keeps_zero_workspaces() {
        let mut fixture = Fixture::new("reopen-empty");

        assert!(!fixture.app.ensure_default_workspace());

        assert!(fixture.app.state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn a_new_client_retries_the_reopen() {
        let mut fixture = Fixture::new("reopen-retry");
        assert!(!fixture.app.ensure_default_workspace());
        let folder = fixture.folder("later");
        let registry_path = fixture.app.workspace_registry_path.clone().unwrap();
        registry::add_folder(&registry_path, &folder).unwrap();
        assert!(
            !fixture.app.ensure_default_workspace(),
            "the event loop must not read the registry again"
        );

        assert!(fixture.app.ensure_default_workspace_for_new_client());

        assert_eq!(fixture.app.state.workspaces.len(), 1);
    }

    #[tokio::test]
    async fn herdr_still_creates_a_default_workspace() {
        let mut fixture = Fixture::new("herdr-default");
        fixture.app.workspace_registry_path = None;

        assert!(fixture.app.ensure_default_workspace());

        assert_eq!(fixture.open_folder_ids(), vec![None]);
    }

    #[tokio::test]
    async fn a_workspace_without_a_folder_is_marked_legacy() {
        let mut fixture = Fixture::new("legacy");
        let legacy_id = fixture.legacy_workspace();
        let folder = fixture.folder("projeto");
        let (bound, _, _) = fixture.add(&folder);

        let listed = fixture.request("workspace.list", serde_json::json!({}));

        let workspaces = listed["result"]["workspaces"].as_array().unwrap();
        assert_eq!(workspaces[0]["workspace_id"], legacy_id);
        assert_eq!(workspaces[0]["legacy"], true);
        assert_eq!(workspaces[1]["workspace_id"], bound.workspace_id);
        assert_eq!(workspaces[1].get("legacy"), None);
        let registry = fixture.request("workspace.registry.list", serde_json::json!({}));
        let legacy = &registry["result"]["legacy"];
        assert_eq!(legacy.as_array().unwrap().len(), 1, "{registry}");
        assert_eq!(legacy[0]["workspace_id"], legacy_id);
        assert_eq!(
            registry["result"]["workspaces"].as_array().unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn herdr_workspaces_are_not_legacy() {
        let mut fixture = Fixture::new("herdr-legacy");
        fixture.app.workspace_registry_path = None;
        fixture.legacy_workspace();

        assert!(!fixture.app.workspace_info(0).legacy);
    }

    #[tokio::test]
    async fn link_binds_a_legacy_workspace_to_a_folder() {
        let mut fixture = Fixture::new("link");
        let legacy_id = fixture.legacy_workspace();
        let panes_before = fixture.app.workspace_info(0).pane_count;
        let folder = fixture.folder("projeto");

        let response = fixture.request(
            "workspace.link",
            serde_json::json!({"workspace_id": legacy_id, "path": folder}),
        );

        let result = &response["result"];
        assert_eq!(result["type"], "workspace_linked", "{response}");
        assert_eq!(result["initialized"], true);
        assert_eq!(result["workspace"]["workspace_id"], legacy_id);
        assert_eq!(result["workspace"]["label"], "projeto");
        assert_eq!(result["workspace"].get("legacy"), None);
        assert_eq!(result["workspace"]["pane_count"], panes_before);
        let manifest = folder::read_manifest(&folder).unwrap();
        assert_eq!(result["workspace"]["folder"]["id"], manifest.id);
        assert!(folder.join(".regents/agents/chief-of-staff.md").is_file());
        let registered = fixture.registered();
        assert_eq!(registered.len(), 1);
        assert_eq!(
            registered[0].workspace_id.as_deref(),
            Some(legacy_id.as_str())
        );
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn link_refuses_a_folder_open_in_another_workspace() {
        let mut fixture = Fixture::new("link-in-use");
        let folder = fixture.folder("projeto");
        fixture.add(&folder);
        let legacy_id = fixture.legacy_workspace();
        let registry_path = fixture.app.workspace_registry_path.clone().unwrap();
        let registry_before = std::fs::read(&registry_path).unwrap();

        let response = fixture.request(
            "workspace.link",
            serde_json::json!({"workspace_id": legacy_id, "path": folder}),
        );

        assert_eq!(error_code(&response), "workspace_folder_in_use");
        let index = fixture.app.parse_workspace_id(&legacy_id).unwrap();
        assert!(fixture.app.workspace_info(index).legacy);
        assert_eq!(std::fs::read(&registry_path).unwrap(), registry_before);
    }

    #[tokio::test]
    async fn link_refuses_a_copy_of_a_registered_folder() {
        let mut fixture = Fixture::new("link-copy");
        let original = fixture.folder("original");
        fixture.add(&original);
        fixture.close_all();
        let copy = fixture.folder("copy");
        std::fs::create_dir_all(copy.join(".regents")).unwrap();
        std::fs::copy(
            original.join(".regents/workspace.toml"),
            copy.join(".regents/workspace.toml"),
        )
        .unwrap();
        let legacy_id = fixture.legacy_workspace();

        let response = fixture.request(
            "workspace.link",
            serde_json::json!({"workspace_id": legacy_id, "path": copy}),
        );

        assert_eq!(error_code(&response), "workspace_id_conflict");
        assert!(fixture.app.workspace_info(0).legacy);
    }

    #[tokio::test]
    async fn link_refuses_a_workspace_that_has_a_folder() {
        let mut fixture = Fixture::new("link-bound");
        let folder = fixture.folder("projeto");
        let other = fixture.folder("other");
        let (bound, _, _) = fixture.add(&folder);

        let response = fixture.request(
            "workspace.link",
            serde_json::json!({"workspace_id": bound.workspace_id, "path": other}),
        );

        assert_eq!(error_code(&response), "workspace_not_legacy");
        assert!(!other.join(".regents").exists());
    }

    #[tokio::test]
    async fn link_refuses_an_unknown_workspace() {
        let mut fixture = Fixture::new("link-unknown");
        let folder = fixture.folder("projeto");

        let response = fixture.request(
            "workspace.link",
            serde_json::json!({"workspace_id": "w99", "path": folder}),
        );

        assert_eq!(error_code(&response), "workspace_not_found");
        assert!(!folder.join(".regents").exists());
    }

    #[tokio::test]
    async fn herdr_has_no_workspace_link() {
        let mut fixture = Fixture::new("herdr-link");
        let legacy_id = fixture.legacy_workspace();
        fixture.app.workspace_registry_path = None;
        let folder = fixture.folder("projeto");

        let response = fixture.request(
            "workspace.link",
            serde_json::json!({"workspace_id": legacy_id, "path": folder}),
        );

        assert_eq!(error_code(&response), "unsupported");
        assert!(!folder.join(".regents").exists());
    }

    #[tokio::test]
    async fn the_folder_guard_refuses_a_legacy_workspace() {
        let mut fixture = Fixture::new("guard");
        let legacy_id = fixture.legacy_workspace();
        let folder = fixture.folder("projeto");
        let (bound, _, _) = fixture.add(&folder);

        let err = fixture
            .app
            .folder_workspace(&legacy_id)
            .expect_err("a legacy workspace has no folder");
        assert_eq!(err.code(), "workspace_legacy");
        let message = err.to_string();
        assert!(message.contains(&legacy_id), "{message}");
        assert!(message.contains("regents workspace link"), "{message}");

        let (index, binding) = fixture.app.folder_workspace(&bound.workspace_id).unwrap();
        assert_eq!(fixture.app.public_workspace_id(index), bound.workspace_id);
        assert_eq!(binding.root, folder);

        let missing = fixture.app.folder_workspace("w99").unwrap_err();
        assert_eq!(missing.code(), "workspace_not_found");
    }

    #[tokio::test]
    async fn herdr_has_no_folder_workspaces() {
        let mut fixture = Fixture::new("herdr");
        fixture.app.workspace_registry_path = None;
        let folder = fixture.folder("projeto");

        let response = fixture.app.handle_workspace_add(
            "add".into(),
            WorkspaceAddParams {
                path: folder.display().to_string(),
                focus: false,
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "unsupported");
        assert!(!folder.join(".regents").exists());
        assert!(fixture.app.state.workspaces.is_empty());
    }

    #[tokio::test]
    async fn a_relative_path_is_refused() {
        let mut fixture = Fixture::new("relative");

        let response = fixture.app.handle_workspace_add(
            "add".into(),
            WorkspaceAddParams {
                path: "projeto".into(),
                focus: false,
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_params");
    }
}
