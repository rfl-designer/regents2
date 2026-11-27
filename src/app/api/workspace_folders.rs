//! Folder workspaces (Regents): `workspace.add` and `workspace.registry.list`.
//! The domain rules live in `crate::regents`; this module binds a folder to the
//! herdr workspace that holds its layout.

use crate::api::schema::{RegisteredWorkspaceInfo, ResponseResult, WorkspaceAddParams};
use crate::app::App;
use crate::regents::folder::{self, FolderBinding};
use crate::regents::registry::{self, Registry};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_workspace_add(
        &mut self,
        id: String,
        params: WorkspaceAddParams,
    ) -> String {
        let Some(registry_path) = self.workspace_registry_path.clone() else {
            return folder_workspaces_unsupported(id);
        };
        let path = crate::worktree::expand_tilde_path(&params.path);
        if !path.is_absolute() {
            return encode_error(
                id,
                "invalid_params",
                format!("workspace path must be absolute: {}", params.path),
            );
        }
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
                    let workspace = &mut self.state.workspaces[index];
                    workspace.folder = Some(FolderBinding {
                        id: added.folder.manifest.id.clone(),
                        root: added.root.clone(),
                    });
                    workspace.set_custom_name(added.folder.manifest.name.clone());
                    self.schedule_session_save();
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
        encode_success(id, ResponseResult::RegisteredWorkspaceList { workspaces })
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

        fn registered(&mut self) -> Vec<RegisteredWorkspaceInfo> {
            let response = self.app.handle_workspace_registry_list("list".into());
            match serde_json::from_str::<SuccessResponse>(&response)
                .unwrap()
                .result
            {
                ResponseResult::RegisteredWorkspaceList { workspaces } => workspaces,
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
