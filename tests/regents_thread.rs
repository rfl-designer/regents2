//! Mentions open hidden threads (ticket #9): a `@agent` mention in a human
//! message launches the agent CLI outside every tab, the thread registry of
//! the workspace records it, and the CLI process ends when the thread is
//! done. The tests put a fake `claude` on the server `PATH`.

#![cfg(unix)]

mod regents_support;
pub mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regents_support::{wait_until, Sandbox, REGENTS_BIN};
use serde_json::Value;

/// A fake `claude` CLI. It records its arguments (NUL separated) and its
/// PID, reports `working`, posts the contents of the file `post` to `#geral`
/// when that file exists, waits for the file `go`, reports `idle` and then
/// stays alive, as an interactive CLI does.
struct FakeClaude {
    bin: PathBuf,
    out: PathBuf,
}

impl FakeClaude {
    fn install(sandbox: &Sandbox) -> Self {
        let bin = sandbox.base.join("bin");
        let out = sandbox.base.join("fake");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&out).unwrap();
        let script = format!(
            r#"#!/bin/sh
out='{out}'
for arg in "$@"; do printf '%s\0' "$arg"; done > "$out/args.tmp"
mv "$out/args.tmp" "$out/args"
printf '%s' "$$" > "$out/pid"
'{regents}' pane report-agent "$HERDR_PANE_ID" --source custom:fake-claude --agent claude --state working > /dev/null
if [ -f "$out/post" ]; then
  '{regents}' channel post geral "$(cat "$out/post")" > "$out/posted"
fi
while [ ! -f "$out/go" ]; do sleep 0.05; done
'{regents}' pane report-agent "$HERDR_PANE_ID" --source custom:fake-claude --agent claude --state idle > /dev/null
exec sleep 600
"#,
            out = out.display(),
            regents = REGENTS_BIN,
        );
        let path = bin.join("claude");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Self { bin, out }
    }

    fn path_env(&self) -> (&'static str, String) {
        let inherited = std::env::var("PATH").unwrap_or_default();
        ("PATH", format!("{}:{inherited}", self.bin.display()))
    }

    /// The arguments of the last launch, after it started.
    fn wait_for_args(&self) -> Vec<String> {
        let file = self.out.join("args");
        assert!(
            wait_until(Duration::from_secs(10), || file.is_file()),
            "the fake claude did not start"
        );
        String::from_utf8(fs::read(file).unwrap())
            .unwrap()
            .split_terminator('\0')
            .map(str::to_owned)
            .collect()
    }

    fn pid(&self) -> i32 {
        fs::read_to_string(self.out.join("pid"))
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Makes the next launch post `body` to `#geral` from inside its thread.
    fn post_from_thread(&self, body: &str) {
        fs::write(self.out.join("post"), body).unwrap();
    }

    /// The JSON result of the post from inside the thread.
    fn wait_for_thread_post(&self) -> Value {
        let file = self.out.join("posted");
        assert!(
            wait_until(Duration::from_secs(10), || fs::read(&file)
                .is_ok_and(|bytes| bytes.ends_with(b"\n"))),
            "the fake claude did not post"
        );
        let response: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
        response["result"].clone()
    }

    /// Lets the fake finish its turn.
    fn finish_turn(&self) {
        fs::write(self.out.join("go"), "").unwrap();
    }
}

fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn add_workspace(sandbox: &Sandbox, name: &str) -> PathBuf {
    let folder = sandbox.folder(name);
    let added = sandbox.cli(&["workspace", "add", folder.to_str().unwrap()]);
    assert_eq!(added["type"], "workspace_added", "{added}");
    folder
}

fn threads(sandbox: &Sandbox) -> Vec<Value> {
    let listed = sandbox.cli(&["thread", "list"]);
    assert_eq!(listed["type"], "thread_list", "{listed}");
    listed["threads"].as_array().unwrap().clone()
}

fn wait_for_thread_state(sandbox: &Sandbox, state: &str) -> Vec<Value> {
    let mut last = Vec::new();
    let reached = wait_until(Duration::from_secs(10), || {
        last = threads(sandbox);
        last.first().is_some_and(|thread| thread["state"] == state)
    });
    assert!(reached, "the thread did not reach {state}: {last:?}");
    last
}

/// The value that follows `flag` in `args`.
fn flag_value<'a>(args: &'a [String], flag: &str) -> &'a str {
    let index = args
        .iter()
        .position(|arg| arg == flag)
        .unwrap_or_else(|| panic!("no {flag} in {args:?}"));
    &args[index + 1]
}

fn chief_role(folder: &Path) -> String {
    let definition = fs::read_to_string(folder.join(".regents/agents/chief-of-staff.md")).unwrap();
    let body = definition.splitn(3, "+++").nth(2).unwrap();
    body.trim().to_owned()
}

#[test]
fn a_mention_opens_a_hidden_thread_that_ends_done_with_its_session() {
    let sandbox = Sandbox::new();
    let fake = FakeClaude::install(&sandbox);
    let _server = sandbox.start_server_with_env(&[fake.path_env()]);
    let folder = add_workspace(&sandbox, "projeto");
    sandbox.cli(&["channel", "post", "geral", "contexto", "anterior"]);
    sandbox.cli(&["channel", "post", "geral", "outra", "mensagem"]);

    let posted = sandbox.cli(&["channel", "post", "geral", "@chief", "olá"]);

    assert_eq!(posted["message"]["mentions"], serde_json::json!(["chief"]));
    let active = threads(&sandbox);
    assert_eq!(active.len(), 1, "{active:?}");
    let thread = &active[0];
    assert_eq!(thread["agent"], "chief");
    assert_eq!(thread["state"], "active");
    assert_eq!(thread["channel"], "geral");
    assert_eq!(thread["origin_message_id"], posted["message"]["id"]);

    let args = fake.wait_for_args();
    assert_eq!(
        flag_value(&args, "--append-system-prompt"),
        chief_role(&folder)
    );
    let prompt = args.last().unwrap();
    for body in ["@chief olá", "contexto anterior", "outra mensagem"] {
        assert!(prompt.contains(body), "prompt without {body:?}: {prompt}");
    }
    let session = flag_value(&args, "--session-id").to_owned();

    // The thread pane is in no tab: the workspace still shows only its root pane.
    let panes = sandbox.api("pane.list", serde_json::json!({}))["panes"].clone();
    assert_eq!(panes.as_array().unwrap().len(), 1, "{panes}");
    let pid = fake.pid();
    assert!(process_exists(pid));

    fake.finish_turn();

    let done = wait_for_thread_state(&sandbox, "done");
    assert_eq!(done[0]["session_id"], session.as_str(), "{done:?}");
    assert!(
        wait_until(Duration::from_secs(10), || !process_exists(pid)),
        "the CLI process {pid} is still running after done"
    );
}

#[test]
fn a_mention_of_an_unknown_agent_is_ignored() {
    let sandbox = Sandbox::new();
    let fake = FakeClaude::install(&sandbox);
    let _server = sandbox.start_server_with_env(&[fake.path_env()]);
    add_workspace(&sandbox, "projeto");

    let posted = sandbox.cli(&["channel", "post", "geral", "@ninguem", "oi"]);

    assert_eq!(posted["type"], "channel_message_posted", "{posted}");
    assert_eq!(posted["message"].get("mentions"), None, "{posted}");
    assert_eq!(threads(&sandbox), Vec::<Value>::new());
}

#[test]
fn agent_list_shows_the_agents_of_the_workspace() {
    let sandbox = Sandbox::new();
    let _server = sandbox.start_server();
    let folder = add_workspace(&sandbox, "projeto");
    fs::write(
        folder.join(".regents/agents/reviewer.md"),
        "+++\nname = \"reviewer\"\ncli = \"codex\"\nmodel = \"gpt-5\"\n+++\nVocê revisa código.\n",
    )
    .unwrap();

    let listed = sandbox.cli(&["agent", "list"]);

    assert_eq!(listed["type"], "agent_definition_list", "{listed}");
    let agents = listed["agents"].as_array().unwrap();
    let summary: Vec<(Value, Value, Value)> = agents
        .iter()
        .map(|agent| {
            (
                agent["name"].clone(),
                agent["cli"].clone(),
                agent["reserved"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("chief".into(), "claude".into(), true.into()),
            ("reviewer".into(), "codex".into(), false.into()),
        ]
    );
    assert_eq!(agents[1]["model"], "gpt-5");
}

#[test]
fn the_thread_registry_survives_a_restart() {
    let sandbox = Sandbox::new();
    let fake = FakeClaude::install(&sandbox);
    let server = sandbox.start_server_with_env(&[fake.path_env()]);
    add_workspace(&sandbox, "projeto");
    sandbox.cli(&["channel", "post", "geral", "@chief", "olá"]);
    fake.wait_for_args();
    fake.finish_turn();
    let before = wait_for_thread_state(&sandbox, "done");
    sandbox.stop_server(server);

    let _server = sandbox.start_server_with_env(&[fake.path_env()]);

    assert_eq!(threads(&sandbox), before);
}

#[test]
fn a_mention_posted_from_a_thread_opens_no_thread() {
    let sandbox = Sandbox::new();
    let fake = FakeClaude::install(&sandbox);
    fake.post_from_thread("@chief pode continuar");
    let _server = sandbox.start_server_with_env(&[fake.path_env()]);
    add_workspace(&sandbox, "projeto");

    sandbox.cli(&["channel", "post", "geral", "@chief", "olá"]);
    let from_thread = fake.wait_for_thread_post();

    let thread_id = threads(&sandbox)[0]["id"].clone();
    assert_eq!(
        from_thread["message"]["origin_thread_id"], thread_id,
        "{from_thread}"
    );
    assert_eq!(
        from_thread["message"]["mentions"],
        serde_json::json!(["chief"])
    );
    fake.finish_turn();
    let after = wait_for_thread_state(&sandbox, "done");
    assert_eq!(
        after.len(),
        1,
        "a post from a thread opened a thread: {after:?}"
    );
}
