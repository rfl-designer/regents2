//! Integration tests for the `regents` binary built from the herdr sources.

#![cfg(unix)]

pub mod support;

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};

const HERDR_BIN: &str = env!("CARGO_BIN_EXE_herdr");
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

fn dir_name(product: &str) -> String {
    if cfg!(debug_assertions) {
        format!("{product}-dev")
    } else {
        product.to_string()
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
        let base = PathBuf::from(format!("/tmp/regents-bin-{}-{nanos}", std::process::id()));
        for dir in ["config", "state", "home"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        register_runtime_dir(&base.join("runtime"));
        for product in ["herdr", "regents"] {
            let config_dir = base.join("config").join(dir_name(product));
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(config_dir.join("config.toml"), "onboarding = false\n").unwrap();
        }
        Self { base }
    }

    fn api_socket(&self, product: &str) -> PathBuf {
        self.base
            .join("config")
            .join(dir_name(product))
            .join("herdr.sock")
    }

    fn envs(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.base.join("home")),
            ("XDG_CONFIG_HOME", self.base.join("config")),
            ("XDG_STATE_HOME", self.base.join("state")),
            ("XDG_RUNTIME_DIR", self.base.join("runtime")),
        ]
    }

    fn command(&self, bin: &str) -> Command {
        let mut cmd = Command::new(bin);
        for var in INHERITED_OVERRIDES {
            cmd.env_remove(var);
        }
        cmd.envs(self.envs());
        cmd
    }

    fn spawn_server(&self, bin: &str) -> SpawnedServer {
        self.spawn_server_with_env(bin, &[])
    }

    fn spawn_server_with_env(&self, bin: &str, extra: &[(&str, &Path)]) -> SpawnedServer {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(bin);
        for var in INHERITED_OVERRIDES {
            cmd.env_remove(var);
        }
        for (key, value) in self.envs() {
            cmd.env(key, value);
        }
        for (key, value) in extra {
            cmd.env(key, value);
        }
        cmd.env("SHELL", "/bin/sh");
        cmd.arg("server");
        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);
        SpawnedServer {
            _master: pair.master,
            child,
        }
    }

    /// Runs `<bin> status server` and returns its stdout.
    fn server_status(&self, bin: &str) -> String {
        self.server_status_with_env(bin, &[])
    }

    fn server_status_with_env(&self, bin: &str, extra: &[(&str, &Path)]) -> String {
        let output = self
            .command(bin)
            .envs(extra.iter().copied())
            .args(["status", "server"])
            .output()
            .unwrap();
        assert!(output.status.success(), "status failed: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
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
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn accepts_connections(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
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

#[test]
fn regents_version_shows_regents_brand() {
    let output = Command::new(REGENTS_BIN)
        .env_remove("HERDR_ENV")
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("regents "),
        "unexpected version output: {stdout:?}"
    );
    assert!(!stdout.contains("herdr"), "version output: {stdout:?}");
}

#[test]
fn regents_and_herdr_servers_run_side_by_side_without_seeing_each_other() {
    let sandbox = Sandbox::new();
    let herdr_socket = sandbox.api_socket("herdr");
    let regents_socket = sandbox.api_socket("regents");

    let _herdr = sandbox.spawn_server(HERDR_BIN);
    assert!(
        wait_until(Duration::from_secs(10), || accepts_connections(
            &herdr_socket
        )),
        "herdr server socket did not appear at {}",
        herdr_socket.display()
    );

    let regents_status = sandbox.server_status(REGENTS_BIN);
    assert!(
        regents_status.contains("status: not running"),
        "regents must not see the herdr server: {regents_status}"
    );

    let _regents = sandbox.spawn_server(REGENTS_BIN);
    assert!(
        wait_until(Duration::from_secs(10), || accepts_connections(
            &regents_socket
        )),
        "regents server socket did not appear at {}",
        regents_socket.display()
    );

    let regents_status = sandbox.server_status(REGENTS_BIN);
    assert!(
        regents_status.contains("status: running"),
        "{regents_status}"
    );
    assert!(
        regents_status.contains(&regents_socket.display().to_string()),
        "{regents_status}"
    );
    let herdr_status = sandbox.server_status(HERDR_BIN);
    assert!(herdr_status.contains("status: running"), "{herdr_status}");
    assert!(
        herdr_status.contains(&herdr_socket.display().to_string()),
        "{herdr_status}"
    );

    let stop = sandbox
        .command(REGENTS_BIN)
        .args(["server", "stop"])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "regents server stop failed: {stop:?}"
    );
    assert!(
        wait_until(Duration::from_secs(10), || !accepts_connections(
            &regents_socket
        )),
        "regents server kept running after stop"
    );
    assert!(
        accepts_connections(&herdr_socket),
        "stopping regents must not stop the herdr server"
    );
}

/// Environment that a server of `bin` sets for its panes.
fn pane_env<'a>(bin: &'a str, api_socket: &'a Path) -> Vec<(&'static str, &'a Path)> {
    vec![
        ("HERDR_ENV", Path::new("1")),
        ("HERDR_BIN_PATH", Path::new(bin)),
        ("HERDR_SOCKET_PATH", api_socket),
        ("HERDR_PANE_ID", Path::new("w1-1")),
    ]
}

#[test]
fn regents_launched_from_a_herdr_pane_ignores_the_herdr_server() {
    let sandbox = Sandbox::new();
    let herdr_socket = sandbox.api_socket("herdr");
    let regents_socket = sandbox.api_socket("regents");

    let _herdr = sandbox.spawn_server(HERDR_BIN);
    assert!(
        wait_until(Duration::from_secs(10), || accepts_connections(
            &herdr_socket
        )),
        "herdr server socket did not appear at {}",
        herdr_socket.display()
    );

    let status = sandbox.server_status_with_env(REGENTS_BIN, &pane_env(HERDR_BIN, &herdr_socket));
    assert!(
        status.contains("status: not running"),
        "regents inside a herdr pane must not use the herdr server: {status}"
    );
    assert!(
        status.contains(&regents_socket.display().to_string()),
        "{status}"
    );
}

#[test]
fn regents_launched_from_its_own_pane_keeps_its_server_socket() {
    let sandbox = Sandbox::new();
    let custom_socket = sandbox.base.join("custom-regents.sock");

    let _regents =
        sandbox.spawn_server_with_env(REGENTS_BIN, &[("HERDR_SOCKET_PATH", &custom_socket)]);
    assert!(
        wait_until(Duration::from_secs(10), || accepts_connections(
            &custom_socket
        )),
        "regents server socket did not appear at {}",
        custom_socket.display()
    );

    let status =
        sandbox.server_status_with_env(REGENTS_BIN, &pane_env(REGENTS_BIN, &custom_socket));
    assert!(status.contains("status: running"), "{status}");
    assert!(
        status.contains(&custom_socket.display().to_string()),
        "{status}"
    );
}

#[test]
fn regents_refuses_to_self_update_from_herdr_releases() {
    let sandbox = Sandbox::new();

    let output = sandbox.command(REGENTS_BIN).arg("update").output().unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("self-update is disabled for regents"),
        "{stderr}"
    );
}

/// Runs `regents workspace add` with no server running and returns its output.
fn regents_workspace_add_without_server(sandbox: &Sandbox, extra: &[(&str, &str)]) -> String {
    let folder = sandbox.base.join("home").join("ws");
    fs::create_dir_all(&folder).unwrap();
    let output = sandbox
        .command(REGENTS_BIN)
        .envs(extra.iter().copied())
        .args(["workspace", "add"])
        .arg(&folder)
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn regents_server_not_running_error_names_regents() {
    let sandbox = Sandbox::new();

    let output = regents_workspace_add_without_server(&sandbox, &[]);

    assert!(output.contains("server_not_running"), "{output}");
    assert!(output.contains("no regents server is running"), "{output}");
    assert!(
        output.contains("run `regents` to start or attach it"),
        "{output}"
    );
    assert!(!output.contains("herdr server"), "{output}");
}

#[test]
fn regents_server_not_running_error_suggests_regents_session_attach() {
    let sandbox = Sandbox::new();

    let output = regents_workspace_add_without_server(&sandbox, &[("HERDR_SESSION", "work")]);

    assert!(
        output.contains("run `regents session attach work` to start or attach it"),
        "{output}"
    );
}

#[test]
fn regents_server_not_running_error_with_socket_override_suggests_regents() {
    let sandbox = Sandbox::new();
    let custom_socket = sandbox.base.join("custom.sock");

    let output = regents_workspace_add_without_server(
        &sandbox,
        &[
            ("HERDR_SESSION", "work"),
            ("HERDR_SOCKET_PATH", custom_socket.to_str().unwrap()),
        ],
    );

    assert!(
        output.contains(&format!(
            "no regents server is running at {}; run `regents` to start or attach it",
            custom_socket.display()
        )),
        "{output}"
    );
}
