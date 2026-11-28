//! Regents thread runtime (ADR-0001, ADR-0004). A mention opens a thread: the
//! agent CLI runs in a background pane of the workspace, outside every tab,
//! so it keeps output parsing and agent detection but never causes
//! presentation work. The thread registry in the workspace folder records
//! it. When the agent finishes its turn, the CLI process ends and the thread
//! keeps its CLI session.

use std::path::{Path, PathBuf};

use crate::app::actions::PaneStateUpdate;
use crate::app::App;
use crate::layout::PaneId;
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::regents::adapter;
use crate::regents::agents::AgentDefinition;
use crate::regents::channel::{Channel, Message, ReadRange};
use crate::regents::threads::{
    self, NewThread, ThreadOrigin, ThreadProgress, ThreadState, ThreadStep,
};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

use crate::regents::{THREAD_ID_ENV_VAR, THREAD_TOKEN_ENV_VAR};

/// Environment of a thread CLI: the stable workspace ID, next to
/// `THREAD_ID_ENV_VAR`.
const WORKSPACE_ID_ENV_VAR: &str = "REGENTS_WORKSPACE_ID";

const PANE_CLOSED: &str = "its pane closed";
const SERVER_HANDOFF: &str = "the server handed off to a new process";

/// A thread whose agent CLI runs.
#[derive(Debug)]
pub(crate) struct RunningThread {
    /// `.regents` of the workspace that owns the thread registry.
    pub(crate) state_dir: PathBuf,
    pub(crate) thread_id: String,
    /// Agent name: the author of the messages that the thread posts.
    pub(crate) agent: String,
    /// Credential in `THREAD_TOKEN_ENV_VAR`. Only memory keeps it: a thread
    /// does not survive a server restart or handoff.
    pub(crate) token: String,
    terminal_id: TerminalId,
    progress: ThreadProgress,
}

/// The first prompt of a thread opened by `message`: the mention and the
/// `adapter::RECENT_MESSAGES` most recent messages of the channel.
pub(crate) fn mention_prompt(channel: &Channel, message: &Message) -> String {
    let recent = match channel.read(ReadRange {
        before: Some(message.id.saturating_add(1)),
        limit: Some(adapter::RECENT_MESSAGES),
    }) {
        Ok(recent) => recent,
        Err(err) => {
            tracing::warn!(%err, "cannot read the channel for a thread prompt");
            vec![message.clone()]
        }
    };
    adapter::initial_prompt(message, &recent)
}

impl App {
    /// Opens one thread for each agent that `message` mentions. A thread
    /// that cannot open is logged; the message stays posted. A message posted
    /// from a thread opens none: delegation, with its opener and depth
    /// limit, is #18.
    pub(crate) fn open_mention_threads(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        channel: &Channel,
        message: &Message,
        agents: &[AgentDefinition],
    ) {
        if message.mentions.is_empty() || message.origin_thread_id.is_some() {
            return;
        }
        let prompt = mention_prompt(channel, message);
        for name in &message.mentions {
            let Some(agent) = agents.iter().find(|agent| &agent.name == name) else {
                continue;
            };
            if let Err(err) = self.open_thread(ws_idx, state_dir, agent, &prompt, message) {
                tracing::warn!(agent = %agent.name, %err, "cannot open a thread");
            }
        }
    }

    fn open_thread(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        agent: &AgentDefinition,
        prompt: &str,
        origin: &Message,
    ) -> Result<PaneId, String> {
        let session_id = adapter::new_session_id();
        let argv = adapter::launch_argv(agent, prompt, &session_id)
            .ok_or_else(|| format!("the {} CLI has no thread adapter yet", agent.cli.as_str()))?;
        let thread = NewThread {
            agent: agent.name.clone(),
            cli: agent.cli,
            origin: ThreadOrigin::Channel,
            channel: Some(origin.channel.clone()),
            origin_message_id: Some(origin.id),
            opener_thread_id: None,
            depth: 0,
            // Worktree isolation is #24: every thread runs in the folder.
            isolation: crate::regents::agents::Isolation::Shared,
            session_id: Some(session_id),
        };
        self.launch_thread(ws_idx, state_dir, thread, &argv)
    }

    /// Records `thread` and starts `argv` for it in a background pane.
    fn launch_thread(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        thread: NewThread,
        argv: &[String],
    ) -> Result<PaneId, String> {
        let registry = threads::registry(state_dir);
        let thread = registry
            .create(thread, time::OffsetDateTime::now_utc())
            .map_err(|err| err.to_string())?;
        let folder = state_dir.parent().unwrap_or(state_dir).to_path_buf();
        let workspace_stable_id = self.state.workspaces[ws_idx]
            .folder
            .as_ref()
            .map(|binding| binding.id.clone())
            .unwrap_or_default();
        let token: String = crate::regents::random_bytes(b"thread-token")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let env = vec![
            (THREAD_ID_ENV_VAR.to_owned(), thread.id.clone()),
            (THREAD_TOKEN_ENV_VAR.to_owned(), token.clone()),
            (WORKSPACE_ID_ENV_VAR.to_owned(), workspace_stable_id),
        ];
        match self.spawn_background_pane(ws_idx, folder, argv, env) {
            Ok((pane_id, terminal_id)) => {
                self.running_threads.insert(
                    pane_id,
                    RunningThread {
                        state_dir: state_dir.to_path_buf(),
                        thread_id: thread.id,
                        agent: thread.agent,
                        token,
                        terminal_id,
                        progress: ThreadProgress::default(),
                    },
                );
                Ok(pane_id)
            }
            Err(err) => {
                let reason = format!("cannot start {}: {err}", argv[0]);
                record_state(&registry, &thread.id, ThreadState::Failed, Some(&reason));
                Err(reason)
            }
        }
    }

    /// Starts `argv` in a new background pane of the workspace: a pane with
    /// agent detection and a pane identity, in no tab.
    fn spawn_background_pane(
        &mut self,
        ws_idx: usize,
        cwd: PathBuf,
        argv: &[String],
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(PaneId, TerminalId)> {
        let workspace = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("the workspace disappeared"))?;
        let reserved = workspace.reserve_background_pane_number();
        let launch_env = PaneLaunchEnv::from_extra(extra_env)
            .with_background_identity(workspace.id.clone(), reserved.public_id.clone());
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let (cols, rows) = self.state.headless_size;
        let runtime = TerminalRuntime::spawn_argv_command(
            pane_id,
            rows.max(4),
            cols.max(4),
            cwd.clone(),
            argv,
            &launch_env,
            crate::pane::AgentDetection::Enabled,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.pane_host_terminal_appearance(),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), cwd).with_launch_argv(argv.to_vec()),
        );
        self.state.workspaces[ws_idx].insert_background_pane(
            reserved,
            pane_id,
            PaneState::new(terminal_id.clone()),
        );
        Ok((pane_id, terminal_id))
    }

    /// Advances the running threads whose agent state changed. A thread whose
    /// agent finished its turn is done.
    pub(crate) fn observe_thread_states(&mut self, updates: &[PaneStateUpdate]) {
        if self.running_threads.is_empty() {
            return;
        }
        for update in updates {
            let Some(thread) = self.running_threads.get_mut(&update.pane_id) else {
                continue;
            };
            if thread.progress.observe(update.state) == ThreadStep::Done {
                self.end_thread(update.pane_id, ThreadState::Done, None);
            }
        }
    }

    /// Handles the exit of a background pane process. True when the pane was
    /// a thread: a running thread ends, and the exit of an ended thread is
    /// expected.
    pub(crate) fn end_exited_thread(&mut self, pane_id: PaneId) -> bool {
        if self.expected_thread_exits.remove(&pane_id) {
            return true;
        }
        let Some(thread) = self.running_threads.get(&pane_id) else {
            return false;
        };
        let (state, reason) = thread.progress.state_after_exit();
        self.end_thread(pane_id, state, reason);
        // The process already exited: no other exit event follows.
        self.expected_thread_exits.remove(&pane_id);
        true
    }

    /// `pane.close` on a thread pane: the thread ends as failed. False when
    /// the pane is not a running thread.
    pub(crate) fn close_thread_pane(&mut self, pane_id: PaneId) -> bool {
        if !self.running_threads.contains_key(&pane_id) {
            return false;
        }
        self.end_thread(pane_id, ThreadState::Failed, Some(PANE_CLOSED));
        true
    }

    /// Ends the threads whose terminals are about to stop because their pane
    /// or workspace closed. Call it before the runtimes shut down.
    pub(crate) fn end_threads_of_detached_terminals(&mut self, terminal_ids: &[TerminalId]) {
        if self.running_threads.is_empty() || terminal_ids.is_empty() {
            return;
        }
        let ended: Vec<PaneId> = self
            .running_threads
            .iter()
            .filter(|(_, thread)| terminal_ids.contains(&thread.terminal_id))
            .map(|(&pane_id, _)| pane_id)
            .collect();
        for pane_id in ended {
            self.end_thread(pane_id, ThreadState::Failed, Some(PANE_CLOSED));
        }
    }

    /// Marks every running thread interrupted, for a live handoff that does
    /// not carry thread panes (#19). Their runtimes stop with the old server.
    pub(crate) fn interrupt_running_threads(&mut self) {
        for (_, thread) in std::mem::take(&mut self.running_threads) {
            record_state(
                &threads::registry(&thread.state_dir),
                &thread.thread_id,
                ThreadState::Interrupted,
                Some(SERVER_HANDOFF),
            );
        }
    }

    /// Records the final state, removes the background pane and stops its
    /// CLI process. The CLI session stays in the registry.
    fn end_thread(&mut self, pane_id: PaneId, state: ThreadState, reason: Option<&str>) {
        let Some(thread) = self.running_threads.remove(&pane_id) else {
            return;
        };
        record_state(
            &threads::registry(&thread.state_dir),
            &thread.thread_id,
            state,
            reason,
        );
        for workspace in &mut self.state.workspaces {
            workspace.remove_background_pane(pane_id);
        }
        self.state.terminals.remove(&thread.terminal_id);
        self.state.pending_agent_notifications.remove(&pane_id);
        if self.terminal_runtimes.get(&thread.terminal_id).is_some() {
            self.expected_thread_exits.insert(pane_id);
            self.shutdown_terminal_runtime(thread.terminal_id);
        }
    }

    /// Agent of the running thread `thread_id` of the workspace whose state
    /// directory is `state_dir`, when `token` is its credential.
    pub(crate) fn thread_agent(
        &self,
        state_dir: &Path,
        thread_id: &str,
        token: &str,
    ) -> Option<&str> {
        self.running_threads
            .values()
            .find(|thread| {
                thread.thread_id == thread_id
                    && thread.state_dir == state_dir
                    && thread.token == token
            })
            .map(|thread| thread.agent.as_str())
    }

    /// Public pane ID of the running thread `thread_id` of the workspace
    /// whose state directory is `state_dir`.
    pub(crate) fn running_thread_pane_id(
        &self,
        state_dir: &Path,
        thread_id: &str,
    ) -> Option<String> {
        let (pane_id, _) = self
            .running_threads
            .iter()
            .find(|(_, thread)| thread.thread_id == thread_id && thread.state_dir == state_dir)?;
        let (ws_idx, _) = self.find_pane(*pane_id)?;
        self.public_pane_id(ws_idx, *pane_id)
    }
}

fn record_state(
    registry: &threads::ThreadRegistry,
    thread_id: &str,
    state: ThreadState,
    reason: Option<&str>,
) {
    if let Err(err) = registry.set_state(thread_id, state, reason, time::OffsetDateTime::now_utc())
    {
        tracing::warn!(thread = %thread_id, %err, "cannot record the thread state");
    }
}

#[cfg(test)]
mod tests {
    use super::super::api::test_support::shutdown_test_runtimes;
    use super::*;
    use crate::regents::agents::{AgentCli, Isolation};
    use crate::regents::channel::{Author, AuthorKind, NewMessage};
    use crate::regents::store::TempFolder;
    use serde_json::{json, Value};

    struct Fixture {
        app: App,
        base: TempFolder,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let base = TempFolder::new(&format!("app-threads-{name}"));
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                crate::app::AppPolicy::TEST,
                None,
                api_rx,
                crate::api::EventHub::default(),
            );
            app.state.default_shell =
                super::super::api::test_support::exiting_test_command().into();
            app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
            app.workspace_registry_path = Some(base.0.join("config").join("workspaces.toml"));
            Self { app, base }
        }

        /// Adds a folder workspace and returns its index and state directory.
        fn workspace(&mut self, name: &str) -> (usize, PathBuf) {
            let path = self.base.0.join(name);
            std::fs::create_dir_all(&path).unwrap();
            let path = crate::platform::canonical_path(&path).unwrap();
            let added = self.request(
                "workspace.add",
                json!({"path": path.display().to_string(), "focus": true}),
            );
            assert_eq!(added["result"]["type"], "workspace_added", "{added}");
            let index = self.app.state.active.unwrap();
            (index, path.join(".regents"))
        }

        /// A running thread whose CLI is a long `sleep`.
        fn thread(&mut self, ws_idx: usize, state_dir: &Path) -> PaneId {
            let argv = ["sleep".to_owned(), "30".to_owned()];
            self.app
                .launch_thread(
                    ws_idx,
                    state_dir,
                    NewThread {
                        agent: "chief".into(),
                        cli: AgentCli::Claude,
                        origin: ThreadOrigin::Channel,
                        channel: Some("geral".into()),
                        origin_message_id: Some(1),
                        opener_thread_id: None,
                        depth: 0,
                        isolation: Isolation::Shared,
                        session_id: Some("sid".into()),
                    },
                    &argv,
                )
                .unwrap()
        }

        fn request(&mut self, method: &str, params: Value) -> Value {
            let request = serde_json::from_value(json!({
                "id": "test", "method": method, "params": params
            }))
            .unwrap();
            serde_json::from_str(&self.app.handle_api_request(request)).unwrap()
        }

        fn public_pane_id(&self, pane_id: PaneId) -> String {
            let (ws_idx, _) = self.app.find_pane(pane_id).unwrap();
            self.app.public_pane_id(ws_idx, pane_id).unwrap()
        }

        fn records(state_dir: &Path) -> Vec<threads::ThreadRecord> {
            threads::registry(state_dir).list().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown_test_runtimes(&mut self.app);
        }
    }

    #[tokio::test]
    async fn closing_the_workspace_ends_its_threads() {
        let mut fixture = Fixture::new("close-ws");
        let (ws_idx, state_dir) = fixture.workspace("projeto");
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let terminal_id = fixture.app.running_threads[&pane_id].terminal_id.clone();
        let workspace_id = fixture.app.public_workspace_id(ws_idx);

        let closed = fixture.request("workspace.close", json!({"workspace_id": workspace_id}));

        assert!(closed.get("error").is_none(), "{closed}");
        assert!(fixture.app.running_threads.is_empty());
        assert!(fixture.app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(!fixture.app.state.terminals.contains_key(&terminal_id));
        let records = Fixture::records(&state_dir);
        assert_eq!(records[0].state, ThreadState::Failed);
        assert_eq!(records[0].end_reason.as_deref(), Some(PANE_CLOSED));
        assert!(
            fixture.app.end_exited_thread(pane_id),
            "the exit is expected"
        );
    }

    #[tokio::test]
    async fn pane_close_on_a_thread_pane_ends_the_thread() {
        let mut fixture = Fixture::new("pane-close");
        let (ws_idx, state_dir) = fixture.workspace("projeto");
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let terminal_id = fixture.app.running_threads[&pane_id].terminal_id.clone();
        let public_pane_id = fixture.public_pane_id(pane_id);

        let closed = fixture.request("pane.close", json!({"pane_id": public_pane_id}));

        assert!(closed.get("error").is_none(), "{closed}");
        assert!(fixture.app.running_threads.is_empty());
        assert!(fixture.app.find_pane(pane_id).is_none());
        assert!(fixture.app.state.workspaces[ws_idx]
            .background_panes
            .is_empty());
        assert!(fixture.app.terminal_runtimes.get(&terminal_id).is_none());
        fixture.app.state.assert_invariants_for_test();
        let records = Fixture::records(&state_dir);
        assert_eq!(records[0].state, ThreadState::Failed);
        assert_eq!(records[0].end_reason.as_deref(), Some(PANE_CLOSED));
    }

    #[tokio::test]
    async fn agent_lookups_find_a_thread_pane_but_focus_refuses_it() {
        let mut fixture = Fixture::new("agent");
        let (ws_idx, state_dir) = fixture.workspace("projeto");
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let public_pane_id = fixture.public_pane_id(pane_id);
        let reported = fixture.request(
            "pane.report_agent",
            json!({"pane_id": public_pane_id, "source": "custom:test", "agent": "claude",
                   "state": "working"}),
        );
        assert!(reported.get("error").is_none(), "{reported}");

        let got = fixture.request("agent.get", json!({"target": public_pane_id}));
        let read = fixture.request(
            "agent.read",
            json!({"target": public_pane_id, "source": "recent", "format": "text"}),
        );
        let focus = fixture.request("agent.focus", json!({"target": public_pane_id}));

        assert_eq!(got["result"]["agent"]["pane_id"], public_pane_id, "{got}");
        assert_eq!(got["result"]["agent"]["tab_id"], "", "{got}");
        assert_eq!(read["result"]["read"]["pane_id"], public_pane_id, "{read}");
        assert_eq!(focus["error"]["code"], "agent_not_in_tab", "{focus}");
    }

    #[test]
    fn the_prompt_carries_the_mention_and_the_twenty_messages_before_it() {
        let folder = TempFolder::new("app-threads-prompt");
        std::fs::create_dir_all(folder.0.join("channels").join("geral")).unwrap();
        let channel = crate::regents::channel::open(&folder.0, "geral").unwrap();
        let post = |body: String| {
            channel
                .post(
                    NewMessage {
                        author: Author {
                            kind: AuthorKind::Human,
                            id: "rafael".into(),
                        },
                        body,
                        reply_to: None,
                        origin_thread_id: None,
                        mentions: Vec::new(),
                    },
                    time::OffsetDateTime::now_utc(),
                )
                .unwrap()
        };
        for number in 1..=21 {
            post(format!("msg-{number:02}"));
        }
        let mention = post("@chief olá".into());
        post("depois da menção".into());

        let prompt = mention_prompt(&channel, &mention);

        assert_eq!(adapter::RECENT_MESSAGES, 20);
        // The mention is the newest of the 20 messages, so 19 come before it.
        for kept in 3..=21 {
            assert!(prompt.contains(&format!("msg-{kept:02}")), "{prompt}");
        }
        assert!(
            !prompt.contains("msg-02"),
            "the 21st message is in: {prompt}"
        );
        assert!(!prompt.contains("msg-01"), "{prompt}");
        assert!(!prompt.contains("depois da menção"), "{prompt}");
        assert!(prompt.contains("@chief olá"));
    }
}
