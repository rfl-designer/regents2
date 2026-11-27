//! Folder workspaces of the `regents` binary: `workspace add`, `workspace
//! list`, the sidebar name and the restore after a restart (ticket #3); legacy
//! workspaces, `workspace link` and the reopen of the last used folder (#4);
//! disconnected workspaces, `workspace relink` and `workspace remove` (#5).

#![cfg(unix)]

mod regents_support;
pub mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use regents_support::{contains, tree, wait_until, Sandbox};

#[test]
fn workspace_add_creates_the_structure_and_lists_the_workspace_once() {
    let sandbox = Sandbox::new();
    let folder = sandbox.folder("projeto");
    let _server = sandbox.start_server();
    let open_before = sandbox.open_workspace_count();

    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);

    assert_eq!(added["type"], "workspace_added", "{added}");
    assert_eq!(added["initialized"], true);
    assert_eq!(added["already_registered"], false);
    assert_eq!(added["workspace"]["label"], "projeto");
    let id = added["workspace"]["folder"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let workspace_id = added["workspace"]["workspace_id"].clone();
    let state = folder.join(".regents");
    for dir in ["agents", "channels/geral", "wiki/raw"] {
        assert!(state.join(dir).is_dir(), "missing {dir}");
    }
    for file in [
        "workspace.toml",
        "agents/chief-of-staff.md",
        "wiki/index.md",
        "wiki/log.md",
        "wiki/SCHEMA.md",
    ] {
        assert!(state.join(file).is_file(), "missing {file}");
    }
    assert!(fs::read_to_string(state.join("workspace.toml"))
        .unwrap()
        .contains(&id));
    let registry = fs::read_to_string(sandbox.registry_file()).unwrap();
    assert!(registry.contains(folder.to_str().unwrap()), "{registry}");
    let expected_list = serde_json::json!([{
        "path": folder, "id": id, "name": "projeto", "workspace_id": workspace_id
    }]);
    assert_eq!(
        sandbox.cli(&["workspace", "list"])["workspaces"],
        expected_list
    );
    assert_eq!(sandbox.open_workspace_count(), open_before + 1);

    let again = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);

    assert_eq!(again["already_registered"], true);
    assert_eq!(again["initialized"], false);
    assert_eq!(again["workspace"]["workspace_id"], workspace_id);
    assert_eq!(
        sandbox.cli(&["workspace", "list"])["workspaces"],
        expected_list
    );
    assert_eq!(sandbox.open_workspace_count(), open_before + 1);
}

#[test]
fn the_workspace_name_comes_from_workspace_toml_and_not_from_the_folder() {
    let sandbox = Sandbox::new();
    let folder = sandbox.folder("projeto");
    let _server = sandbox.start_server();
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    let id = added["workspace"]["folder"]["id"].clone();
    sandbox.api(
        "workspace.close",
        serde_json::json!({"workspace_id": added["workspace"]["workspace_id"]}),
    );
    let manifest = folder.join(".regents/workspace.toml");
    let renamed = fs::read_to_string(&manifest)
        .unwrap()
        .replace("name = \"projeto\"", "name = \"Nome do Manifesto\"");
    assert!(renamed.contains("Nome do Manifesto"), "{renamed}");
    fs::write(&manifest, renamed).unwrap();

    let reopened = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);

    assert_eq!(reopened["already_registered"], true);
    assert_eq!(reopened["workspace"]["folder"]["id"], id);
    assert_eq!(reopened["workspace"]["label"], "Nome do Manifesto");
    assert_eq!(
        sandbox.cli(&["workspace", "list"])["workspaces"][0]["name"],
        "Nome do Manifesto"
    );
    assert!(
        contains(&sandbox.client_shell_snapshot(), b"Nome do Manifesto"),
        "the sidebar must show the name from workspace.toml"
    );
}

#[test]
fn workspace_add_keeps_the_state_of_a_folder_from_another_machine() {
    let sandbox = Sandbox::new();
    let folder = sandbox.folder("clone");
    let state = folder.join(".regents");
    fs::create_dir_all(state.join("agents")).unwrap();
    fs::create_dir_all(state.join("wiki")).unwrap();
    fs::write(
        state.join("workspace.toml"),
        "id = \"0123456789abcdef0123456789abcdef\"\nname = \"Equipe Alfa\"\nmax_active_threads = 5\n",
    )
    .unwrap();
    fs::write(state.join("agents/chief-of-staff.md"), "my own chief\n").unwrap();
    fs::write(state.join("wiki/index.md"), "# pages from machine A\n").unwrap();
    let before = tree(&folder);
    let _server = sandbox.start_server();

    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);

    assert_eq!(added["initialized"], false);
    assert_eq!(
        added["workspace"]["folder"]["id"],
        "0123456789abcdef0123456789abcdef"
    );
    assert_eq!(added["workspace"]["label"], "Equipe Alfa");
    let after = tree(&folder);
    for (path, content) in &before {
        assert_eq!(after.get(path), Some(content), "{} changed", path.display());
    }
    assert!(
        contains(&sandbox.client_shell_snapshot(), b"Equipe Alfa"),
        "the sidebar must show the workspace name"
    );
}

#[test]
fn folder_workspace_comes_back_after_a_restart_with_its_id_and_layout() {
    let sandbox = Sandbox::new();
    let folder = sandbox.folder("projeto");
    let elsewhere = sandbox.folder("elsewhere");
    let server = sandbox.start_server();
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    let id = added["workspace"]["folder"]["id"].clone();
    let workspace_id = added["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let panes = sandbox.api(
        "pane.list",
        serde_json::json!({"workspace_id": workspace_id}),
    )["panes"]
        .clone();
    let root_pane = panes[0]["pane_id"].as_str().unwrap().to_owned();
    sandbox.api(
        "pane.split",
        serde_json::json!({"target_pane_id": root_pane, "direction": "right"}),
    );
    sandbox.api(
        "pane.send_text",
        serde_json::json!({
            "pane_id": root_pane,
            "text": format!("cd '{}'\n", elsewhere.display()),
        }),
    );
    let elsewhere_cwd = elsewhere.display().to_string();
    assert!(
        sandbox.wait_for_pane_cwd(&root_pane, &elsewhere_cwd),
        "the root pane never reported its new cwd"
    );
    let before = sandbox.api(
        "workspace.get",
        serde_json::json!({"workspace_id": workspace_id}),
    )["workspace"]
        .clone();
    assert_eq!(before["pane_count"], 2);
    assert_eq!(before["folder"]["id"], id, "a cwd change must keep the id");
    assert_eq!(before["workspace_id"], workspace_id);
    assert_eq!(before["label"], "projeto");

    sandbox.stop_server(server);
    let _server = sandbox.start_server();

    assert!(
        sandbox.wait_for_pane_cwd(&root_pane, &elsewhere_cwd),
        "the restored root pane must keep its saved cwd"
    );
    let after = sandbox.api(
        "workspace.get",
        serde_json::json!({"workspace_id": workspace_id}),
    )["workspace"]
        .clone();
    assert_eq!(after["workspace_id"], workspace_id);
    assert_eq!(after["folder"]["id"], id);
    assert_eq!(after["label"], "projeto");
    assert_eq!(after["pane_count"], 2);
    assert_eq!(after["tab_count"], before["tab_count"]);
    let listed = sandbox.cli(&["workspace", "list"])["workspaces"].clone();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["workspace_id"], workspace_id);
    let again = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    assert_eq!(again["workspace"]["workspace_id"], workspace_id);
}

/// A `session.json` from before Regents: two workspaces without a folder, the
/// first with two panes side by side.
fn write_old_session(sandbox: &Sandbox, cwd: &Path) {
    let cwd = cwd.display().to_string();
    let session = serde_json::json!({
        "version": 3,
        "workspaces": [
            {
                "id": "wold1",
                "custom_name": "antigo",
                "identity_cwd": cwd,
                "tabs": [{
                    "custom_name": null,
                    "layout": {"Split": {
                        "direction": "Vertical",
                        "ratio": 0.5,
                        "first": {"Pane": 1},
                        "second": {"Pane": 2}
                    }},
                    "panes": {"1": {"cwd": cwd}, "2": {"cwd": cwd}},
                    "zoomed": false,
                    "focused": 1,
                    "root_pane": 1
                }],
                "active_tab": 0
            },
            {
                "id": "wold2",
                "custom_name": null,
                "identity_cwd": cwd,
                "tabs": [{
                    "custom_name": null,
                    "layout": {"Pane": 3},
                    "panes": {"3": {"cwd": cwd}},
                    "zoomed": false,
                    "focused": 3,
                    "root_pane": 3
                }],
                "active_tab": 0
            }
        ],
        "active": 0,
        "selected": 0
    });
    fs::write(
        sandbox.session_file(),
        serde_json::to_string_pretty(&session).unwrap(),
    )
    .unwrap();
}

#[test]
fn creating_a_workspace_without_a_folder_is_refused() {
    let sandbox = Sandbox::new();
    let cwd = sandbox.folder("solto");
    let _server = sandbox.start_server();

    let error = sandbox.cli_error(&["workspace", "create", "--cwd", cwd.to_str().unwrap()]);

    assert_eq!(error["code"], "workspace_folder_required", "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("regents workspace add"),
        "{error}"
    );
    assert!(sandbox.open_workspaces().is_empty());
}

#[test]
fn an_old_session_restores_as_legacy_workspaces_and_link_binds_one() {
    let sandbox = Sandbox::new();
    let cwd = sandbox.folder("antigo");
    let folder = sandbox.folder("projeto");
    write_old_session(&sandbox, &cwd);
    let server = sandbox.start_server();

    let restored = sandbox.open_workspaces();

    assert_eq!(restored.len(), 2, "{restored:?}");
    assert_eq!(restored[0]["workspace_id"], "wold1");
    assert_eq!(restored[0]["label"], "antigo");
    assert_eq!(restored[0]["pane_count"], 2);
    assert_eq!(restored[1]["workspace_id"], "wold2");
    assert_eq!(restored[1]["pane_count"], 1);
    for workspace in &restored {
        assert_eq!(workspace["legacy"], true, "{workspace}");
        assert!(workspace.get("folder").is_none(), "{workspace}");
    }
    let listed = sandbox.cli(&["workspace", "list"]);
    assert_eq!(listed["workspaces"], serde_json::json!([]));
    assert_eq!(
        listed["legacy"],
        serde_json::json!([
            {"workspace_id": "wold1", "label": "antigo"},
            {"workspace_id": "wold2", "label": restored[1]["label"]},
        ])
    );
    assert!(
        contains(&sandbox.client_shell_snapshot(), b"\"legacy\":true"),
        "the sidebar data must mark the legacy workspaces"
    );

    let linked = sandbox.cli(&["workspace", "link", "wold1", folder.to_str().unwrap()]);

    assert_eq!(linked["type"], "workspace_linked", "{linked}");
    assert_eq!(linked["initialized"], true);
    let workspace = &linked["workspace"];
    assert_eq!(workspace["workspace_id"], "wold1");
    assert_eq!(workspace["label"], "projeto");
    assert_eq!(workspace["pane_count"], 2);
    assert!(workspace.get("legacy").is_none(), "{workspace}");
    let id = workspace["folder"]["id"].clone();
    assert!(folder.join(".regents/workspace.toml").is_file());
    assert!(folder.join(".regents/channels/geral").is_dir());
    let listed = sandbox.cli(&["workspace", "list"]);
    assert_eq!(listed["workspaces"][0]["workspace_id"], "wold1");
    assert_eq!(listed["workspaces"][0]["id"], id);
    assert_eq!(listed["legacy"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["legacy"][0]["workspace_id"], "wold2");

    let error = sandbox.cli_error(&["workspace", "link", "wold2", folder.to_str().unwrap()]);
    assert_eq!(error["code"], "workspace_folder_in_use", "{error}");

    sandbox.stop_server(server);
    let _server = sandbox.start_server();

    let after = sandbox.open_workspaces();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0]["folder"]["id"], id);
    assert!(after[0].get("legacy").is_none());
    assert_eq!(after[0]["pane_count"], 2);
    assert_eq!(after[1]["legacy"], true);
}

#[test]
fn without_a_session_regents_reopens_the_last_used_folder() {
    let sandbox = Sandbox::new();
    let first = sandbox.folder("primeiro");
    let second = sandbox.folder("segundo");
    let server = sandbox.start_server();
    sandbox.cli(&["workspace", "add", first.to_str().unwrap()]);
    let last = sandbox.cli(&["workspace", "add", second.to_str().unwrap()]);
    sandbox.stop_server(server);
    fs::remove_file(sandbox.session_file()).unwrap();

    let server = sandbox.start_server();

    let open = sandbox.open_workspaces();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["folder"], last["workspace"]["folder"]);
    assert_eq!(open[0]["label"], "segundo");

    sandbox.stop_server(server);
    fs::remove_file(sandbox.session_file()).unwrap();
    fs::remove_dir_all(&second).unwrap();
    let _server = sandbox.start_server();

    let open = sandbox.open_workspaces();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["label"], "primeiro");
}

#[test]
fn with_no_usable_folder_a_client_attaches_to_zero_workspaces() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();

    let snapshot = sandbox.client_shell_snapshot();

    assert!(
        contains(&snapshot, b"\"workspaces\":[]"),
        "{}",
        String::from_utf8_lossy(&snapshot)
    );
    assert!(
        contains(
            &snapshot,
            b"\"workspace_add_command\":\"regents workspace add <folder>\""
        ),
        "the snapshot must tell clients how to add a workspace: {}",
        String::from_utf8_lossy(&snapshot)
    );
    assert!(sandbox.open_workspaces().is_empty());
    assert_eq!(sandbox.api("ping", serde_json::json!({}))["type"], "pong");
}

#[test]
fn a_folder_moved_while_the_server_runs_becomes_disconnected_without_a_list() {
    let sandbox = Sandbox::new();
    let folder = sandbox.folder("projeto");
    let _server = sandbox.start_server();
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    let workspace_id = added["workspace"]["workspace_id"].clone();
    let get = || {
        sandbox.api(
            "workspace.get",
            serde_json::json!({"workspace_id": workspace_id}),
        )["workspace"]
            .clone()
    };
    assert!(get().get("disconnected").is_none());

    let moved = folder.with_file_name("movido");
    fs::rename(&folder, &moved).unwrap();

    // `workspace.get` reads no folder: only the periodic check of the
    // server loop can mark the workspace.
    assert!(
        wait_until(Duration::from_secs(30), || get()["disconnected"] == true),
        "the server must mark the moved folder: {}",
        get()
    );
    assert!(
        contains(&sandbox.client_shell_snapshot(), b"\"disconnected\":true"),
        "the sidebar data must mark the disconnected workspace"
    );

    fs::rename(&moved, &folder).unwrap();

    assert!(
        wait_until(Duration::from_secs(30), || get()
            .get("disconnected")
            .is_none()),
        "the server must clear the mark when the folder is back: {}",
        get()
    );
}

#[test]
fn a_moved_folder_is_disconnected_until_relink_and_remove_keeps_the_folder() {
    let sandbox = Sandbox::new();
    let folder = sandbox.folder("projeto");
    let other = sandbox.folder("outro");
    let server = sandbox.start_server();
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    let id = added["workspace"]["folder"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let workspace_id = added["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    sandbox.cli(&["workspace", "add", other.to_str().unwrap()]);
    sandbox.stop_server(server);
    let moved = folder.with_file_name("movido");
    fs::rename(&folder, &moved).unwrap();
    let _server = sandbox.start_server();

    let listed = sandbox.cli(&["workspace", "list"]);

    let entry = &listed["workspaces"][0];
    assert_eq!(entry["path"], folder.to_str().unwrap(), "{listed}");
    assert_eq!(entry["id"], id);
    assert_eq!(entry["disconnected"], true, "{listed}");
    assert!(listed["workspaces"][1].get("disconnected").is_none());
    let open = sandbox.open_workspaces();
    let restored = open
        .iter()
        .find(|workspace| workspace["workspace_id"] == workspace_id.as_str())
        .unwrap_or_else(|| panic!("the disconnected workspace must stay open: {open:?}"));
    assert_eq!(restored["disconnected"], true, "{restored}");
    assert!(
        contains(&sandbox.client_shell_snapshot(), b"\"disconnected\":true"),
        "the sidebar data must mark the disconnected workspace"
    );

    let error = sandbox.cli_error(&["workspace", "relink", &id, other.to_str().unwrap()]);
    assert_eq!(error["code"], "workspace_id_mismatch", "{error}");

    let relinked = sandbox.cli(&["workspace", "relink", &id, moved.to_str().unwrap()]);

    assert_eq!(relinked["type"], "workspace_relinked", "{relinked}");
    let listed = sandbox.cli(&["workspace", "list"]);
    assert_eq!(listed["workspaces"][0]["path"], moved.to_str().unwrap());
    assert!(listed["workspaces"][0].get("disconnected").is_none());
    let open = sandbox.open_workspaces();
    let relinked = open
        .iter()
        .find(|workspace| workspace["workspace_id"] == workspace_id.as_str())
        .unwrap();
    assert!(relinked.get("disconnected").is_none(), "{relinked}");
    assert_eq!(relinked["folder"]["path"], moved.to_str().unwrap());

    let error = sandbox.cli_error(&["workspace", "remove", &id]);
    assert_eq!(error["code"], "workspace_open", "{error}");
    sandbox.cli(&["workspace", "close", &workspace_id]);
    let before = tree(&moved);

    let removed = sandbox.cli(&["workspace", "remove", &id]);

    assert_eq!(removed["type"], "workspace_removed", "{removed}");
    let listed = sandbox.cli(&["workspace", "list"])["workspaces"].clone();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["path"], other.to_str().unwrap());
    assert_eq!(tree(&moved), before, "remove must not change the folder");
}
