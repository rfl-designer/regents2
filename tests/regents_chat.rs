//! The chat pane of the `regents` binary: `channel open` puts a channel in
//! the layout as a pane that runs the chat view, the pane follows the
//! channel live, sends the typed message as a human and comes back in the
//! same layout place after a restart (ticket #7).

#![cfg(unix)]

mod regents_support;
pub mod support;

use std::path::PathBuf;
use std::time::Duration;

use regents_support::{wait_until, Sandbox};
use serde_json::{json, Value};

/// Adds `name` as a workspace and returns its folder.
fn add_workspace(sandbox: &Sandbox, name: &str) -> PathBuf {
    let folder = sandbox.folder(name);
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    assert_eq!(added["type"], "workspace_added", "{added}");
    folder
}

fn visible_text(sandbox: &Sandbox, pane_id: &str) -> String {
    sandbox.api(
        "pane.read",
        json!({"pane_id": pane_id, "source": "visible"}),
    )["read"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Waits until the visible screen of `pane_id` contains `needle`.
fn wait_for_screen(sandbox: &Sandbox, pane_id: &str, needle: &str) {
    let mut last = String::new();
    let found = wait_until(Duration::from_secs(10), || {
        last = visible_text(sandbox, pane_id);
        last.contains(needle)
    });
    assert!(found, "{needle:?} never appeared in {pane_id}:\n{last}");
}

/// Opens `#geral` as a chat pane and waits for the chat view to draw.
fn open_geral(sandbox: &Sandbox) -> Value {
    let opened = sandbox.cli(&["channel", "open", "geral"]);
    assert_eq!(opened["type"], "channel_opened", "{opened}");
    let pane_id = opened["pane"]["pane_id"].as_str().unwrap().to_owned();
    wait_for_screen(sandbox, &pane_id, "#geral");
    opened
}

fn layout(sandbox: &Sandbox, pane_id: &str) -> Value {
    sandbox.api("pane.layout", json!({"pane_id": pane_id}))["layout"].clone()
}

#[test]
fn a_second_open_focuses_the_chat_pane_without_a_new_pane() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    add_workspace(&sandbox, "projeto");
    let first = open_geral(&sandbox);
    let chat_pane = first["pane"]["pane_id"].as_str().unwrap();
    let other_pane = sandbox.api("pane.list", json!({}))["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["pane_id"].as_str().unwrap().to_owned())
        .find(|pane_id| pane_id != chat_pane)
        .unwrap();
    sandbox.api("pane.focus", json!({"pane_id": other_pane}));

    let second = sandbox.cli(&["channel", "open", "geral"]);

    assert_eq!(second["opened"], false, "{second}");
    assert_eq!(second["pane"]["pane_id"], chat_pane);
    assert_eq!(second["pane"]["focused"], true);
    assert_eq!(
        sandbox.api("pane.list", json!({}))["panes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_message_posted_by_the_cli_appears_in_the_open_chat_pane() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    add_workspace(&sandbox, "projeto");
    sandbox.cli(&["channel", "post", "geral", "antes da abertura"]);
    let opened = open_geral(&sandbox);
    let pane_id = opened["pane"]["pane_id"].as_str().unwrap();
    wait_for_screen(&sandbox, pane_id, "antes da abertura");

    sandbox.cli(&["channel", "post", "geral", "chegou ao vivo"]);

    wait_for_screen(&sandbox, pane_id, "chegou ao vivo");
    let screen = visible_text(&sandbox, pane_id);
    let line = screen
        .lines()
        .find(|line| line.contains("chegou ao vivo"))
        .unwrap();
    let author = sandbox.cli(&["channel", "read", "geral"])["messages"][1]["author"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(line.contains(&format!("{author}: ")), "{screen}");
}

#[test]
fn a_message_sent_from_the_chat_pane_is_posted_with_a_human_author() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    add_workspace(&sandbox, "projeto");
    let opened = open_geral(&sandbox);
    let pane_id = opened["pane"]["pane_id"].as_str().unwrap();

    sandbox.api(
        "pane.send_text",
        json!({"pane_id": pane_id, "text": "olá do pane"}),
    );
    wait_for_screen(&sandbox, pane_id, "> olá do pane");
    sandbox.api(
        "pane.send_keys",
        json!({"pane_id": pane_id, "keys": ["Enter"]}),
    );

    let mut messages = Value::Null;
    assert!(
        wait_until(Duration::from_secs(10), || {
            messages = sandbox.cli(&["channel", "read", "geral"])["messages"].clone();
            messages
                .as_array()
                .is_some_and(|messages| !messages.is_empty())
        }),
        "the message never reached the channel"
    );
    assert_eq!(messages[0]["body"], "olá do pane", "{messages}");
    assert_eq!(messages[0]["author"]["kind"], "human", "{messages}");
    wait_for_screen(&sandbox, pane_id, ": olá do pane");
}

#[test]
fn the_chat_pane_comes_back_in_the_same_layout_place_after_a_restart() {
    let sandbox = Sandbox::new();
    let server = sandbox.start_server();
    let folder = add_workspace(&sandbox, "projeto");
    let opened = open_geral(&sandbox);
    let pane_id = opened["pane"]["pane_id"].as_str().unwrap().to_owned();
    let channel = opened["pane"]["channel"].clone();
    let folder_id = sandbox.open_workspaces()[0]["folder"]["id"].clone();
    assert_eq!(channel, json!({"folder_id": folder_id, "channel": "geral"}));
    let before = layout(&sandbox, &pane_id);
    sandbox.cli(&["channel", "post", "geral", "guardada"]);
    sandbox.stop_server(server);

    let _server = sandbox.start_server();

    let pane = sandbox.api("pane.get", json!({"pane_id": pane_id}))["pane"].clone();
    assert_eq!(pane["channel"], channel, "{pane}");
    assert_eq!(layout(&sandbox, &pane_id), before);
    wait_for_screen(&sandbox, &pane_id, "#geral");
    wait_for_screen(&sandbox, &pane_id, "guardada");
    assert!(folder.join(".regents/channels/geral").is_dir());
}

#[test]
fn a_chat_pane_whose_channel_is_gone_keeps_its_place_and_recovers() {
    let sandbox = Sandbox::new();
    let server = sandbox.start_server();
    let folder = add_workspace(&sandbox, "projeto");
    let opened = open_geral(&sandbox);
    let pane_id = opened["pane"]["pane_id"].as_str().unwrap().to_owned();
    sandbox.cli(&["channel", "post", "geral", "ainda aqui"]);
    let before = layout(&sandbox, &pane_id);
    sandbox.stop_server(server);
    let channels = folder.join(".regents").join("channels");
    std::fs::rename(channels.join("geral"), channels.join("guardado")).unwrap();

    let _server = sandbox.start_server();

    wait_for_screen(&sandbox, &pane_id, "retrying");
    assert_eq!(layout(&sandbox, &pane_id), before);
    std::fs::rename(channels.join("guardado"), channels.join("geral")).unwrap();
    wait_for_screen(&sandbox, &pane_id, "ainda aqui");
    assert!(!visible_text(&sandbox, &pane_id).contains("retrying"));
}
