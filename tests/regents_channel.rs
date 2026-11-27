//! Channels of the `regents` binary through the CLI and the JSON API:
//! `channel post`, `channel read`, the per-channel database and the
//! `channel.message_posted` event (ticket #6).

#![cfg(unix)]

mod regents_support;
pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regents_support::Sandbox;
use serde_json::Value;

/// Adds `name` as a workspace and returns its folder.
fn add_workspace(sandbox: &Sandbox, name: &str) -> PathBuf {
    let folder = sandbox.folder(name);
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    assert_eq!(added["type"], "workspace_added", "{added}");
    folder
}

fn channel_database(folder: &Path, channel: &str) -> PathBuf {
    folder
        .join(".regents")
        .join("channels")
        .join(channel)
        .join("channel.sqlite")
}

/// The user name that the server stamps on human messages. Keep this rule in
/// sync with `local_human_id()` in `src/app/api/channels.rs`.
fn local_user() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "human".to_owned())
}

#[test]
fn a_posted_message_reads_back_with_a_human_author_and_its_date() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    add_workspace(&sandbox, "projeto");

    let posted = sandbox.cli(&["channel", "post", "geral", "oi"]);
    let read = sandbox.cli(&["channel", "read", "geral"]);

    assert_eq!(posted["type"], "channel_message_posted", "{posted}");
    assert_eq!(read["type"], "channel_messages", "{read}");
    assert_eq!(read["channel"], "geral");
    let messages = read["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "{read}");
    let message = &messages[0];
    assert_eq!(message["body"], "oi");
    assert_eq!(message["author"]["kind"], "human");
    assert_eq!(message["author"]["id"], local_user());
    let created_at = message["created_at"].as_str().unwrap();
    assert!(
        created_at.len() == "2026-11-27T18:04:05Z".len()
            && created_at.ends_with('Z')
            && created_at.as_bytes()[10] == b'T',
        "{created_at}"
    );
    assert_eq!(message, &posted["message"]);
}

#[test]
fn the_channel_database_appears_only_with_the_first_message() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    let folder = add_workspace(&sandbox, "projeto");
    let database = channel_database(&folder, "geral");
    assert!(database.parent().unwrap().is_dir());
    assert!(!database.exists());

    let empty = sandbox.cli(&["channel", "read", "geral"]);

    assert_eq!(empty["messages"], serde_json::json!([]));
    assert!(!database.exists(), "reading created the database");

    sandbox.cli(&["channel", "post", "geral", "primeira", "mensagem"]);

    assert!(database.is_file());
    assert_eq!(
        sandbox.cli(&["channel", "read", "geral"])["messages"][0]["body"],
        "primeira mensagem"
    );
}

#[test]
fn a_subscribed_client_receives_the_posted_message() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    add_workspace(&sandbox, "projeto");
    let mut stream = UnixStream::connect(sandbox.api_socket()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": "sub", "method": "events.subscribe",
            "params": {"subscriptions": [{"type": "channel.message_posted"}]}
        })
    )
    .unwrap();
    let mut lines = BufReader::new(stream);
    let mut line = String::new();
    lines.read_line(&mut line).unwrap();
    let started: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(started["result"]["type"], "subscription_started", "{line}");

    let posted = sandbox.cli(&["channel", "post", "geral", "oi"]);

    line.clear();
    lines.read_line(&mut line).unwrap();
    let event: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(event["event"], "channel_message_posted", "{line}");
    assert_eq!(event["data"]["workspace_id"], posted["workspace_id"]);
    assert_eq!(event["data"]["message"], posted["message"]);
}

#[test]
fn messages_survive_a_restart() {
    let sandbox = Sandbox::new();
    let server = sandbox.start_server();
    add_workspace(&sandbox, "projeto");
    sandbox.cli(&["channel", "post", "geral", "antes"]);
    let before = sandbox.cli(&["channel", "read", "geral"])["messages"].clone();
    sandbox.stop_server(server);

    let _server = sandbox.start_server();

    assert_eq!(
        sandbox.cli(&["channel", "read", "geral"])["messages"],
        before
    );
    sandbox.cli(&["channel", "post", "geral", "depois"]);
    let bodies: Vec<Value> = sandbox.cli(&["channel", "read", "geral"])["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["body"].clone())
        .collect();
    assert_eq!(bodies, ["antes", "depois"]);
}

#[test]
fn posting_to_a_channel_that_does_not_exist_is_a_clear_error() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    let folder = add_workspace(&sandbox, "projeto");

    let error = sandbox.cli_error(&["channel", "post", "inexistente", "oi"]);

    assert_eq!(error["code"], "channel_not_found", "{error}");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("#inexistente"), "{message}");
    assert!(!folder.join(".regents/channels/inexistente").exists());
}

#[test]
fn a_disconnected_workspace_refuses_channel_commands() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    let folder = add_workspace(&sandbox, "projeto");
    fs::rename(&folder, folder.with_file_name("movido")).unwrap();

    let error = sandbox.cli_error(&["channel", "post", "geral", "oi"]);

    assert_eq!(error["code"], "workspace_disconnected", "{error}");
}

#[test]
fn a_command_inside_a_workspace_folder_posts_to_that_workspace() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    let first = add_workspace(&sandbox, "primeiro");
    let second = sandbox.folder("segundo");
    sandbox.cli(&["workspace", "add", "--focus", second.to_str().unwrap()]);
    let inside_first = first.join("src").join("modulo");
    fs::create_dir_all(&inside_first).unwrap();

    let posted = sandbox.cli_in(&inside_first, &["channel", "post", "geral", "daqui"]);
    let from_outside = sandbox.cli(&["channel", "post", "geral", "ativo"]);

    let first_messages = sandbox.cli_in(&first, &["channel", "read", "geral"])["messages"].clone();
    assert_eq!(
        first_messages.as_array().unwrap().len(),
        1,
        "{first_messages}"
    );
    assert_eq!(first_messages[0]["body"], "daqui");
    assert_ne!(posted["workspace_id"], from_outside["workspace_id"]);
    let second_messages =
        sandbox.cli_in(&second, &["channel", "read", "geral"])["messages"].clone();
    assert_eq!(
        second_messages[0]["body"], "ativo",
        "the active workspace is the fallback"
    );
}
