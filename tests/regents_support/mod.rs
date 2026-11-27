//! Shared harness for the `regents` integration tests: an isolated sandbox
//! (config, state and runtime directories), a real `regents server` in a PTY
//! and helpers that run `regents` commands and JSON API requests against it.

// Each test binary uses a different part of this harness.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;

pub const REGENTS_BIN: &str = env!("CARGO_BIN_EXE_regents");

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

pub fn regents_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "regents-dev"
    } else {
        "regents"
    }
}

pub struct Sandbox {
    pub base: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
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

    pub fn api_socket(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("herdr.sock")
    }

    pub fn client_socket(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("herdr-client.sock")
    }

    pub fn registry_file(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("workspaces.toml")
    }

    /// A new, canonical folder under the sandbox.
    pub fn folder(&self, name: &str) -> PathBuf {
        let path = self.base.join("work").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    pub fn envs(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.base.join("home")),
            ("XDG_CONFIG_HOME", self.base.join("config")),
            ("XDG_STATE_HOME", self.base.join("state")),
            ("XDG_RUNTIME_DIR", self.base.join("runtime")),
        ]
    }

    pub fn start_server(&self) -> SpawnedServer {
        self.start_server_with_env(&[])
    }

    /// Starts the server with extra environment, for example a `PATH` with
    /// fake agent CLIs. Panes inherit it.
    pub fn start_server_with_env(&self, extra: &[(&str, String)]) -> SpawnedServer {
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
        for (key, value) in extra {
            cmd.env(key, value);
        }
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

    pub fn stop_server(&self, mut server: SpawnedServer) {
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

    pub fn cli_output(&self, args: &[&str]) -> std::process::Output {
        self.cli_output_in(&self.base, args)
    }

    /// Runs a `regents` command with `cwd` as its working directory.
    pub fn cli_output_in(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(REGENTS_BIN);
        for var in INHERITED_OVERRIDES {
            cmd.env_remove(var);
        }
        cmd.envs(self.envs()).current_dir(cwd).args(args);
        cmd.output().unwrap()
    }

    /// Runs a `regents` command and returns its JSON `result`.
    pub fn cli(&self, args: &[&str]) -> Value {
        self.cli_in(&self.base, args)
    }

    /// Runs a `regents` command in `cwd` and returns its JSON `result`.
    pub fn cli_in(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.cli_output_in(cwd, args);
        assert!(
            output.status.success(),
            "regents {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        response["result"].clone()
    }

    /// Runs a `regents` command that must fail and returns its JSON `error`.
    pub fn cli_error(&self, args: &[&str]) -> Value {
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

    pub fn session_file(&self) -> PathBuf {
        self.base
            .join("config")
            .join(regents_dir_name())
            .join("session.json")
    }

    pub fn open_workspaces(&self) -> Vec<Value> {
        self.api("workspace.list", serde_json::json!({}))["workspaces"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Sends one JSON API request and returns its `result`.
    pub fn api(&self, method: &str, params: Value) -> Value {
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
    pub fn wait_for_pane_cwd(&self, pane_id: &str, cwd: &str) -> bool {
        wait_until(Duration::from_secs(10), || {
            self.api("pane.get", serde_json::json!({"pane_id": pane_id}))["pane"]["cwd"] == cwd
        })
    }

    pub fn open_workspace_count(&self) -> usize {
        self.api("workspace.list", serde_json::json!({}))["workspaces"]
            .as_array()
            .unwrap()
            .len()
    }

    /// Bytes of the first client shell snapshot, the data that the sidebar draws.
    pub fn client_shell_snapshot(&self) -> Vec<u8> {
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
        let (_, error) = crate::support::client_shell_handshake(&mut stream, 1, 80, 24).unwrap();
        assert_eq!(error, None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok((crate::support::SERVER_MESSAGE_ENDPOINT_CONTROL, payload)) =
                crate::support::read_server_message(&mut stream)
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

pub struct SpawnedServer {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let pid = self.child.process_id();
        crate::support::stop_spawned_herdr(&mut *self.child);
        let _ = wait_until(Duration::from_secs(2), || {
            self.child.try_wait().ok().flatten().is_some()
        });
        unregister_spawned_herdr_pid(pid);
    }
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    condition()
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Every file and directory under `root`, with the contents of each file.
pub fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
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
