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
use crate::regents::agents::{AgentCli, AgentDefinition};
use crate::regents::channel::{self, Capture, Channel, Message, ReadRange};
use crate::regents::threads::{
    self, NewThread, ThreadOrigin, ThreadProgress, ThreadState, ThreadStep,
};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

use crate::regents::{THREAD_ID_ENV_VAR, THREAD_TOKEN_ENV_VAR};

/// Environment of a thread CLI: the stable workspace ID, next to
/// `THREAD_ID_ENV_VAR`.
const WORKSPACE_ID_ENV_VAR: &str = "REGENTS_WORKSPACE_ID";

const PANE_CLOSED: &str = "its pane closed";
/// Newest threads of each workspace in the thread index.
/// shortcut: older threads are not in the index; page through `thread.list`
/// when the index must show the whole history.
pub(crate) const INDEX_THREADS: usize = 50;
/// Lines of the last output that an ended thread keeps.
const OUTPUT_LINES: usize = 1000;
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
    cli: Option<AgentCli>,
    session_id: Option<String>,
    /// Channel and message of the mention that opened the thread.
    channel: Option<String>,
    origin_message_id: Option<u64>,
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
        let title = Some(format!("@{} · {}", thread.agent, thread.id));
        let launched = self.spawn_background_pane(ws_idx, folder, argv, env, title);
        self.refresh_folder_threads();
        match launched {
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
                        cli: thread.cli,
                        session_id: thread.session_id,
                        channel: thread.channel,
                        origin_message_id: thread.origin_message_id,
                    },
                );
                Ok(pane_id)
            }
            Err(err) => {
                let reason = format!("cannot start {}: {err}", argv[0]);
                record_state(&registry, &thread.id, ThreadState::Failed, Some(&reason));
                self.refresh_folder_threads();
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
        title: Option<String>,
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
        let mut terminal =
            TerminalState::new(terminal_id.clone(), cwd).with_launch_argv(argv.to_vec());
        terminal.manual_label = title;
        self.state.terminals.insert(terminal_id.clone(), terminal);
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
            // The thread index shows the agent state of a hidden pane.
            self.render_dirty.request_generic();
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
        self.refresh_folder_threads();
    }

    /// Records the final state, removes the background pane and stops its
    /// CLI process. The CLI session stays in the registry.
    fn end_thread(&mut self, pane_id: PaneId, state: ThreadState, reason: Option<&str>) {
        let Some(thread) = self.running_threads.remove(&pane_id) else {
            return;
        };
        // ADR-0004: a failed or interrupted turn can be incomplete or go on.
        if state == ThreadState::Done {
            self.capture_reply(pane_id, &thread);
        }
        if let Some(runtime) = self.terminal_runtimes.get(&thread.terminal_id) {
            let output = runtime.recent_ansi_snapshot(OUTPUT_LINES).text;
            if let Err(err) = threads::write_output(&thread.state_dir, &thread.thread_id, &output) {
                tracing::warn!(thread = %thread.thread_id, %err, "cannot record the thread output");
            }
        }
        if self
            .state
            .popup_pane
            .as_ref()
            .is_some_and(|popup| popup.thread && popup.pane_id == pane_id)
        {
            self.close_popup_pane();
        }
        record_state(
            &threads::registry(&thread.state_dir),
            &thread.thread_id,
            state,
            reason,
        );
        self.refresh_folder_threads();
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

    /// Posts the last reply of a done thread in its channel as its agent,
    /// when the thread did not post: from the CLI transcript, or from the
    /// screen when the session has no transcript. Mentions in the reply open
    /// no thread; to delegate, the agent posts.
    fn capture_reply(&mut self, pane_id: PaneId, thread: &RunningThread) {
        let (Some(channel_name), Some(after)) = (&thread.channel, thread.origin_message_id) else {
            return;
        };
        let transcript_reply = match (thread.cli, &thread.session_id, &self.claude_dir) {
            (Some(cli), Some(session_id), Some(home)) => adapter::last_reply(cli, session_id, home),
            _ => None,
        };
        let reply = transcript_reply.or_else(|| {
            let runtime = self.terminal_runtimes.get(&thread.terminal_id)?;
            let screen = runtime.recent_unwrapped_text_snapshot(OUTPUT_LINES).text;
            // shortcut: a screen names no turn, so it is turn 0; read the
            // turn from the screen when a thread without a transcript can
            // have more turns (#17).
            Some(adapter::TurnReply {
                turn: 0,
                text: adapter::screen_reply(&screen),
            })
        });
        let Some(reply) = reply else {
            return;
        };
        let Some(body) = reply.text else {
            tracing::info!(thread = %thread.thread_id, turn = reply.turn,
                "the thread turn has no text reply to capture");
            return;
        };
        // shortcut: every turn starts after the mention that opened the
        // thread; an absorbed mention (#17) must pass its own message ID.
        let capture = Capture {
            agent: thread.agent.clone(),
            thread_id: thread.thread_id.clone(),
            turn: reply.turn,
            after,
            body,
        };
        let posted = channel::open(&thread.state_dir, channel_name)
            .and_then(|channel| channel.post_capture(capture, time::OffsetDateTime::now_utc()));
        match posted {
            Ok(Some(message)) => {
                tracing::info!(thread = %thread.thread_id, turn = reply.turn,
                    message = message.id, "captured the reply of a thread that did not post");
                if let Some((ws_idx, _)) = self.find_pane(pane_id) {
                    self.announce_channel_message(ws_idx, message);
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(thread = %thread.thread_id, %err, "cannot capture the thread reply");
            }
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

/// Why `thread.open` cannot show a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadPopupError {
    /// Another popup is open; a thread popup only replaces a thread popup.
    PopupOpen,
    NotFound,
    /// The thread is not running and recorded no output.
    NoOutput,
    CannotStart,
}

impl App {
    /// Lists the thread registry of each connected open folder workspace
    /// again. It reads one database per folder workspace: call it after a
    /// registry write and with the folder checks, never from render.
    pub(crate) fn refresh_folder_threads(&mut self) {
        let records: std::collections::HashMap<String, Vec<threads::ThreadRecord>> = self
            .state
            .workspaces
            .iter()
            .filter_map(|workspace| workspace.folder.as_ref())
            .filter(|binding| !self.disconnected_folders.contains(&binding.id))
            .map(|binding| {
                let state_dir = binding.root.join(crate::regents::folder::STATE_DIR);
                let list = threads::registry(&state_dir)
                    .recent(INDEX_THREADS)
                    .unwrap_or_else(|err| {
                        tracing::warn!(%err, "cannot list the threads of a workspace");
                        Vec::new()
                    });
                (binding.id.clone(), list)
            })
            .collect();
        if records != self.folder_threads {
            self.folder_threads = records;
            self.render_dirty.request_generic();
        }
    }

    /// The `INDEX_THREADS` newest threads of the workspace at `index`, newest
    /// first, as of the last refresh. It reads no file, so snapshot code can
    /// call it.
    pub(crate) fn workspace_thread_records(&self, index: usize) -> &[threads::ThreadRecord] {
        self.state
            .workspaces
            .get(index)
            .and_then(|workspace| workspace.folder.as_ref())
            .and_then(|binding| self.folder_threads.get(&binding.id))
            .map_or(&[], Vec::as_slice)
    }

    /// Detected agent state of the running thread `thread_id` of the
    /// workspace whose state directory is `state_dir`.
    pub(crate) fn running_thread_agent_state(
        &self,
        state_dir: &Path,
        thread_id: &str,
    ) -> Option<crate::detect::AgentState> {
        let thread = self
            .running_threads
            .values()
            .find(|thread| thread.thread_id == thread_id && thread.state_dir == state_dir)?;
        self.state
            .terminals
            .get(&thread.terminal_id)
            .map(|terminal| terminal.state)
    }

    /// True when the open popup shows a thread: the pane of a running thread
    /// or the output of an ended one. A click outside closes it, and another
    /// thread replaces it.
    pub(crate) fn popup_shows_thread(&self) -> bool {
        self.state.popup_pane.as_ref().is_some_and(|popup| {
            popup.thread
                || self
                    .state
                    .terminals
                    .get(&popup.terminal_id)
                    .and_then(|terminal| terminal.launch_argv.as_deref())
                    .is_some_and(|argv| {
                        matches!(argv, [_, group, command, _]
                            if group == "thread"
                                && command == crate::cli::thread_bindings::OUTPUT_COMMAND)
                    })
        })
    }

    /// Shows the thread `thread_id` as a popup over the layout: the pane of a
    /// running thread, or the recorded output of an ended one, read only.
    /// Closing the popup of a running thread keeps the thread.
    pub(crate) fn open_thread_popup(
        &mut self,
        state_dir: &Path,
        thread_id: &str,
    ) -> Result<(), ThreadPopupError> {
        if self.state.popup_pane.is_some() {
            if !self.popup_shows_thread() {
                return Err(ThreadPopupError::PopupOpen);
            }
            self.close_popup_pane();
        }
        let running = self
            .running_threads
            .iter()
            .find(|(_, thread)| thread.thread_id == thread_id && thread.state_dir == state_dir)
            .map(|(&pane_id, thread)| (pane_id, thread.terminal_id.clone()));
        if let Some((pane_id, terminal_id)) = running {
            self.state.popup_pane = Some(crate::app::state::PopupPaneState {
                pane_id,
                terminal_id,
                width: None,
                height: None,
                thread: true,
            });
            self.state.mode = crate::app::Mode::Terminal;
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            return Ok(());
        }
        if !threads::has_output(state_dir, thread_id) {
            let known = threads::registry(state_dir)
                .list()
                .is_ok_and(|records| records.iter().any(|record| record.id == thread_id));
            return Err(if known {
                ThreadPopupError::NoOutput
            } else {
                ThreadPopupError::NotFound
            });
        }
        let argv = output_argv(thread_id).map_err(|_| ThreadPopupError::CannotStart)?;
        let folder = state_dir.parent().unwrap_or(state_dir).to_path_buf();
        self.spawn_popup_argv_command(
            &argv,
            Some(folder),
            Vec::new(),
            crate::app::popup::PopupGeometry::default(),
        )
        .map_err(|err| {
            tracing::warn!(thread = %thread_id, %err, "cannot open the thread output popup");
            ThreadPopupError::CannotStart
        })
    }
}

/// Command line of the popup of an ended thread: the running binary's
/// `thread output` command, in the workspace folder.
fn output_argv(thread_id: &str) -> std::io::Result<Vec<String>> {
    // Unit tests run in the test binary, which must not start itself again.
    #[cfg(test)]
    let program = std::path::PathBuf::from(if cfg!(windows) {
        "C:\\Windows\\System32\\whoami.exe"
    } else {
        "/usr/bin/true"
    });
    #[cfg(not(test))]
    let program = crate::platform::launch_executable()?;
    Ok(vec![
        program.display().to_string(),
        "thread".into(),
        crate::cli::thread_bindings::OUTPUT_COMMAND.into(),
        thread_id.into(),
    ])
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
impl App {
    /// Registers each background pane of the workspace at `ws_idx` as a
    /// running thread of `#geral` in the thread index, for render profiles.
    pub(crate) fn register_test_threads(&mut self, ws_idx: usize) {
        let root = PathBuf::from("/bench");
        let state_dir = root.join(crate::regents::folder::STATE_DIR);
        let workspace = &mut self.state.workspaces[ws_idx];
        workspace.folder = Some(crate::regents::folder::FolderBinding {
            id: "bench".into(),
            root,
        });
        let panes: Vec<(PaneId, TerminalId)> = workspace
            .background_panes
            .iter()
            .map(|(&pane_id, pane)| (pane_id, pane.attached_terminal_id.clone()))
            .collect();
        let mut records = Vec::new();
        for (number, (pane_id, terminal_id)) in panes.into_iter().enumerate() {
            let id = format!("t{}", number + 1);
            records.push(threads::ThreadRecord {
                id: id.clone(),
                agent: "chief".into(),
                cli: Some(crate::regents::agents::AgentCli::Claude),
                origin: ThreadOrigin::Channel,
                channel: Some("geral".into()),
                origin_message_id: Some(1),
                opener_thread_id: None,
                depth: 0,
                isolation: Some(crate::regents::agents::Isolation::Shared),
                state: ThreadState::Active,
                end_reason: None,
                session_id: None,
                created_at: "2026-11-27T18:04:05Z".into(),
                updated_at: "2026-11-27T18:04:05Z".into(),
            });
            self.running_threads.insert(
                pane_id,
                RunningThread {
                    state_dir: state_dir.clone(),
                    thread_id: id,
                    agent: "chief".into(),
                    token: String::new(),
                    terminal_id,
                    progress: ThreadProgress::default(),
                    cli: Some(AgentCli::Claude),
                    session_id: None,
                    channel: Some("geral".into()),
                    origin_message_id: Some(1),
                },
            );
        }
        // The index is newest first.
        records.reverse();
        self.folder_threads.insert("bench".into(), records);
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
            app.claude_dir = Some(base.0.join("claude"));
            Self { app, base }
        }

        /// What a fake claude writes: the transcript of the session `sid`,
        /// from a sanitized claude 2.1.332 transcript.
        fn transcript(&self, fixture: &str) {
            let project = self.base.0.join("claude").join("projects").join("-projeto");
            std::fs::create_dir_all(&project).unwrap();
            let source = format!(
                "{}/tests/fixtures/regents/claude-2.1.332/{fixture}.jsonl",
                env!("CARGO_MANIFEST_DIR")
            );
            std::fs::copy(source, project.join("sid.jsonl")).unwrap();
        }

        /// A workspace with the channel `geral` and its message 1, the
        /// mention that opens the test threads.
        fn channel_workspace(&mut self) -> (usize, PathBuf, String) {
            let (ws_idx, state_dir) = self.workspace("projeto");
            let workspace_id = self.app.public_workspace_id(ws_idx);
            self.request(
                "channel.create",
                json!({"workspace_id": workspace_id, "channel": "geral"}),
            );
            let posted = self.request(
                "channel.post",
                json!({"workspace_id": workspace_id, "channel": "geral", "body": "pergunta"}),
            );
            assert_eq!(posted["result"]["message"]["id"], 1, "{posted}");
            (ws_idx, state_dir, workspace_id)
        }

        /// The agent of the thread works and goes idle: the thread is done.
        fn finish_turn(&mut self, pane_id: PaneId) {
            let public_pane_id = self.public_pane_id(pane_id);
            for state in ["working", "idle"] {
                let reported = self.request(
                    "pane.report_agent",
                    json!({"pane_id": public_pane_id, "source": "custom:test",
                           "agent": "claude", "state": state}),
                );
                assert!(reported.get("error").is_none(), "{reported}");
            }
            assert!(!self.app.running_threads.contains_key(&pane_id));
        }

        /// A post of the thread `t1` with its credential.
        fn thread_post(&mut self, workspace_id: &str, token: &str) -> Value {
            self.request(
                "channel.post",
                json!({"workspace_id": workspace_id, "channel": "geral", "body": "postei",
                       "origin_thread_id": "t1", "thread_token": token}),
            )
        }

        fn messages(state_dir: &Path) -> Vec<Message> {
            channel::open(state_dir, "geral")
                .unwrap()
                .read(ReadRange::default())
                .unwrap()
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

    #[tokio::test]
    async fn a_running_thread_opens_as_a_popup_that_closes_without_ending_it() {
        let mut fixture = Fixture::new("popup-running");
        let (ws_idx, state_dir) = fixture.workspace("projeto");
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let terminal_id = fixture.app.running_threads[&pane_id].terminal_id.clone();
        let public_pane_id = fixture.public_pane_id(pane_id);
        fixture.request(
            "pane.report_agent",
            json!({"pane_id": public_pane_id, "source": "custom:test", "agent": "claude",
                   "state": "blocked"}),
        );

        let listed = fixture.request("thread.list", json!({}));
        let indexed = fixture.app.workspace_thread_infos(ws_idx);
        let opened = fixture.request("thread.open", json!({"thread_id": "t1"}));

        assert_eq!(
            listed["result"]["threads"][0]["agent_status"], "blocked",
            "{listed}"
        );
        assert_eq!(indexed.len(), 1);
        assert_eq!(
            indexed[0].agent_status,
            Some(crate::api::schema::AgentStatus::Blocked)
        );
        assert!(opened.get("error").is_none(), "{opened}");
        let popup = fixture.app.state.popup_pane.as_ref().unwrap();
        assert!(popup.thread);
        assert_eq!(popup.pane_id, pane_id);
        assert_eq!(
            fixture.app.state.terminals[&terminal_id]
                .manual_label
                .as_deref(),
            Some("@chief · t1")
        );
        fixture.app.state.assert_invariants_for_test();

        let closed = fixture.request("popup.close", json!({}));

        assert!(closed.get("error").is_none(), "{closed}");
        assert!(fixture.app.state.popup_pane.is_none());
        assert!(fixture.app.running_threads.contains_key(&pane_id));
        assert!(fixture.app.terminal_runtimes.get(&terminal_id).is_some());
        assert!(fixture.app.state.terminals.contains_key(&terminal_id));
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Active);
    }

    #[tokio::test]
    async fn an_ended_thread_keeps_its_output_and_opens_it_read_only() {
        let mut fixture = Fixture::new("popup-ended");
        let (ws_idx, state_dir) = fixture.workspace("projeto");
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.request("thread.open", json!({"thread_id": "t1"}));
        let public_pane_id = fixture.public_pane_id(pane_id);

        fixture.request("pane.close", json!({"pane_id": public_pane_id}));

        assert!(
            fixture.app.state.popup_pane.is_none(),
            "the popup closes with its thread"
        );
        assert!(threads::has_output(&state_dir, "t1"));
        let indexed = fixture.app.workspace_thread_infos(ws_idx);
        assert_eq!(indexed[0].state, crate::api::schema::ThreadState::Failed);
        assert_eq!(indexed[0].agent_status, None);

        let opened = fixture.request("thread.open", json!({"thread_id": "t1"}));
        let output_popup = fixture
            .app
            .state
            .popup_pane
            .as_ref()
            .map(|popup| popup.thread);
        let shows_thread = fixture.app.popup_shows_thread();
        let reopened = fixture.request("thread.open", json!({"thread_id": "t1"}));
        let unknown = fixture.request("thread.open", json!({"thread_id": "t7"}));

        assert!(opened.get("error").is_none(), "{opened}");
        assert_eq!(
            output_popup,
            Some(false),
            "the output runs in its own popup"
        );
        assert!(shows_thread, "the output popup is a thread popup");
        assert!(reopened.get("error").is_none(), "{reopened}");
        assert_eq!(unknown["error"]["code"], "thread_not_found", "{unknown}");
    }

    #[tokio::test]
    async fn the_thread_index_follows_the_registry_newest_first() {
        let mut fixture = Fixture::new("index");
        let (ws_idx, state_dir) = fixture.workspace("projeto");
        assert!(fixture.app.workspace_thread_infos(ws_idx).is_empty());
        fixture.thread(ws_idx, &state_dir);
        fixture.thread(ws_idx, &state_dir);
        fixture.thread(ws_idx, &state_dir);

        let ids: Vec<_> = fixture
            .app
            .workspace_thread_infos(ws_idx)
            .into_iter()
            .map(|thread| thread.id)
            .collect();

        assert_eq!(ids, ["t3", "t2", "t1"]);
    }

    #[tokio::test]
    async fn a_done_thread_that_did_not_post_posts_its_last_reply_as_its_agent() {
        let mut fixture = Fixture::new("capture");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("one-turn");

        fixture.finish_turn(pane_id);

        let messages = Fixture::messages(&state_dir);
        assert_eq!(messages.len(), 2, "{messages:?}");
        let captured = &messages[1];
        assert_eq!(captured.author.kind, AuthorKind::Agent);
        assert_eq!(captured.author.id, "chief");
        assert_eq!(captured.origin_thread_id.as_deref(), Some("t1"));
        assert!(captured.captured);
        // Byte for byte the last text block of the transcript, no prompt echo.
        assert_eq!(captured.body.as_bytes(), b"pronto, linha 1\nlinha 2");
        let read = fixture.request("channel.read", json!({"channel": "geral"}));
        assert_eq!(read["result"]["messages"][1]["captured"], true, "{read}");
        assert_eq!(
            read["result"]["messages"][0].get("captured"),
            None,
            "{read}"
        );
    }

    #[tokio::test]
    async fn a_thread_that_posted_gets_no_capture() {
        let mut fixture = Fixture::new("capture-posted");
        let (ws_idx, state_dir, workspace_id) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("one-turn");
        let token = fixture.app.running_threads[&pane_id].token.clone();

        let posted = fixture.thread_post(&workspace_id, &token);
        fixture.finish_turn(pane_id);

        assert!(posted.get("error").is_none(), "{posted}");
        let messages = Fixture::messages(&state_dir);
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[1].body, "postei");
        assert!(!messages[1].captured);
    }

    #[tokio::test]
    async fn a_post_after_the_idle_adds_no_second_message() {
        let mut fixture = Fixture::new("capture-late");
        let (ws_idx, state_dir, workspace_id) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("one-turn");
        let token = fixture.app.running_threads[&pane_id].token.clone();

        fixture.finish_turn(pane_id);
        let late = fixture.thread_post(&workspace_id, &token);

        assert_eq!(late["error"]["code"], "invalid_thread_credential", "{late}");
        let messages = Fixture::messages(&state_dir);
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[1].captured);
    }

    #[tokio::test]
    async fn a_failed_thread_or_a_turn_with_only_tool_calls_posts_nothing() {
        let mut fixture = Fixture::new("capture-none");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let failed = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("one-turn");
        let public_pane_id = fixture.public_pane_id(failed);
        fixture.request("pane.close", json!({"pane_id": public_pane_id}));
        let tool_only = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("tool-calls-only");
        // A screen reply must not replace a transcript turn without text.
        let terminal_id = &fixture.app.running_threads[&tool_only].terminal_id;
        fixture
            .app
            .terminal_runtimes
            .get(terminal_id)
            .unwrap()
            .test_process_pty_bytes(
                format!("{}\r\nresposta da tela\r\n", adapter::PROMPT_LAST_LINE).as_bytes(),
            );

        fixture.finish_turn(tool_only);

        assert_eq!(Fixture::messages(&state_dir).len(), 1);
    }

    #[tokio::test]
    async fn an_interrupted_thread_posts_nothing() {
        let mut fixture = Fixture::new("capture-interrupted");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        fixture.thread(ws_idx, &state_dir);
        fixture.transcript("one-turn");

        fixture.app.interrupt_running_threads();

        assert_eq!(
            Fixture::records(&state_dir)[0].state,
            ThreadState::Interrupted
        );
        assert_eq!(Fixture::messages(&state_dir).len(), 1);
    }

    #[tokio::test]
    async fn a_mention_in_a_captured_reply_opens_no_thread() {
        let mut fixture = Fixture::new("capture-mention");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        std::fs::write(
            state_dir.join("agents").join("reviewer.md"),
            "+++\nname = \"reviewer\"\ncli = \"claude\"\n+++\n",
        )
        .unwrap();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("two-turns");

        fixture.finish_turn(pane_id);

        let messages = Fixture::messages(&state_dir);
        assert_eq!(messages[1].body, "segundo turno @reviewer");
        assert!(messages[1].mentions.is_empty());
        assert!(fixture.app.running_threads.is_empty());
        assert_eq!(Fixture::records(&state_dir).len(), 1);
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
