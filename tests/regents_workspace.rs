//! Folder workspaces of the `regents` binary: `workspace add`, `workspace
//! list`, the sidebar name and the restore after a restart (ticket #3); legacy
//! workspaces, `workspace link` and the reopen of the last used folder (#4).

#![cfg(unix)]

pub mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};

const REGENTS_BIN: &str = env!("CARGO_BIN_EXE_regents");

/// Environment variables that could point a spawned process at a server
/// outside the test sandbox.
const INHERITED_OVERRIDES: &[&str] = &[
    "HERDR_ENV",
    "HERDR_SESSION",
    "HERDR_STARTUP_CWD",
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_CONFIG_PATH",
    "HERDR_BIN_PATH",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
];

fn regents_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "regents-dev"
    } else {
        "regents"
    }
}

struct Sandbox {
    base: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = PathBuf::from(format!("/tmp/regents-ws-{}-{nanos}", std::process::id()));
        for dir in ["config", "state", "home", "work"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        let base = fs::canonicalize(base).unwrap();
        register_runtime_dir(&base.join("runtime"));
        let config_dir = base.join("config").join(regents_dir_name());
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\n",
        )
        .unwrap();
        Self { base }
    }

    fn api_socket(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("herdr.sock")
    }

    fn client_socket(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("herdr-client.sock")
    }

    fn registry_file(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("workspaces.toml")
    }

    /// A new, canonical folder under the sandbox.
    fn folder(&self, name: &str) -> PathBuf {
        let path = self.base.join("work").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn envs(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.base.join("home")),
            ("XDG_CONFIG_HOME", self.base.join("config")),
            ("XDG_STATE_HOME", self.base.join("state")),
            ("XDG_RUNTIME_DIR", self.base.join("runtime")),
        ]
    }

    fn start_server(&self) -> SpawnedServer {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(REGENTS_BIN);
        for var in INHERITED_OVERRIDES {
            cmd.env_remove(var);
        }
        for (key, value) in self.envs() {
            cmd.env(key, value);
        }
        cmd.env("SHELL", "/bin/sh");
        cmd.cwd(&self.base);
        cmd.arg("server");
        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);
        let server = SpawnedServer {
            _master: pair.master,
            child,
        };
        assert!(
            wait_until(Duration::from_secs(10), || UnixStream::connect(
                self.api_socket()
            )
            .is_ok()),
            "regents server socket did not appear at {}",
            self.api_socket().display()
        );
        server
    }

    fn stop_server(&self, mut server: SpawnedServer) {
        let output = self.cli_output(&["server", "stop"]);
        assert!(output.status.success(), "server stop failed: {output:?}");
        let pid = server.child.process_id();
        assert!(
            wait_until(Duration::from_secs(10), || {
                server.child.try_wait().ok().flatten().is_some()
            }),
            "regents server kept running after stop"
        );
        unregister_spawned_herdr_pid(pid);
    }

    fn cli_output(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(REGENTS_BIN);
        for var in INHERITED_OVERRIDES {
            cmd.env_remove(var);
        }
        cmd.envs(self.envs()).current_dir(&self.base).args(args);
        cmd.output().unwrap()
    }

    /// Runs a `regents` command and returns its JSON `result`.
    fn cli(&self, args: &[&str]) -> Value {
        let output = self.cli_output(args);
        assert!(
            output.status.success(),
            "regents {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        response["result"].clone()
    }

    /// Runs a `regents` command that must fail and returns its JSON `error`.
    fn cli_error(&self, args: &[&str]) -> Value {
        let output = self.cli_output(args);
        assert!(
            !output.status.success(),
            "regents {args:?} must fail: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let response: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|err| {
            panic!(
                "regents {args:?} stderr is not JSON ({err}): {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        response["error"].clone()
    }

    fn session_file(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("session.json")
    }

    fn open_workspaces(&self) -> Vec<Value> {
        self.api("workspace.list", serde_json::json!({}))["workspaces"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Sends one JSON API request and returns its `result`.
    fn api(&self, method: &str, params: Value) -> Value {
        let mut stream = UnixStream::connect(self.api_socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": "test", "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    /// Polls `pane.get` until the pane reports `cwd`.
    fn wait_for_pane_cwd(&self, pane_id: &str, cwd: &str) -> bool {
        wait_until(Duration::from_secs(10), || {
            self.api("pane.get", serde_json::json!({"pane_id": pane_id}))["pane"]["cwd"] == cwd
        })
    }

    fn open_workspace_count(&self) -> usize {
        self.api("workspace.list", serde_json::json!({}))["workspaces"]
            .as_array()
            .unwrap()
            .len()
    }

    /// Bytes of the first client shell snapshot, the data that the sidebar draws.
    fn client_shell_snapshot(&self) -> Vec<u8> {
        // The server binds the client socket after the API socket.
        assert!(
            wait_until(Duration::from_secs(10), || UnixStream::connect(
                self.client_socket()
            )
            .is_ok()),
            "regents client socket did not appear"
        );
        let mut stream = UnixStream::connect(self.client_socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, error) = support::client_shell_handshake(&mut stream, 1, 80, 24).unwrap();
        assert_eq!(error, None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok((support::SERVER_MESSAGE_ENDPOINT_CONTROL, payload)) =
                support::read_server_message(&mut stream)
            {
                if contains(&payload, b"shell.snapshot.v1") {
                    return payload;
                }
            }
        }
        panic!("no client shell snapshot");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        cleanup_test_base(&self.base);
    }
}

struct SpawnedServer {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
        let _ = wait_until(Duration::from_secs(2), || {
            self.child.try_wait().ok().flatten().is_some()
        });
        unregister_spawned_herdr_pid(pid);
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    condition()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Every file and directory under `root`, with the contents of each file.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(relative, None);
                walk(root, &path, out);
            } else {
                out.insert(relative, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

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
