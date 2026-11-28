//! Regents thread runtime (ADR-0001, ADR-0004). A mention opens a thread: the
//! agent CLI runs in a background pane of the workspace, outside every tab,
//! so it keeps output parsing and agent detection but never causes
//! presentation work. The thread registry in the workspace folder records
//! it. When the agent finishes its turn, the CLI process ends and the thread
//! keeps its CLI session. A threaded reply to a message of a done thread
//! resumes that session in the next turn of the same thread (#14).

use std::path::{Path, PathBuf};

use crate::api::schema::{EventData, EventEnvelope, EventKind};
use crate::app::actions::PaneStateUpdate;
use crate::app::App;
use crate::layout::PaneId;
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::regents::adapter;
use crate::regents::agents::{self, AgentCli, AgentDefinition, Isolation};
use crate::regents::channel::{self, AuthorKind, Capture, Channel, Message, ReadRange};
use crate::regents::threads::{
    self, Admission, NewThread, ThreadOrigin, ThreadProgress, ThreadRecord, ThreadState, ThreadStep,
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
const SERVER_RESTARTED: &str = "the server that ran it stopped";
const WORKSPACE_CLOSED: &str = "the workspace closed";
/// Most queued threads that one drain takes: each step starts a thread or
/// ends one that cannot start, and the next admission point goes on.
const MAX_DRAIN_STEPS: usize = 64;

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
    /// Channel of the mention that opened the thread.
    channel: Option<String>,
    /// Turn of the CLI session that runs, and the message that started it:
    /// a post of the thread after it is the reply of the turn.
    turn: u32,
    turn_message_id: Option<u64>,
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
    /// Admits one thread for each agent that `message` mentions, except
    /// `replied_agent`, whose thread the message answers, then drains the
    /// queue. A thread that cannot be admitted is logged; the message stays
    /// posted. A message posted from a thread opens none: delegation, with
    /// its opener and depth limit, is #18.
    pub(crate) fn open_mention_threads(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        message: &Message,
        agents: &[AgentDefinition],
        replied_agent: Option<&str>,
    ) {
        if message.mentions.is_empty() || message.origin_thread_id.is_some() {
            return;
        }
        for name in &message.mentions {
            if replied_agent == Some(name.as_str()) {
                continue;
            }
            let Some(agent) = agents.iter().find(|agent| &agent.name == name) else {
                continue;
            };
            if let Err(err) = self.admit_mention(ws_idx, state_dir, agent, message) {
                tracing::warn!(agent = %agent.name, %err, "cannot admit a thread");
            }
        }
        self.drain_thread_queue(state_dir);
    }

    /// The admission of a mention of `agent`: a new queued thread, or the
    /// mention moves the anchor of its queued thread in the channel.
    fn admit_mention(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        agent: &AgentDefinition,
        origin: &Message,
    ) -> Result<(), String> {
        if !adapter::has_thread_adapter(agent.cli) {
            let reason = format!("the {} CLI has no thread adapter yet", agent.cli.as_str());
            self.post_system_message(
                Some(ws_idx),
                state_dir,
                &origin.channel,
                None,
                format!("a thread de @{} não iniciou: {reason}", agent.name),
            );
            return Err(reason);
        }
        let thread = NewThread {
            agent: agent.name.clone(),
            cli: agent.cli,
            origin: ThreadOrigin::Channel,
            channel: Some(origin.channel.clone()),
            origin_message_id: Some(origin.id),
            opener_thread_id: None,
            depth: 0,
            // Worktree isolation is #24: every thread runs in the folder.
            isolation: Isolation::Shared,
            session_id: Some(adapter::new_session_id()),
        };
        let admission = threads::registry(state_dir)
            .admit_mention(thread, time::OffsetDateTime::now_utc())
            .map_err(|err| err.to_string())?;
        if let Admission::Absorbed(thread_id) = &admission {
            tracing::info!(thread = %thread_id, message = origin.id, "a queued thread absorbed a mention");
        }
        self.refresh_folder_threads();
        Ok(())
    }

    /// Starts the queued threads of the workspace whose state directory is
    /// `state_dir`, oldest first, while fewer than `max_active_threads` take
    /// a slot. Every end of a thread calls it, and so do the admissions and
    /// the folder checks. It reads the manifest and the registry: never call
    /// it from render. A closed workspace fails its queued threads; a
    /// disconnected one keeps them.
    pub(crate) fn drain_thread_queue(&mut self, state_dir: &Path) {
        let Some(ws_idx) = self.folder_workspace_index(state_dir) else {
            self.fail_queued_threads(state_dir, WORKSPACE_CLOSED);
            return;
        };
        if self.is_disconnected_workspace(ws_idx) {
            return;
        }
        let root = state_dir.parent().unwrap_or(state_dir);
        let limit = match crate::regents::folder::read_manifest(root) {
            Ok(manifest) => manifest.max_active_threads,
            Err(err) => {
                tracing::warn!(%err, "the thread queue waits for a valid workspace.toml");
                return;
            }
        };
        let registry = threads::registry(state_dir);
        for _ in 0..MAX_DRAIN_STEPS {
            match registry.promote_next(limit, time::OffsetDateTime::now_utc()) {
                Ok(Some((queued, promoted))) => {
                    self.start_queued_turn(ws_idx, state_dir, queued, promoted);
                }
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(%err, "cannot take a thread from the queue");
                    return;
                }
            }
        }
    }

    /// The open workspace whose folder has the state directory `state_dir`.
    fn folder_workspace_index(&self, state_dir: &Path) -> Option<usize> {
        self.state.workspaces.iter().position(|workspace| {
            workspace.folder.as_ref().is_some_and(|binding| {
                binding.root.join(crate::regents::folder::STATE_DIR) == state_dir
            })
        })
    }

    /// Starts the turn of `promoted`, a thread that just left the queue as
    /// `queued`. A new thread that cannot start fails; a resume that cannot
    /// start is done again in its previous turn. Both say why in the channel.
    fn start_queued_turn(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        queued: ThreadRecord,
        promoted: ThreadRecord,
    ) {
        let resumed = queued.turn_message_id.is_some();
        let (thread_id, agent, channel, turn) = (
            promoted.id.clone(),
            promoted.agent.clone(),
            promoted.channel.clone(),
            promoted.turn,
        );
        let started = queued_turn_argv(state_dir, &queued)
            .and_then(|argv| self.start_thread(ws_idx, state_dir, promoted, &argv));
        let registry = threads::registry(state_dir);
        let body = match started {
            Ok(_) if resumed => {
                let message_id = queued.queued_message_id.unwrap_or_default();
                tracing::info!(thread = %thread_id, turn, message = message_id, "resumed a thread");
                self.emit_event(EventEnvelope {
                    event: EventKind::ThreadResumed,
                    data: EventData::ThreadResumed {
                        workspace_id: self.public_workspace_id(ws_idx),
                        thread_id,
                        turn,
                        message_id,
                    },
                });
                return;
            }
            Ok(_) => return,
            Err(reason) if resumed => {
                if let Err(err) = registry.undo_resume(&queued, time::OffsetDateTime::now_utc()) {
                    // No pane runs it: an active row would take a slot.
                    tracing::warn!(thread = %thread_id, %err, "cannot undo the thread resume");
                    record_state(&registry, &thread_id, ThreadState::Done, None);
                }
                format!("a thread {thread_id} de @{agent} não foi retomada: {reason}")
            }
            Err(reason) => {
                record_state(&registry, &thread_id, ThreadState::Failed, Some(&reason));
                format!("a thread de @{agent} não iniciou: {reason}")
            }
        };
        self.refresh_folder_threads();
        if let Some(channel) = &channel {
            self.post_system_message(Some(ws_idx), state_dir, channel, Some(&thread_id), body);
        }
    }

    /// Ends the queued threads of `state_dir` as failed with `reason`, and
    /// says so in their channels.
    fn fail_queued_threads(&mut self, state_dir: &Path, reason: &str) {
        let failed = threads::registry(state_dir)
            .fail_queued(reason, time::OffsetDateTime::now_utc())
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "cannot end the queued threads");
                Vec::new()
            });
        for (thread_id, agent, channel) in &failed {
            if let Some(channel) = channel {
                self.post_system_message(
                    None,
                    state_dir,
                    channel,
                    Some(thread_id),
                    format!("a thread de @{agent} falhou: {reason}"),
                );
            }
        }
        if !failed.is_empty() {
            self.refresh_folder_threads();
        }
    }

    /// The folder checks of the thread queue. The first check of a folder
    /// in this process marks interrupted the active threads that no pane of
    /// this process runs: the server that ran them stopped. Then each queue
    /// drains, so a higher limit applies at the next check.
    pub(crate) fn check_folder_thread_queues(&mut self) {
        let folders: Vec<(String, PathBuf)> = self
            .state
            .workspaces
            .iter()
            .filter_map(|workspace| workspace.folder.as_ref())
            .filter(|binding| !self.disconnected_folders.contains(&binding.id))
            .map(|binding| {
                (
                    binding.id.clone(),
                    binding.root.join(crate::regents::folder::STATE_DIR),
                )
            })
            .collect();
        for (folder_id, state_dir) in folders {
            if self.recovered_thread_folders.insert(folder_id) {
                self.interrupt_orphan_threads(&state_dir);
            }
            self.drain_thread_queue(&state_dir);
        }
    }

    /// Marks interrupted the threads of `state_dir` that take a slot but
    /// that no pane of this process runs.
    fn interrupt_orphan_threads(&mut self, state_dir: &Path) {
        let registry = threads::registry(state_dir);
        let orphans: Vec<ThreadRecord> = registry
            .slot_threads()
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "cannot list the active threads");
                Vec::new()
            })
            .into_iter()
            .filter(|record| self.running_thread_pane_id(state_dir, &record.id).is_none())
            .collect();
        for record in orphans {
            self.interrupt_thread(
                state_dir,
                &record.id,
                &record.agent,
                record.channel.as_deref(),
                SERVER_RESTARTED,
            );
        }
        self.refresh_folder_threads();
    }

    /// Compare-and-set of an interruption: the thread becomes interrupted
    /// with `reason` and says so in its channel, once, when it is still
    /// active.
    fn interrupt_thread(
        &mut self,
        state_dir: &Path,
        thread_id: &str,
        agent: &str,
        channel: Option<&str>,
        reason: &str,
    ) {
        match threads::registry(state_dir).interrupt(
            thread_id,
            reason,
            time::OffsetDateTime::now_utc(),
        ) {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                tracing::warn!(thread = %thread_id, %err, "cannot record the thread interruption");
                return;
            }
        }
        let Some(channel) = channel else {
            return;
        };
        self.post_system_message(
            self.folder_workspace_index(state_dir),
            state_dir,
            channel,
            Some(thread_id),
            format!("a thread de @{agent} foi interrompida: {reason}"),
        );
    }

    /// Records `thread` as active outside the queue and starts `argv` for it
    /// in a background pane: tests only, as a thread that holds a slot.
    #[cfg(test)]
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
        let thread_id = thread.id.clone();
        self.start_thread(ws_idx, state_dir, thread, argv)
            .inspect_err(|reason| {
                record_state(&registry, &thread_id, ThreadState::Failed, Some(reason));
                self.refresh_folder_threads();
            })
    }

    /// Starts `argv` for the active thread `thread` in a background pane.
    /// The error is why the CLI did not start.
    fn start_thread(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        thread: threads::ThreadRecord,
        argv: &[String],
    ) -> Result<PaneId, String> {
        #[cfg(test)]
        let argv = &match &self.test_thread_prefix {
            Some(prefix) => prefix.iter().chain(argv).cloned().collect::<Vec<_>>(),
            None => argv.to_vec(),
        };
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
                        turn: thread.turn,
                        turn_message_id: thread.turn_message_id,
                    },
                );
                Ok(pane_id)
            }
            Err(err) => Err(format!("cannot start {}: {err}", argv[0])),
        }
    }

    /// A threaded reply: `reply`, a human post that answers a message of an
    /// agent thread, resumes that thread when it is done. The result is the
    /// agent of the answered thread, which a mention in `reply` then does
    /// not open again; `None` means that `reply` is not a threaded reply and
    /// follows the normal routing. A thread that does not resume says why in
    /// the channel. A post of a thread resumes nothing: delegation is #18.
    pub(crate) fn resume_replied_thread(
        &mut self,
        ws_idx: usize,
        state_dir: &Path,
        channel: &Channel,
        reply: &Message,
        agents: &[AgentDefinition],
    ) -> Option<String> {
        let replied_id = reply.reply_to?;
        if reply.origin_thread_id.is_some() {
            return None;
        }
        let replied = read_message(channel, replied_id)?;
        if replied.author.kind != AuthorKind::Agent {
            return None;
        }
        let thread_id = replied.origin_thread_id?;
        let refusal = match self.resume_thread(state_dir, channel, reply, &thread_id, agents) {
            Ok(_) => return Some(replied.author.id),
            Err(refusal) => refusal,
        };
        let (body, link) = match &refusal {
            Refusal::Running(body) | Refusal::Ended(body) => {
                (body.clone(), Some(thread_id.as_str()))
            }
            Refusal::NoThread(body) => (body.clone(), None),
        };
        self.post_system_message(Some(ws_idx), state_dir, channel.name(), link, body);
        // The thread runs: it reads the reply in the channel. Otherwise a
        // mention in the reply opens a new thread.
        matches!(refusal, Refusal::Running(_)).then_some(replied.author.id)
    }

    /// The admission of the resume of the done thread `thread_id` by
    /// `reply`: the thread enters the queue for its next turn, and the drain
    /// starts it. The error says why it did not resume.
    fn resume_thread(
        &mut self,
        state_dir: &Path,
        channel: &Channel,
        reply: &Message,
        thread_id: &str,
        agents: &[AgentDefinition],
    ) -> Result<(), Refusal> {
        let registry = threads::registry(state_dir);
        let record = registry
            .get(thread_id)
            .map_err(|err| Refusal::Ended(format!("a thread {thread_id} não foi retomada: {err}")))?
            .ok_or_else(|| {
                Refusal::NoThread(format!("a thread {thread_id} não existe neste workspace"))
            })?;
        let agent = record.agent.clone();
        let new_thread = format!("mencione @{agent} para abrir uma thread nova");
        let ended = |body: String| Err(Refusal::Ended(body));
        match record.state {
            ThreadState::Done => {}
            ThreadState::Active | ThreadState::Queued => {
                return Err(Refusal::Running(still_running(&agent, thread_id)));
            }
            // shortcut: an interrupted thread resumes only by `thread resume`
            // (#19), which adds it to the compare-and-set of `resume`.
            ThreadState::Interrupted => {
                return ended(format!(
                    "a thread {thread_id} de @{agent} foi interrompida; {new_thread}"
                ));
            }
            ThreadState::Failed => {
                return ended(format!(
                    "a thread {thread_id} de @{agent} falhou e não é retomada; {new_thread}"
                ));
            }
            ThreadState::Unknown => {
                return ended(format!(
                    "a thread {thread_id} de @{agent} tem um estado desconhecido e não é retomada"
                ));
            }
        }
        let not_resumed = |reason: &str| {
            Refusal::Ended(format!(
                "a thread {thread_id} de @{agent} não foi retomada: {reason}"
            ))
        };
        // Message IDs are per channel: a turn anchor of another channel
        // means nothing here.
        if record.channel.as_deref() != Some(channel.name()) {
            let home = record.channel.as_deref().unwrap_or_default();
            return Err(not_resumed(&format!(
                "the thread belongs to #{home}; reply there"
            )));
        }
        let Some(definition) = agents.iter().find(|definition| definition.name == agent) else {
            return ended(format!(
                "@{agent} não é mais um agente deste workspace; a thread {thread_id} não é retomada"
            ));
        };
        if record.cli != Some(definition.cli) {
            return Err(not_resumed(
                "the agent CLI changed after the thread started",
            ));
        }
        match record.isolation {
            Some(Isolation::Shared) => {}
            // shortcut: no thread runs in a worktree yet; #24 resumes one in
            // its own worktree and refuses one whose worktree is gone.
            Some(Isolation::Worktree) => {
                return Err(not_resumed("worktree threads cannot resume yet"));
            }
            None => return Err(not_resumed("the thread isolation is unknown")),
        }
        if !adapter::has_thread_adapter(definition.cli) {
            return Err(not_resumed(&format!(
                "the {} CLI has no thread adapter yet",
                definition.cli.as_str()
            )));
        }
        let session_id = record.session_id.clone().unwrap_or_default();
        let has_session = self
            .claude_dir
            .as_deref()
            .is_some_and(|home| adapter::has_session(definition.cli, &session_id, home));
        if !has_session {
            return Err(not_resumed("the CLI session has no transcript"));
        }
        // At most one queued thread per agent and channel.
        match registry.queued_thread(&agent, channel.name()) {
            Ok(Some(queued)) => {
                return Err(Refusal::Running(format!(
                    "@{agent} já tem a thread {queued} na fila; a resposta fica no canal"
                )));
            }
            Ok(None) => {}
            Err(err) => return Err(not_resumed(&err.to_string())),
        }
        registry
            .admit_resume(
                thread_id,
                record.turn,
                reply.id,
                time::OffsetDateTime::now_utc(),
            )
            .map_err(|err| not_resumed(&err.to_string()))?
            .ok_or_else(|| Refusal::Running(still_running(&agent, thread_id)))?;
        self.refresh_folder_threads();
        self.drain_thread_queue(state_dir);
        Ok(())
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
    /// agent finished its turn is done; a thread whose agent began to wait
    /// for an approval says so in its channel.
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
            match thread.progress.observe(update.state) {
                ThreadStep::Running => {}
                ThreadStep::Blocked => {
                    let Some(channel) = thread.channel.clone() else {
                        continue;
                    };
                    let state_dir = thread.state_dir.clone();
                    let thread_id = thread.thread_id.clone();
                    let body = format!("@{} precisa de aprovação", thread.agent);
                    self.post_system_message(
                        Some(update.ws_idx),
                        &state_dir,
                        &channel,
                        Some(&thread_id),
                        body,
                    );
                }
                ThreadStep::Done => self.end_thread(update.pane_id, ThreadState::Done, None),
            }
        }
    }

    /// Posts `body` as a system message of the channel `channel_name` and
    /// announces it. A post that fails is logged.
    pub(crate) fn post_system_message(
        &mut self,
        ws_idx: Option<usize>,
        state_dir: &Path,
        channel_name: &str,
        thread_id: Option<&str>,
        body: String,
    ) {
        let posted = channel::open(state_dir, channel_name).and_then(|channel| {
            channel.post_system(
                body,
                thread_id.map(str::to_owned),
                time::OffsetDateTime::now_utc(),
            )
        });
        match (posted, ws_idx) {
            (Ok(message), Some(ws_idx)) => {
                self.announce_channel_message(ws_idx, message);
            }
            (Ok(_), None) => {}
            (Err(err), _) => {
                tracing::warn!(channel = %channel_name, %err, "cannot post a system message");
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
            self.interrupt_thread(
                &thread.state_dir,
                &thread.thread_id,
                &thread.agent,
                thread.channel.as_deref(),
                SERVER_HANDOFF,
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
        if let (ThreadState::Failed, Some(reason), Some(channel)) = (state, reason, &thread.channel)
        {
            let ws_idx = self.find_pane(pane_id).map(|(ws_idx, _)| ws_idx);
            self.post_system_message(
                ws_idx,
                &thread.state_dir,
                channel,
                Some(&thread.thread_id),
                format!("a thread de @{} falhou: {reason}", thread.agent),
            );
        }
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
        // The slot is free.
        self.drain_thread_queue(&thread.state_dir);
    }

    /// Posts the last reply of a done thread turn in its channel as its
    /// agent, when the thread did not post after the turn started: from the
    /// CLI transcript, or from the screen when the session has no
    /// transcript. Mentions in the reply open no thread; to delegate, the
    /// agent posts.
    fn capture_reply(&mut self, pane_id: PaneId, thread: &RunningThread) {
        let (Some(channel_name), Some(after)) = (&thread.channel, thread.turn_message_id) else {
            return;
        };
        let transcript_reply = match (thread.cli, &thread.session_id, &self.claude_dir) {
            (Some(cli), Some(session_id), Some(home)) => adapter::last_reply(cli, session_id, home),
            _ => None,
        };
        if let Some(reply) = transcript_reply
            .as_ref()
            .filter(|reply| reply.turn < thread.turn)
        {
            // The last turn of the transcript is an earlier turn: its reply
            // is already in the channel.
            tracing::info!(thread = %thread.thread_id, turn = thread.turn,
                transcript_turn = reply.turn, "the transcript has no reply of the thread turn");
            return;
        }
        let reply = transcript_reply.or_else(|| {
            let runtime = self.terminal_runtimes.get(&thread.terminal_id)?;
            let screen = runtime.recent_unwrapped_text_snapshot(OUTPUT_LINES).text;
            // Every turn prompt ends with the same line, so the screen reply
            // is the one of the last turn; the registry names the turn.
            Some(adapter::TurnReply {
                turn: thread.turn,
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

/// Why a threaded reply did not resume its thread: the system message to
/// post.
#[derive(Debug)]
enum Refusal {
    /// The thread runs or waits; it reads the reply in the channel.
    Running(String),
    /// The thread cannot continue; a mention opens a new thread.
    Ended(String),
    /// The registry has no such thread.
    NoThread(String),
}

fn still_running(agent: &str, thread_id: &str) -> String {
    format!("@{agent} ainda está na thread {thread_id}; a resposta fica no canal")
}

/// The prompt of the resume of `thread` by `reply`: the messages after the
/// message that started the previous turn, without the posts of the thread,
/// at most `adapter::RECENT_MESSAGES`.
fn resume_prompt(channel: &Channel, reply: &Message, thread: &threads::ThreadRecord) -> String {
    let previous = thread.turn_message_id.unwrap_or(0);
    let new: Vec<Message> = match channel.read(ReadRange {
        before: Some(reply.id.saturating_add(1)),
        limit: Some(adapter::RECENT_MESSAGES),
    }) {
        Ok(recent) => recent
            .into_iter()
            .filter(|message| {
                message.id > previous
                    && message.origin_thread_id.as_deref() != Some(thread.id.as_str())
            })
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "cannot read the channel for a resume prompt");
            vec![reply.clone()]
        }
    };
    adapter::resume_prompt(reply, &new)
}

/// The message `id` of `channel`.
fn read_message(channel: &Channel, id: u64) -> Option<Message> {
    channel
        .read(ReadRange {
            before: Some(id.saturating_add(1)),
            limit: Some(1),
        })
        .inspect_err(|err| tracing::warn!(%err, "cannot read a channel message"))
        .ok()?
        .pop()
        .filter(|message| message.id == id)
}

/// The command line of the turn of `queued`, a thread as it left the
/// queue: its prompt is made again from its anchor. A thread that ran
/// before resumes its CLI session.
fn queued_turn_argv(state_dir: &Path, queued: &ThreadRecord) -> Result<Vec<String>, String> {
    let (Some(channel_name), Some(anchor_id)) = (&queued.channel, queued.queued_message_id) else {
        return Err("the queued thread has no channel message".to_owned());
    };
    let channel = channel::open(state_dir, channel_name).map_err(|err| err.to_string())?;
    let anchor = read_message(&channel, anchor_id)
        .ok_or_else(|| format!("the message {anchor_id} of #{channel_name} is gone"))?;
    let agents = agents::list(state_dir);
    let definition = agents
        .iter()
        .find(|definition| definition.name == queued.agent)
        .ok_or_else(|| format!("@{} is no longer an agent of the workspace", queued.agent))?;
    let session_id = queued.session_id.clone().unwrap_or_default();
    let argv = if queued.turn_message_id.is_some() {
        let prompt = resume_prompt(&channel, &anchor, queued);
        adapter::resume_argv(definition, &prompt, &session_id)
    } else {
        adapter::launch_argv(definition, &mention_prompt(&channel, &anchor), &session_id)
    };
    argv.ok_or_else(|| {
        format!(
            "the {} CLI has no thread adapter yet",
            definition.cli.as_str()
        )
    })
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
                turn: 0,
                turn_message_id: Some(1),
                queue_position: None,
                queued_message_id: None,
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
                    turn: 0,
                    turn_message_id: Some(1),
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
            // The thread command line becomes the arguments of a long sleep.
            app.test_thread_prefix = Some(
                ["sh", "-c", "sleep 30", "fake-cli"]
                    .map(str::to_owned)
                    .to_vec(),
            );
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

    /// Codex is only a registry key here: a synthetic manifest that tells
    /// the screens of the fake CLI apart. It never classifies a real CLI.
    const FAKE_MANIFEST: &str = r#"
id = "codex"

[[rules]]
id = "fake-approval"
state = "blocked"
contains = ["FAKE-APPROVAL"]

[[rules]]
id = "fake-working"
state = "working"
contains = ["FAKE-WORKING"]
"#;
    const FAKE_BLOCKED: &str = "FAKE-APPROVAL allow the command? [y/n]";
    const FAKE_WORKING: &str = "FAKE-WORKING running the command";
    const FAKE_IDLE: &str = "> ";

    /// The synthetic manifest as the local override of the process; the
    /// drop restores the configuration directory.
    struct FakeManifest {
        _lock: std::sync::MutexGuard<'static, ()>,
        old_config: Option<std::ffi::OsString>,
        _dir: TempFolder,
    }

    impl FakeManifest {
        fn install(name: &str) -> Self {
            let lock = crate::config::test_config_env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = TempFolder::new(&format!("fake-manifest-{name}"));
            let old_config = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", &dir.0);
            let path = crate::config::config_dir()
                .join("agent-detection")
                .join("codex.toml");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, FAKE_MANIFEST).unwrap();
            crate::detect::manifest::reload_manifests();
            Self {
                _lock: lock,
                old_config,
                _dir: dir,
            }
        }
    }

    impl Drop for FakeManifest {
        fn drop(&mut self) {
            match self.old_config.take() {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            crate::detect::manifest::reload_manifests();
        }
    }

    impl Fixture {
        /// The fake CLI of the thread draws `screen`; the synthetic manifest
        /// classifies the detection text, and the state reaches the app as
        /// a detection event.
        fn show(&mut self, pane_id: PaneId, screen: &str) -> crate::detect::AgentState {
            let terminal_id = self.app.running_threads[&pane_id].terminal_id.clone();
            let runtime = self.app.terminal_runtimes.get(&terminal_id).unwrap();
            runtime.test_process_pty_bytes(format!("\x1b[2J\x1b[H{screen}").as_bytes());
            let text = runtime.detection_text();
            let state = crate::detect::detect_state(Some(crate::detect::Agent::Codex), &text);
            self.app
                .handle_internal_event(crate::events::AppEvent::StateChanged {
                    pane_id,
                    agent: Some(crate::detect::Agent::Codex),
                    state,
                    visible_blocker: state == crate::detect::AgentState::Blocked,
                    visible_working: state == crate::detect::AgentState::Working,
                    process_exited: false,
                    observed_at: std::time::Instant::now(),
                });
            self.app.state.assert_invariants_for_test();
            state
        }

        /// `agent_status` of the thread `t1` in `thread.list`.
        fn listed_status(&mut self) -> Value {
            let listed = self.request("thread.list", json!({}));
            listed["result"]["threads"][0]["agent_status"].clone()
        }

        /// Bodies of the system messages of `geral`.
        fn system_bodies(state_dir: &Path) -> Vec<String> {
            Self::messages(state_dir)
                .into_iter()
                .filter(|message| message.author.kind == AuthorKind::System)
                .map(|message| message.body)
                .collect()
        }

        fn exit(&mut self, pane_id: PaneId) {
            self.app
                .handle_internal_event(crate::events::AppEvent::PaneDied {
                    pane_id,
                    exit_reason: crate::platform::ChildExitReason::Exited,
                });
            self.app.state.assert_invariants_for_test();
        }
    }

    const BLOCKED_MESSAGE: &str = "@chief precisa de aprovação";

    #[tokio::test]
    async fn a_block_posts_one_system_message_and_the_approval_clears_the_highlight() {
        let _manifest = FakeManifest::install("approve");
        let mut fixture = Fixture::new("blocked-approve");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        use crate::detect::AgentState;

        assert_eq!(fixture.show(pane_id, FAKE_WORKING), AgentState::Working);
        assert_eq!(fixture.listed_status(), "working");
        assert_eq!(fixture.show(pane_id, FAKE_BLOCKED), AgentState::Blocked);
        assert_eq!(fixture.listed_status(), "blocked");
        let indexed = fixture.app.workspace_thread_infos(ws_idx);
        assert_eq!(
            indexed[0].agent_status,
            Some(crate::api::schema::AgentStatus::Blocked)
        );
        // The same block again, as a repeated detection: no second message.
        fixture.show(pane_id, FAKE_BLOCKED);
        assert_eq!(Fixture::system_bodies(&state_dir), [BLOCKED_MESSAGE]);

        // The approval in the popup: the agent works again.
        assert_eq!(fixture.show(pane_id, FAKE_WORKING), AgentState::Working);

        assert_eq!(fixture.listed_status(), "working");
        assert_eq!(Fixture::system_bodies(&state_dir), [BLOCKED_MESSAGE]);
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Active);
        let read = fixture.request("channel.read", json!({"channel": "geral"}));
        let system = &read["result"]["messages"][1];
        assert_eq!(system["author"]["kind"], "system", "{read}");
        assert_eq!(system["author"]["id"], "regents", "{read}");
        assert_eq!(system["origin_thread_id"], "t1", "{read}");
    }

    #[tokio::test]
    async fn each_new_block_posts_one_more_message() {
        let _manifest = FakeManifest::install("again");
        let mut fixture = Fixture::new("blocked-again");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);

        for screen in [FAKE_BLOCKED, FAKE_WORKING, FAKE_BLOCKED, FAKE_WORKING] {
            fixture.show(pane_id, screen);
        }

        assert_eq!(
            Fixture::system_bodies(&state_dir),
            [BLOCKED_MESSAGE, BLOCKED_MESSAGE]
        );
    }

    #[tokio::test]
    async fn a_system_message_that_mentions_an_agent_opens_no_thread() {
        let _manifest = FakeManifest::install("no-thread");
        let mut fixture = Fixture::new("blocked-no-thread");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);

        fixture.show(pane_id, FAKE_BLOCKED);

        let messages = Fixture::messages(&state_dir);
        assert!(messages[1].body.contains("@chief"));
        assert!(messages[1].mentions.is_empty());
        assert_eq!(Fixture::records(&state_dir).len(), 1);
        assert_eq!(fixture.app.running_threads.len(), 1);
    }

    #[tokio::test]
    async fn a_block_that_ends_in_idle_without_work_keeps_the_thread_running() {
        let _manifest = FakeManifest::install("idle");
        let mut fixture = Fixture::new("blocked-idle");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);

        fixture.show(pane_id, FAKE_BLOCKED);
        assert_eq!(
            fixture.show(pane_id, FAKE_IDLE),
            crate::detect::AgentState::Idle
        );

        assert!(fixture.app.running_threads.contains_key(&pane_id));
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Active);
        assert_eq!(fixture.listed_status(), "idle", "the highlight is gone");
        assert_eq!(Fixture::system_bodies(&state_dir), [BLOCKED_MESSAGE]);

        fixture.show(pane_id, FAKE_WORKING);
        fixture.show(pane_id, FAKE_IDLE);

        assert!(fixture.app.running_threads.is_empty());
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);
    }

    #[tokio::test]
    async fn a_cli_that_exits_while_blocked_fails_and_says_so() {
        let _manifest = FakeManifest::install("exit");
        let mut fixture = Fixture::new("blocked-exit");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.show(pane_id, FAKE_WORKING);
        fixture.show(pane_id, FAKE_BLOCKED);

        fixture.exit(pane_id);

        let record = &Fixture::records(&state_dir)[0];
        assert_eq!(record.state, ThreadState::Failed);
        let reason = "the agent CLI exited while it waited for approval";
        assert_eq!(record.end_reason.as_deref(), Some(reason));
        assert_eq!(fixture.listed_status(), Value::Null);
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            [
                BLOCKED_MESSAGE.to_owned(),
                format!("a thread de @chief falhou: {reason}")
            ]
        );
        let agent_posts = Fixture::messages(&state_dir)
            .iter()
            .filter(|message| message.author.kind == AuthorKind::Agent)
            .count();
        assert_eq!(agent_posts, 0, "a failed turn captures no reply");
    }

    #[tokio::test]
    async fn a_cli_that_exits_before_any_work_fails_and_says_so() {
        let mut fixture = Fixture::new("exit-no-work");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);

        fixture.exit(pane_id);

        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Failed);
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            ["a thread de @chief falhou: the agent CLI exited before it did any work"]
        );
    }

    #[tokio::test]
    async fn closing_the_pane_of_a_blocked_thread_fails_it_and_clears_the_highlight() {
        let _manifest = FakeManifest::install("close");
        let mut fixture = Fixture::new("blocked-close");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.show(pane_id, FAKE_BLOCKED);
        let public_pane_id = fixture.public_pane_id(pane_id);

        let closed = fixture.request("pane.close", json!({"pane_id": public_pane_id}));

        assert!(closed.get("error").is_none(), "{closed}");
        fixture.app.state.assert_invariants_for_test();
        let record = &Fixture::records(&state_dir)[0];
        assert_eq!(record.state, ThreadState::Failed);
        assert_eq!(record.end_reason.as_deref(), Some(PANE_CLOSED));
        assert_eq!(fixture.listed_status(), Value::Null);
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            [
                BLOCKED_MESSAGE.to_owned(),
                format!("a thread de @chief falhou: {PANE_CLOSED}")
            ]
        );
    }

    #[tokio::test]
    async fn a_handoff_interrupts_a_blocked_thread_once() {
        let _manifest = FakeManifest::install("handoff");
        let mut fixture = Fixture::new("blocked-handoff");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        fixture.show(pane_id, FAKE_BLOCKED);

        fixture.app.interrupt_running_threads();

        fixture.app.state.assert_invariants_for_test();
        assert_eq!(
            Fixture::records(&state_dir)[0].state,
            ThreadState::Interrupted
        );
        assert_eq!(fixture.listed_status(), Value::Null);
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            [
                BLOCKED_MESSAGE.to_owned(),
                format!("a thread de @chief foi interrompida: {SERVER_HANDOFF}")
            ]
        );
    }

    #[tokio::test]
    async fn a_new_server_shows_no_highlight_and_repeats_no_message_of_a_blocked_thread() {
        let _manifest = FakeManifest::install("restart");
        let mut old = Fixture::new("blocked-restart-old");
        let (ws_idx, state_dir, _) = old.channel_workspace();
        let pane_id = old.thread(ws_idx, &state_dir);
        old.show(pane_id, FAKE_BLOCKED);
        let folder = state_dir.parent().unwrap().to_path_buf();

        // A cold restart: the old server dies without a handoff and handles
        // no more events, and a new one opens the same folder.
        shutdown_test_runtimes(&mut old.app);
        let mut new = Fixture::new("blocked-restart-new");
        let added = new.request(
            "workspace.add",
            json!({"path": folder.display().to_string(), "focus": true}),
        );
        assert_eq!(added["result"]["type"], "workspace_added", "{added}");
        let ws_idx = new.app.state.active.unwrap();
        new.app.state.assert_invariants_for_test();

        let indexed = new.app.workspace_thread_infos(ws_idx);
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].agent_status, None, "no stuck highlight");
        assert_eq!(
            indexed[0].state,
            crate::api::schema::ThreadState::Interrupted
        );
        assert_eq!(new.listed_status(), Value::Null);
        assert!(new.app.running_threads.is_empty());
        let interrupted = format!("a thread de @chief foi interrompida: {SERVER_RESTARTED}");
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            [BLOCKED_MESSAGE.to_owned(), interrupted.clone()]
        );

        // A later folder check says nothing again.
        new.app.refresh_folder_connections();
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            [BLOCKED_MESSAGE.to_owned(), interrupted]
        );
    }

    #[tokio::test]
    async fn a_cli_that_cannot_start_fails_with_a_system_message() {
        let mut fixture = Fixture::new("spawn-fails");
        let (ws_idx, state_dir, workspace_id) = fixture.channel_workspace();
        let not_executable = fixture.base.0.join("fake-cli");
        std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();
        let cases = [
            "regents-fake-cli-not-in-path".to_owned(),
            not_executable.display().to_string(),
        ];

        for (index, program) in cases.iter().enumerate() {
            fixture.app.test_thread_prefix = Some(vec![program.clone()]);
            fixture.mention(&workspace_id, "geral", "@chief faça");
            fixture.app.state.assert_invariants_for_test();

            let record = &Fixture::records(&state_dir)[index];
            assert_eq!(record.state, ThreadState::Failed, "{program}");
            let reason = record.end_reason.clone().unwrap();
            assert!(
                reason.starts_with(&format!("cannot start {program}")),
                "{reason}"
            );
            let messages = Fixture::messages(&state_dir);
            let system = messages.last().unwrap();
            assert_eq!(system.author.kind, AuthorKind::System);
            assert_eq!(
                system.body,
                format!("a thread de @chief não iniciou: {reason}")
            );
            assert_eq!(system.origin_thread_id, Some(record.id.clone()));
        }
        assert!(fixture.app.running_threads.is_empty());
        assert!(fixture.app.state.workspaces[ws_idx]
            .background_panes
            .is_empty());
    }

    #[tokio::test]
    async fn a_mention_of_an_unknown_agent_posts_a_system_message_and_opens_no_thread() {
        let mut fixture = Fixture::new("unknown-mention");
        let (_, state_dir, workspace_id) = fixture.channel_workspace();

        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "geral",
                   "body": "@ninguem e @outro, olá; rafael@chief.com"}),
        );

        assert!(posted.get("error").is_none(), "{posted}");
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            ["@ninguem, @outro não são agentes deste workspace"]
        );
        assert!(Fixture::records(&state_dir).is_empty());
        assert!(fixture.app.running_threads.is_empty());
        fixture.app.state.assert_invariants_for_test();

        fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "geral", "body": "@ninguem?"}),
        );
        assert_eq!(
            Fixture::system_bodies(&state_dir)
                .last()
                .map(String::as_str),
            Some("@ninguem não é um agente deste workspace")
        );
    }

    #[tokio::test]
    async fn a_mention_of_an_agent_without_a_thread_adapter_posts_a_system_message() {
        let mut fixture = Fixture::new("no-adapter");
        let (_, state_dir, workspace_id) = fixture.channel_workspace();
        std::fs::write(
            state_dir.join("agents").join("coder.md"),
            "+++\nname = \"coder\"\ncli = \"codex\"\n+++\n",
        )
        .unwrap();

        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "geral", "body": "@coder faça"}),
        );

        assert!(posted.get("error").is_none(), "{posted}");
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            ["a thread de @coder não iniciou: the codex CLI has no thread adapter yet"]
        );
        assert!(Fixture::records(&state_dir).is_empty());
        assert!(fixture.app.running_threads.is_empty());
        fixture.app.state.assert_invariants_for_test();
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

        let authors: Vec<AuthorKind> = Fixture::messages(&state_dir)
            .iter()
            .map(|message| message.author.kind)
            .collect();
        // The failed thread says so as the Regents; no thread reply is posted.
        assert_eq!(authors, [AuthorKind::Human, AuthorKind::System]);
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
        let messages = Fixture::messages(&state_dir);
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[1].author.kind, AuthorKind::System);
        assert_eq!(
            messages[1].body,
            format!("a thread de @chief foi interrompida: {SERVER_HANDOFF}")
        );
        assert_eq!(messages[1].origin_thread_id.as_deref(), Some("t1"));
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

    /// `capture_key` of each captured message of `geral`, in posting order.
    fn capture_keys(state_dir: &Path) -> Vec<String> {
        let path = state_dir
            .join("channels")
            .join("geral")
            .join("channel.sqlite");
        let connection = rusqlite::Connection::open(path).unwrap();
        let mut statement = connection
            .prepare("SELECT capture_key FROM messages WHERE capture_key IS NOT NULL ORDER BY id")
            .unwrap();
        let keys = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        keys
    }

    // Characterization of the first turn (#14): the capture key of a
    // transcript reply is the transcript turn.
    #[tokio::test]
    async fn the_capture_key_of_a_transcript_reply_is_its_transcript_turn() {
        let mut fixture = Fixture::new("capture-key-transcript");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let first = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("one-turn");
        fixture.finish_turn(first);
        let second = fixture.thread(ws_idx, &state_dir);
        fixture.transcript("two-turns");

        fixture.finish_turn(second);

        assert_eq!(capture_keys(&state_dir), ["t1#0", "t2#1"]);
    }

    // Characterization of the first turn (#14): without a transcript, the
    // screen reply after the last prompt echo is turn 0.
    #[tokio::test]
    async fn without_a_transcript_the_screen_reply_is_captured_as_turn_zero() {
        let mut fixture = Fixture::new("capture-key-screen");
        let (ws_idx, state_dir, _) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let terminal_id = &fixture.app.running_threads[&pane_id].terminal_id;
        fixture
            .app
            .terminal_runtimes
            .get(terminal_id)
            .unwrap()
            .test_process_pty_bytes(
                format!("{}\r\nresposta da tela\r\n", adapter::PROMPT_LAST_LINE).as_bytes(),
            );

        fixture.finish_turn(pane_id);

        let messages = Fixture::messages(&state_dir);
        assert_eq!(messages[1].body, "resposta da tela");
        assert_eq!(messages[1].origin_thread_id.as_deref(), Some("t1"));
        assert_eq!(capture_keys(&state_dir), ["t1#0"]);
    }

    // Threaded replies (#14). Each test drives `channel.post` with
    // `reply_to`; the fake CLI prefix keeps the resumed command line as its
    // arguments, so no test starts claude.

    const TURN_ZERO_REPLY: &str = "pronto, linha 1\nlinha 2";

    impl Fixture {
        /// A done thread `t1` whose turn 0 reply, message 2, was captured
        /// from its transcript.
        fn done_thread(&mut self) -> (usize, PathBuf, String) {
            let (ws_idx, state_dir, workspace_id) = self.channel_workspace();
            let pane_id = self.thread(ws_idx, &state_dir);
            self.transcript("one-turn");
            self.finish_turn(pane_id);
            let messages = Self::messages(&state_dir);
            assert_eq!(messages[1].id, 2);
            assert_eq!(messages[1].origin_thread_id.as_deref(), Some("t1"));
            assert_eq!(Self::records(&state_dir)[0].state, ThreadState::Done);
            (ws_idx, state_dir, workspace_id)
        }

        /// A human post of `body` that answers the message `reply_to`; its
        /// message ID.
        fn reply(&mut self, workspace_id: &str, reply_to: u64, body: &str) -> u64 {
            let posted = self.request(
                "channel.post",
                json!({"workspace_id": workspace_id, "channel": "geral", "body": body,
                       "reply_to": reply_to}),
            );
            assert!(posted.get("error").is_none(), "{posted}");
            self.app.state.assert_invariants_for_test();
            posted["result"]["message"]["id"].as_u64().unwrap()
        }

        /// The pane of the running thread `thread_id`.
        fn running_pane(&self, thread_id: &str) -> PaneId {
            self.app
                .running_threads
                .iter()
                .find(|(_, thread)| thread.thread_id == thread_id)
                .map(|(&pane_id, _)| pane_id)
                .unwrap_or_else(|| panic!("{thread_id} is not running"))
        }

        /// The thread command line that the pane of `thread_id` runs, without
        /// the fake CLI prefix.
        fn running_argv(&self, thread_id: &str) -> Vec<String> {
            let pane_id = self.running_pane(thread_id);
            let terminal_id = &self.app.running_threads[&pane_id].terminal_id;
            let argv = self.app.state.terminals[terminal_id]
                .launch_argv
                .clone()
                .unwrap();
            let prefix = self.app.test_thread_prefix.as_ref().map_or(0, Vec::len);
            argv[prefix..].to_vec()
        }

        fn resumed_events(&self) -> Vec<EventData> {
            self.app
                .event_hub
                .events_after(0)
                .into_iter()
                .filter(|(_, event)| event.event == EventKind::ThreadResumed)
                .map(|(_, event)| event.data)
                .collect()
        }

        fn agent_file(state_dir: &Path, front_matter: &str) {
            std::fs::write(
                state_dir.join("agents").join("chief-of-staff.md"),
                format!("+++\n{front_matter}\n+++\nVocê é o Chief of Staff.\n"),
            )
            .unwrap();
        }

        fn sql(state_dir: &Path, statement: &str) {
            rusqlite::Connection::open(state_dir.join("threads.sqlite"))
                .unwrap()
                .execute_batch(statement)
                .unwrap();
        }
    }

    /// Asserts that `reply` resumed nothing and said `refusal` in the
    /// channel.
    fn assert_refused(fixture: &Fixture, state_dir: &Path, refusal: &str) {
        assert_eq!(
            Fixture::system_bodies(state_dir).last().map(String::as_str),
            Some(refusal)
        );
        let records = Fixture::records(state_dir);
        assert_eq!(records.len(), 1, "no new thread: {records:?}");
        assert_eq!(records[0].turn, 0);
        assert!(fixture.resumed_events().is_empty());
        let system = Fixture::messages(state_dir).pop().unwrap();
        assert_eq!(system.origin_thread_id.as_deref(), Some("t1"));
        assert!(system.mentions.is_empty(), "a refusal opens no thread");
    }

    // AC1: same thread ID, the session resumed by the adapter, no new thread.
    #[tokio::test]
    async fn a_reply_to_a_done_thread_resumes_the_same_thread_and_session() {
        let mut fixture = Fixture::new("resume");
        let (ws_idx, state_dir, workspace_id) = fixture.done_thread();

        let reply = fixture.reply(&workspace_id, 2, "e o resto?");

        let records = Fixture::records(&state_dir);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "t1");
        assert_eq!(records[0].state, ThreadState::Active);
        assert_eq!(records[0].turn, 1);
        assert_eq!(records[0].turn_message_id, Some(reply));
        assert_eq!(records[0].session_id.as_deref(), Some("sid"));
        let argv = fixture.running_argv("t1");
        assert_eq!(argv[..3], ["claude", "--resume", "sid"], "{argv:?}");
        let prompt = argv.last().unwrap();
        assert!(prompt.contains("e o resto?"), "{prompt}");
        assert!(
            !prompt.contains("pergunta"),
            "only the new messages: {prompt}"
        );
        assert!(!prompt.contains(TURN_ZERO_REPLY), "{prompt}");
        assert_eq!(
            fixture.resumed_events(),
            [EventData::ThreadResumed {
                workspace_id: workspace_id.clone(),
                thread_id: "t1".into(),
                turn: 1,
                message_id: reply,
            }]
        );
        let order: Vec<EventKind> = fixture
            .app
            .event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::ChannelMessagePosted | EventKind::ThreadResumed
                )
            })
            .collect();
        assert_eq!(
            order[order.len() - 2..],
            [EventKind::ChannelMessagePosted, EventKind::ThreadResumed],
            "the reply comes before the resume it causes"
        );
        let listed = fixture.request("thread.list", json!({"workspace_id": workspace_id}));
        assert_eq!(listed["result"]["threads"][0]["turn"], 1, "{listed}");
        assert_eq!(
            listed["result"]["threads"][0]["state"], "active",
            "{listed}"
        );
        assert_eq!(fixture.app.workspace_thread_infos(ws_idx)[0].turn, 1);
        assert!(Fixture::system_bodies(&state_dir).is_empty());
    }

    // The turn 1 reply posts once, after the reply; turn 0 is not posted
    // again.
    #[tokio::test]
    async fn the_resumed_turn_posts_its_own_reply_once() {
        let mut fixture = Fixture::new("resume-capture");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        let reply = fixture.reply(&workspace_id, 2, "e o resto?");
        let pane_id = fixture.running_pane("t1");
        fixture.transcript("two-turns");

        fixture.finish_turn(pane_id);

        let messages = Fixture::messages(&state_dir);
        let bodies: Vec<&str> = messages
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(
            bodies,
            [
                "pergunta",
                TURN_ZERO_REPLY,
                "e o resto?",
                "segundo turno @reviewer"
            ]
        );
        assert!(messages[3].id > reply);
        assert_eq!(capture_keys(&state_dir), ["t1#0", "t1#1"]);
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);
        assert_eq!(Fixture::records(&state_dir).len(), 1);
    }

    #[tokio::test]
    async fn a_resumed_turn_whose_transcript_has_no_new_turn_posts_nothing_again() {
        let mut fixture = Fixture::new("resume-stale");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        fixture.reply(&workspace_id, 2, "e o resto?");
        let pane_id = fixture.running_pane("t1");

        // The transcript still ends with turn 0.
        fixture.finish_turn(pane_id);

        let replies = Fixture::messages(&state_dir)
            .iter()
            .filter(|message| message.body == TURN_ZERO_REPLY)
            .count();
        assert_eq!(replies, 1);
        assert_eq!(capture_keys(&state_dir), ["t1#0"]);
    }

    #[tokio::test]
    async fn without_a_transcript_the_resumed_screen_reply_has_the_registry_turn() {
        let mut fixture = Fixture::new("resume-screen");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        fixture.reply(&workspace_id, 2, "e o resto?");
        let pane_id = fixture.running_pane("t1");
        std::fs::remove_dir_all(fixture.base.0.join("claude")).unwrap();
        let terminal_id = &fixture.app.running_threads[&pane_id].terminal_id;
        // claude 2.1.332 shows the old turn and its prompt echo on resume.
        let screen = format!(
            "{last}\r\nresposta antiga\r\n{last}\r\nresposta nova\r\n",
            last = adapter::PROMPT_LAST_LINE
        );
        fixture
            .app
            .terminal_runtimes
            .get(terminal_id)
            .unwrap()
            .test_process_pty_bytes(screen.as_bytes());

        fixture.finish_turn(pane_id);

        let last = Fixture::messages(&state_dir).pop().unwrap();
        assert_eq!(last.body, "resposta nova");
        assert_eq!(capture_keys(&state_dir), ["t1#0", "t1#1"]);
    }

    // AC2: after the resume, a loose mention opens a new thread.
    #[tokio::test]
    async fn a_loose_mention_after_a_resume_opens_a_new_thread() {
        let mut fixture = Fixture::new("resume-then-mention");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        fixture.reply(&workspace_id, 2, "e o resto?");

        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "geral", "body": "@chief outra coisa"}),
        );

        assert!(posted.get("error").is_none(), "{posted}");
        fixture.app.state.assert_invariants_for_test();
        let ids: Vec<String> = Fixture::records(&state_dir)
            .into_iter()
            .map(|record| record.id)
            .collect();
        assert_eq!(ids, ["t1", "t2"]);
        assert_eq!(fixture.running_argv("t2")[1], "--session-id");
    }

    // AC3: a reply to a human message resumes no thread and routes normally.
    #[tokio::test]
    async fn a_reply_to_a_human_message_follows_the_normal_routing() {
        let mut fixture = Fixture::new("reply-human");
        let (_, state_dir, workspace_id) = fixture.done_thread();

        fixture.reply(&workspace_id, 1, "sem menção");
        assert_eq!(Fixture::records(&state_dir).len(), 1);
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);
        assert!(fixture.app.running_threads.is_empty());

        fixture.reply(&workspace_id, 1, "@chief com menção");
        let records = Fixture::records(&state_dir);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].state, ThreadState::Done, "t1 stays done");
        assert_eq!(records[1].state, ThreadState::Active);
        assert!(fixture.resumed_events().is_empty());
        assert!(Fixture::system_bodies(&state_dir).is_empty());
    }

    // Table: a reply to a reply. A reply to the human reply routes normally;
    // a reply to the agent reply of turn 1 resumes turn 2.
    #[tokio::test]
    async fn a_reply_to_a_human_reply_routes_normally_and_to_an_agent_reply_resumes_again() {
        let mut fixture = Fixture::new("reply-chain");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        let first = fixture.reply(&workspace_id, 2, "e o resto?");
        fixture.transcript("two-turns");
        fixture.finish_turn(fixture.running_pane("t1"));
        let turn_one = Fixture::messages(&state_dir).pop().unwrap();
        assert_eq!(turn_one.origin_thread_id.as_deref(), Some("t1"));

        fixture.reply(&workspace_id, first, "complemento");
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);

        let second = fixture.reply(&workspace_id, turn_one.id, "e agora?");
        let record = &Fixture::records(&state_dir)[0];
        assert_eq!(record.state, ThreadState::Active);
        assert_eq!(record.turn, 2);
        assert_eq!(record.turn_message_id, Some(second));
        let prompt = fixture.running_argv("t1").pop().unwrap();
        assert!(prompt.contains("complemento"), "new since turn 1: {prompt}");
        assert!(!prompt.contains("e o resto?"), "turn 1 saw it: {prompt}");
        assert!(
            !prompt.contains("segundo turno"),
            "a post of the thread: {prompt}"
        );
        assert_eq!(Fixture::records(&state_dir).len(), 1);
    }

    // Table: a reply to a system message routes normally.
    #[tokio::test]
    async fn a_reply_to_a_system_message_resumes_nothing() {
        let mut fixture = Fixture::new("reply-system");
        let (ws_idx, state_dir, workspace_id) = fixture.done_thread();
        fixture.app.post_system_message(
            Some(ws_idx),
            &state_dir,
            "geral",
            Some("t1"),
            "aviso".into(),
        );

        fixture.reply(&workspace_id, 3, "ok");

        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);
        assert_eq!(Fixture::system_bodies(&state_dir), ["aviso"]);
        assert!(fixture.resumed_events().is_empty());
    }

    // Table: a reply that mentions the answered agent and another one.
    #[tokio::test]
    async fn a_reply_that_mentions_agents_resumes_its_thread_and_opens_the_others() {
        let mut fixture = Fixture::new("reply-mentions");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        std::fs::write(
            state_dir.join("agents").join("reviewer.md"),
            "+++\nname = \"reviewer\"\ncli = \"claude\"\n+++\n",
        )
        .unwrap();

        fixture.reply(&workspace_id, 2, "@chief e @reviewer, vejam");

        let records = Fixture::records(&state_dir);
        let threads: Vec<(&str, &str, u32)> = records
            .iter()
            .map(|record| (record.id.as_str(), record.agent.as_str(), record.turn))
            .collect();
        assert_eq!(threads, [("t1", "chief", 1), ("t2", "reviewer", 0)]);
    }

    // Table: Active (and the concurrent loser).
    #[tokio::test]
    async fn a_reply_to_an_active_thread_stays_in_the_channel() {
        let mut fixture = Fixture::new("reply-active");
        let (ws_idx, state_dir, workspace_id) = fixture.channel_workspace();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let token = fixture.app.running_threads[&pane_id].token.clone();
        fixture.thread_post(&workspace_id, &token);

        fixture.reply(&workspace_id, 2, "e o resto?");

        assert_refused(
            &fixture,
            &state_dir,
            "@chief ainda está na thread t1; a resposta fica no canal",
        );
        assert_eq!(fixture.app.running_threads.len(), 1);
    }

    // Table: two replies, one resume; the second says the thread runs.
    #[tokio::test]
    async fn two_replies_to_a_done_thread_resume_it_once() {
        let mut fixture = Fixture::new("reply-twice");
        let (_, state_dir, workspace_id) = fixture.done_thread();

        fixture.reply(&workspace_id, 2, "primeira");
        fixture.reply(&workspace_id, 2, "segunda");

        let records = Fixture::records(&state_dir);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].turn, 1);
        assert_eq!(fixture.app.running_threads.len(), 1);
        assert_eq!(fixture.resumed_events().len(), 1);
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            ["@chief ainda está na thread t1; a resposta fica no canal"]
        );
    }

    // Table: Queued, Interrupted, Failed and Unknown.
    #[tokio::test]
    async fn a_reply_to_a_thread_that_is_not_done_does_not_resume_it() {
        let new_thread = "mencione @chief para abrir uma thread nova";
        let cases = [
            (
                "queued",
                "@chief ainda está na thread t1; a resposta fica no canal".to_owned(),
            ),
            (
                "interrupted",
                format!("a thread t1 de @chief foi interrompida; {new_thread}"),
            ),
            (
                "failed",
                format!("a thread t1 de @chief falhou e não é retomada; {new_thread}"),
            ),
            (
                "paused",
                "a thread t1 de @chief tem um estado desconhecido e não é retomada".to_owned(),
            ),
        ];
        for (state, refusal) in cases {
            let mut fixture = Fixture::new(&format!("reply-{state}"));
            let (_, state_dir, workspace_id) = fixture.done_thread();
            Fixture::sql(&state_dir, &format!("UPDATE threads SET state = '{state}'"));

            fixture.reply(&workspace_id, 2, "e o resto?");

            assert_refused(&fixture, &state_dir, &refusal);
            assert!(fixture.app.running_threads.is_empty(), "{state}");
        }
    }

    // Table: the agent was removed, its CLI changed or has no adapter.
    #[tokio::test]
    async fn a_reply_to_a_thread_whose_agent_cannot_resume_it_says_why() {
        let cases = [
            (None, "@chief não é mais um agente deste workspace; a thread t1 não é retomada"),
            (
                Some("name = \"chief\"\ncli = \"codex\""),
                "a thread t1 de @chief não foi retomada: the agent CLI changed after the thread started",
            ),
        ];
        for (index, (front_matter, refusal)) in cases.into_iter().enumerate() {
            let mut fixture = Fixture::new(&format!("reply-agent-{index}"));
            let (_, state_dir, workspace_id) = fixture.done_thread();
            match front_matter {
                Some(front_matter) => Fixture::agent_file(&state_dir, front_matter),
                None => std::fs::remove_file(state_dir.join("agents").join("chief-of-staff.md"))
                    .unwrap(),
            }

            fixture.reply(&workspace_id, 2, "e o resto?");

            assert_refused(&fixture, &state_dir, refusal);
        }
    }

    #[tokio::test]
    async fn a_reply_to_a_thread_of_a_cli_without_an_adapter_says_so() {
        let mut fixture = Fixture::new("reply-no-adapter");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        Fixture::sql(&state_dir, "UPDATE threads SET cli = 'codex'");
        Fixture::agent_file(&state_dir, "name = \"chief\"\ncli = \"codex\"");

        fixture.reply(&workspace_id, 2, "e o resto?");

        assert_refused(
            &fixture,
            &state_dir,
            "a thread t1 de @chief não foi retomada: the codex CLI has no thread adapter yet",
        );
    }

    // Table: the transcript is gone, or the thread has no session.
    #[tokio::test]
    async fn a_reply_to_a_thread_without_a_transcript_says_so() {
        for (index, fault) in ["transcript", "session"].into_iter().enumerate() {
            let mut fixture = Fixture::new(&format!("reply-no-session-{index}"));
            let (_, state_dir, workspace_id) = fixture.done_thread();
            match fault {
                "transcript" => std::fs::remove_dir_all(fixture.base.0.join("claude")).unwrap(),
                _ => Fixture::sql(&state_dir, "UPDATE threads SET session_id = NULL"),
            }

            fixture.reply(&workspace_id, 2, "e o resto?");

            assert_refused(
                &fixture,
                &state_dir,
                "a thread t1 de @chief não foi retomada: the CLI session has no transcript",
            );
        }
    }

    // Table: a worktree thread (#24), or one of an unknown isolation.
    #[tokio::test]
    async fn a_reply_to_a_worktree_thread_does_not_resume_it_yet() {
        let cases = [
            ("'worktree'", "worktree threads cannot resume yet"),
            ("NULL", "the thread isolation is unknown"),
        ];
        for (index, (isolation, reason)) in cases.into_iter().enumerate() {
            let mut fixture = Fixture::new(&format!("reply-worktree-{index}"));
            let (_, state_dir, workspace_id) = fixture.done_thread();
            Fixture::sql(
                &state_dir,
                &format!("UPDATE threads SET isolation = {isolation}"),
            );

            fixture.reply(&workspace_id, 2, "e o resto?");

            assert_refused(
                &fixture,
                &state_dir,
                &format!("a thread t1 de @chief não foi retomada: {reason}"),
            );
        }
    }

    // A refused reply that mentions the agent of an ended thread opens a
    // new thread, as the refusal says.
    #[tokio::test]
    async fn a_mention_in_a_reply_to_an_ended_thread_opens_a_new_thread() {
        let mut fixture = Fixture::new("reply-failed-mention");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        Fixture::sql(&state_dir, "UPDATE threads SET state = 'failed'");

        fixture.reply(&workspace_id, 2, "@chief tenta de novo");

        let records = Fixture::records(&state_dir);
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[0].state, ThreadState::Failed);
        assert_eq!((records[1].id.as_str(), records[1].turn), ("t2", 0));
        assert_eq!(fixture.running_argv("t2")[1], "--session-id");
    }

    // Message IDs are per channel: a reply in another channel than the one
    // of the thread does not resume it.
    #[tokio::test]
    async fn a_reply_in_another_channel_than_the_thread_does_not_resume_it() {
        let mut fixture = Fixture::new("reply-other-channel");
        let (ws_idx, state_dir, workspace_id) = fixture.channel_workspace();
        fixture.request(
            "channel.create",
            json!({"workspace_id": workspace_id, "channel": "dev"}),
        );
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let token = fixture.app.running_threads[&pane_id].token.clone();
        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "dev", "body": "em dev",
                   "origin_thread_id": "t1", "thread_token": token}),
        );
        assert_eq!(posted["result"]["message"]["id"], 1, "{posted}");
        fixture.finish_turn(pane_id);

        let replied = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "dev", "body": "e o resto?",
                   "reply_to": 1}),
        );

        assert!(replied.get("error").is_none(), "{replied}");
        fixture.app.state.assert_invariants_for_test();
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);
        assert_eq!(Fixture::records(&state_dir)[0].turn, 0);
        let dev = channel::open(&state_dir, "dev")
            .unwrap()
            .read(ReadRange::default())
            .unwrap();
        assert_eq!(
            dev.last().map(|message| message.body.as_str()),
            Some(
                "a thread t1 de @chief não foi retomada: the thread belongs to #geral; reply there"
            )
        );
    }

    // Table: the resumed CLI does not start; the thread stays resumable.
    #[tokio::test]
    async fn a_resume_whose_cli_cannot_start_leaves_the_thread_done() {
        let mut fixture = Fixture::new("reply-spawn-fails");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        let prefix = fixture
            .app
            .test_thread_prefix
            .replace(vec!["regents-fake-cli-not-in-path".into()]);

        fixture.reply(&workspace_id, 2, "primeira");

        let refusal = Fixture::system_bodies(&state_dir).pop().unwrap();
        assert!(
            refusal.starts_with(
                "a thread t1 de @chief não foi retomada: cannot start regents-fake-cli-not-in-path"
            ),
            "{refusal}"
        );
        let record = &Fixture::records(&state_dir)[0];
        assert_eq!(record.state, ThreadState::Done);
        assert_eq!((record.turn, record.turn_message_id), (0, Some(1)));
        assert!(fixture.app.running_threads.is_empty());
        assert!(fixture.resumed_events().is_empty());

        fixture.app.test_thread_prefix = prefix;
        fixture.reply(&workspace_id, 2, "segunda");

        assert_eq!(Fixture::records(&state_dir)[0].turn, 1);
        let prompt = fixture.running_argv("t1").pop().unwrap();
        assert!(
            prompt.contains("primeira"),
            "the failed reply is new: {prompt}"
        );
    }

    // Table: the answered message names a thread that the registry lacks.
    #[tokio::test]
    async fn a_reply_to_a_message_of_an_unknown_thread_says_so() {
        let mut fixture = Fixture::new("reply-unknown-thread");
        let (_, state_dir, workspace_id) = fixture.done_thread();
        channel::open(&state_dir, "geral")
            .unwrap()
            .post(
                NewMessage {
                    author: Author {
                        kind: AuthorKind::Agent,
                        id: "chief".into(),
                    },
                    body: "de outra thread".into(),
                    reply_to: None,
                    origin_thread_id: Some("t9".into()),
                    mentions: Vec::new(),
                },
                time::OffsetDateTime::now_utc(),
            )
            .unwrap();

        fixture.reply(&workspace_id, 3, "e o resto?");

        let system = Fixture::messages(&state_dir).pop().unwrap();
        assert_eq!(system.body, "a thread t9 não existe neste workspace");
        assert_eq!(system.origin_thread_id, None, "no link to a missing thread");
        assert_eq!(Fixture::records(&state_dir)[0].state, ThreadState::Done);
    }

    // Table: a resume takes the same queue as a mention (#16), and keeps
    // the delegation depth.
    #[tokio::test]
    async fn a_resume_waits_in_the_queue_and_keeps_the_depth() {
        let mut fixture = Fixture::new("reply-limit-depth");
        let (ws_idx, state_dir, workspace_id) = fixture.done_thread();
        Fixture::set_limit(&state_dir, 1);
        let holder = fixture.thread(ws_idx, &state_dir);
        Fixture::sql(&state_dir, "UPDATE threads SET depth = 2 WHERE id = 1");

        let reply = fixture.reply(&workspace_id, 2, "e o resto?");

        let record = &Fixture::records(&state_dir)[0];
        assert_eq!(record.state, ThreadState::Queued);
        assert_eq!((record.turn, record.queue_position), (0, Some(1)));
        assert_eq!(fixture.app.running_threads.len(), 1);
        assert!(fixture.resumed_events().is_empty(), "no turn started yet");
        assert!(Fixture::system_bodies(&state_dir).is_empty());

        fixture.finish_turn(holder);

        let record = &Fixture::records(&state_dir)[0];
        assert_eq!(record.state, ThreadState::Active);
        assert_eq!((record.turn, record.turn_message_id), (1, Some(reply)));
        assert_eq!(record.depth, 2);
        assert_eq!(
            fixture.running_argv("t1")[..3],
            ["claude", "--resume", "sid"]
        );
        let prompt = fixture.running_argv("t1").pop().unwrap();
        assert!(prompt.contains("e o resto?"), "{prompt}");
        assert_eq!(
            fixture.resumed_events(),
            [EventData::ThreadResumed {
                workspace_id,
                thread_id: "t1".into(),
                turn: 1,
                message_id: reply,
            }]
        );
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // Table: a resume while the agent has another queued thread in the
    // channel; at most one queued thread per agent and channel.
    #[tokio::test]
    async fn a_resume_is_refused_while_its_agent_has_a_queued_thread_in_the_channel() {
        let mut fixture = Fixture::new("reply-pair-queued");
        let (ws_idx, state_dir, workspace_id) = fixture.done_thread();
        Fixture::set_limit(&state_dir, 1);
        fixture.thread(ws_idx, &state_dir);
        fixture.mention(&workspace_id, "geral", "@chief outra coisa");
        assert_eq!(Fixture::records(&state_dir)[2].state, ThreadState::Queued);

        fixture.reply(&workspace_id, 2, "e o resto?");

        let records = Fixture::records(&state_dir);
        assert_eq!(records.len(), 3);
        assert_eq!((records[0].state, records[0].turn), (ThreadState::Done, 0));
        assert_eq!(
            Fixture::system_bodies(&state_dir)
                .last()
                .map(String::as_str),
            Some("@chief já tem a thread t3 na fila; a resposta fica no canal")
        );
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // Table: a queued resume whose CLI cannot start is done again in its
    // previous turn, and the next queued thread starts.
    #[tokio::test]
    async fn a_queued_resume_that_cannot_start_is_done_again_and_the_queue_goes_on() {
        let mut fixture = Fixture::new("reply-queued-spawn-fails");
        let (ws_idx, state_dir, workspace_id) = fixture.done_thread();
        Fixture::agent(&state_dir, "reviewer");
        Fixture::set_limit(&state_dir, 1);
        let holder = fixture.thread(ws_idx, &state_dir);
        fixture.reply(&workspace_id, 2, "e o resto?");
        fixture.mention(&workspace_id, "geral", "@reviewer veja");
        // The agent of the resume leaves before its turn leaves the queue.
        std::fs::remove_file(state_dir.join("agents").join("chief-of-staff.md")).unwrap();
        let public_pane_id = fixture.public_pane_id(holder);

        fixture.request("pane.close", json!({"pane_id": public_pane_id}));

        let records = Fixture::records(&state_dir);
        assert_eq!((records[0].state, records[0].turn), (ThreadState::Done, 0));
        assert_eq!(records[0].turn_message_id, Some(1));
        assert!(
            Fixture::system_bodies(&state_dir).contains(
                &"a thread t1 de @chief não foi retomada: @chief is no longer an agent of the workspace"
                    .to_owned()
            ),
            "{:?}",
            Fixture::system_bodies(&state_dir)
        );
        assert!(fixture.resumed_events().is_empty());
        assert_eq!(records[2].state, ThreadState::Active, "the next one starts");
        fixture.running_pane("t3");
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // Table: a reply posted by a thread resumes nothing (#18).
    #[tokio::test]
    async fn a_reply_posted_by_a_thread_resumes_nothing() {
        let mut fixture = Fixture::new("reply-from-thread");
        let (ws_idx, state_dir, workspace_id) = fixture.done_thread();
        let pane_id = fixture.thread(ws_idx, &state_dir);
        let token = fixture.app.running_threads[&pane_id].token.clone();

        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "geral", "body": "de t2",
                   "reply_to": 2, "origin_thread_id": "t2", "thread_token": token}),
        );

        assert!(posted.get("error").is_none(), "{posted}");
        let records = Fixture::records(&state_dir);
        assert_eq!(records[0].state, ThreadState::Done);
        assert_eq!(records[0].turn, 0);
        assert!(Fixture::system_bodies(&state_dir).is_empty());
    }

    // Identity: with adversarial pane, tab and workspace IDs before it, the
    // resumed thread runs in the hidden panes of its own workspace.
    #[tokio::test]
    async fn a_resume_keeps_the_identity_invariants_of_an_adversarial_state() {
        let mut fixture = Fixture::new("reply-adversarial");
        fixture.app.state = crate::app::AppState::test_with_adversarial_identity_state();
        fixture.app.state.assert_invariants_for_test();
        let (ws_idx, _, workspace_id) = fixture.done_thread();
        assert_ne!(ws_idx, 0);

        fixture.reply(&workspace_id, 2, "e o resto?");

        fixture.app.state.assert_invariants_for_test();
        let pane_id = fixture.running_pane("t1");
        assert_eq!(
            fixture.app.find_pane(pane_id).map(|(index, _)| index),
            Some(ws_idx)
        );
        assert!(fixture.app.state.workspaces[ws_idx]
            .background_panes
            .contains_key(&pane_id));
        let listed = fixture.request("thread.list", json!({"workspace_id": workspace_id}));
        assert_eq!(
            listed["result"]["threads"][0]["pane_id"],
            fixture.public_pane_id(pane_id),
            "{listed}"
        );
        let events = fixture.resumed_events();
        assert!(
            matches!(&events[..], [EventData::ThreadResumed { workspace_id: id, .. }] if *id == workspace_id),
            "{events:?}"
        );
        fixture.finish_turn(pane_id);
        fixture.app.state.assert_invariants_for_test();
        assert!(fixture.app.state.workspaces[ws_idx]
            .background_panes
            .is_empty());
    }

    // The thread queue (#16). Each test drives mentions through
    // `channel.post`; the fake CLI prefix makes every thread a long sleep.

    impl Fixture {
        /// Writes `max_active_threads = limit` in `workspace.toml`.
        fn set_limit(state_dir: &Path, limit: u32) {
            let path = state_dir.join("workspace.toml");
            let text = std::fs::read_to_string(&path).unwrap();
            let line = text
                .lines()
                .find(|line| line.starts_with("max_active_threads"))
                .unwrap()
                .to_owned();
            std::fs::write(
                &path,
                text.replace(&line, &format!("max_active_threads = {limit}")),
            )
            .unwrap();
        }

        /// A claude agent `name` in the workspace.
        fn agent(state_dir: &Path, name: &str) {
            std::fs::write(
                state_dir.join("agents").join(format!("{name}.md")),
                format!("+++\nname = \"{name}\"\ncli = \"claude\"\n+++\n"),
            )
            .unwrap();
        }

        /// A human post of `body` in `channel`; its message ID.
        fn mention(&mut self, workspace_id: &str, channel: &str, body: &str) -> u64 {
            let posted = self.request(
                "channel.post",
                json!({"workspace_id": workspace_id, "channel": channel, "body": body}),
            );
            assert!(posted.get("error").is_none(), "{posted}");
            posted["result"]["message"]["id"].as_u64().unwrap()
        }

        /// IDs of the active threads, and of the queued ones in queue order.
        fn queue(state_dir: &Path) -> (Vec<String>, Vec<String>) {
            let records = Self::records(state_dir);
            let active = records
                .iter()
                .filter(|record| record.state == ThreadState::Active)
                .map(|record| record.id.clone())
                .collect();
            let mut queued: Vec<&threads::ThreadRecord> = records
                .iter()
                .filter(|record| record.state == ThreadState::Queued)
                .collect();
            queued.sort_by_key(|record| record.queue_position);
            (
                active,
                queued.iter().map(|record| record.id.clone()).collect(),
            )
        }

        /// The invariants of the queue of `state_dir` with the limit
        /// `limit`, and the identity invariants of the app.
        fn assert_queue_invariants(&self, state_dir: &Path, limit: usize) {
            self.app.state.assert_invariants_for_test();
            let records = Self::records(state_dir);
            let (active, queued) = Self::queue(state_dir);
            let mut running: Vec<String> = self
                .app
                .running_threads
                .values()
                .filter(|thread| thread.state_dir == state_dir)
                .map(|thread| thread.thread_id.clone())
                .collect();
            running.sort_by_key(|id| id[1..].parse::<u32>().unwrap());
            assert_eq!(
                running, active,
                "a pane runs each active thread, and only those"
            );
            assert!(queued.iter().all(|id| !running.contains(id)));
            let positions: Vec<Option<u32>> = records
                .iter()
                .filter(|record| record.state == ThreadState::Queued)
                .map(|record| record.queue_position)
                .collect();
            let mut sorted = positions.clone();
            sorted.sort();
            let expected: Vec<Option<u32>> = (1..=positions.len() as u32).map(Some).collect();
            assert_eq!(sorted, expected, "queue positions");
            let mut pairs: Vec<(String, Option<String>)> = records
                .iter()
                .filter(|record| record.state == ThreadState::Queued)
                .map(|record| (record.agent.clone(), record.channel.clone()))
                .collect();
            let count = pairs.len();
            pairs.sort();
            pairs.dedup();
            assert_eq!(
                pairs.len(),
                count,
                "one queued thread per agent and channel"
            );
            if !queued.is_empty() {
                assert!(active.len() >= limit, "a free slot while {queued:?} waits");
            }
        }
    }

    /// A workspace with the agents chief and reviewer, the limit `limit`,
    /// and the channel `geral`.
    fn queue_workspace(fixture: &mut Fixture, limit: u32) -> (usize, PathBuf, String) {
        let (ws_idx, state_dir, workspace_id) = fixture.channel_workspace();
        Fixture::agent(&state_dir, "reviewer");
        Fixture::set_limit(&state_dir, limit);
        (ws_idx, state_dir, workspace_id)
    }

    // AC1, AC2, AC4: with limit 1 the second mention waits, `thread.list`
    // and the index show it queued with its place, and it starts at done.
    #[tokio::test]
    async fn with_limit_one_the_second_mention_waits_and_starts_at_done() {
        let mut fixture = Fixture::new("queue-done");
        let (ws_idx, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);

        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@reviewer dois");

        assert_eq!(
            Fixture::queue(&state_dir),
            (vec!["t1".into()], vec!["t2".into()])
        );
        let listed = fixture.request("thread.list", json!({"workspace_id": workspace_id}));
        assert_eq!(
            listed["result"]["threads"][1]["state"], "queued",
            "{listed}"
        );
        assert_eq!(
            listed["result"]["threads"][1]["queue_position"], 1,
            "{listed}"
        );
        assert_eq!(listed["result"]["threads"][0].get("queue_position"), None);
        let indexed = fixture.app.workspace_thread_infos(ws_idx);
        assert_eq!(indexed[0].state, crate::api::schema::ThreadState::Queued);
        assert_eq!(indexed[0].queue_position, Some(1));
        fixture.assert_queue_invariants(&state_dir, 1);

        fixture.finish_turn(fixture.running_pane("t1"));

        assert_eq!(Fixture::queue(&state_dir), (vec!["t2".into()], vec![]));
        let prompt = fixture.running_argv("t2").pop().unwrap();
        assert!(prompt.contains("@reviewer dois"), "{prompt}");
        assert_eq!(fixture.running_argv("t2")[1], "--session-id");
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // Absorption: one queued thread per agent and channel; the anchor moves,
    // the origin stays, and the prompt is made at the exit from the queue.
    #[tokio::test]
    async fn a_queued_thread_absorbs_a_new_mention_of_its_agent_in_its_channel() {
        let mut fixture = Fixture::new("queue-absorb");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        fixture.request(
            "channel.create",
            json!({"workspace_id": workspace_id, "channel": "dev"}),
        );
        fixture.mention(&workspace_id, "geral", "@chief um");
        let first = fixture.mention(&workspace_id, "geral", "@reviewer primeiro");
        let second = fixture.mention(&workspace_id, "geral", "@reviewer segundo");
        fixture.mention(&workspace_id, "dev", "@reviewer em dev");

        let records = Fixture::records(&state_dir);
        assert_eq!(records.len(), 3, "{records:?}");
        assert_eq!(records[1].origin_message_id, Some(first));
        assert_eq!(records[1].queued_message_id, Some(second));
        assert_eq!(
            records[2].channel.as_deref(),
            Some("dev"),
            "another channel queues"
        );
        fixture.assert_queue_invariants(&state_dir, 1);

        fixture.finish_turn(fixture.running_pane("t1"));

        let prompt = fixture.running_argv("t2").pop().unwrap();
        assert!(prompt.contains("@reviewer segundo"), "{prompt}");
        assert!(
            prompt.contains("@reviewer primeiro"),
            "within the 20: {prompt}"
        );
        let record = &Fixture::records(&state_dir)[1];
        assert_eq!((record.turn, record.turn_message_id), (0, Some(second)));
        assert_eq!(record.origin_message_id, Some(first));
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // Known limit of the absorption shortcut: the prompt holds the 20
    // messages up to the newest mention, so an older absorbed mention can
    // fall out of it.
    #[tokio::test]
    async fn an_absorbed_older_mention_can_fall_out_of_the_prompt() {
        let mut fixture = Fixture::new("queue-absorb-limit");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@reviewer primeiro");
        for number in 0..adapter::RECENT_MESSAGES {
            fixture.mention(&workspace_id, "geral", &format!("conversa {number}"));
        }
        fixture.mention(&workspace_id, "geral", "@reviewer segundo");

        fixture.finish_turn(fixture.running_pane("t1"));

        let prompt = fixture.running_argv("t2").pop().unwrap();
        assert!(prompt.contains("@reviewer segundo"), "{prompt}");
        assert!(!prompt.contains("@reviewer primeiro"), "{prompt}");
        assert_eq!(Fixture::records(&state_dir).len(), 2);
    }

    // Every end of a slot drains the queue: exit, close pane and detach.
    #[tokio::test]
    async fn every_end_of_an_active_thread_starts_the_next_queued_one() {
        for end in ["exit", "close", "detach", "blocked-exit"] {
            let _manifest = FakeManifest::install(&format!("queue-end-{end}"));
            let mut fixture = Fixture::new(&format!("queue-end-{end}"));
            let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
            fixture.mention(&workspace_id, "geral", "@chief um");
            fixture.mention(&workspace_id, "geral", "@reviewer dois");
            let pane_id = fixture.running_pane("t1");

            match end {
                "exit" => fixture.exit(pane_id),
                "close" => {
                    let public_pane_id = fixture.public_pane_id(pane_id);
                    fixture.request("pane.close", json!({"pane_id": public_pane_id}));
                }
                "detach" => {
                    let terminal_id = fixture.app.running_threads[&pane_id].terminal_id.clone();
                    fixture
                        .app
                        .end_threads_of_detached_terminals(&[terminal_id]);
                }
                _ => {
                    // A blocked thread holds its slot until it ends.
                    fixture.show(pane_id, FAKE_BLOCKED);
                    assert_eq!(Fixture::queue(&state_dir).1, ["t2"]);
                    fixture.exit(pane_id);
                }
            }

            assert_eq!(
                Fixture::records(&state_dir)[0].state,
                ThreadState::Failed,
                "{end}"
            );
            assert_eq!(
                Fixture::queue(&state_dir),
                (vec!["t2".into()], vec![]),
                "{end}"
            );
            fixture.assert_queue_invariants(&state_dir, 1);
        }
    }

    // A queued thread that cannot start fails, says so, and the next one
    // starts in the same drain.
    #[tokio::test]
    async fn a_queued_thread_that_cannot_start_fails_and_the_next_one_starts() {
        let mut fixture = Fixture::new("queue-start-fails");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        Fixture::agent(&state_dir, "ghost");
        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@ghost dois");
        fixture.mention(&workspace_id, "geral", "@reviewer três");
        std::fs::remove_file(state_dir.join("agents").join("ghost.md")).unwrap();

        fixture.finish_turn(fixture.running_pane("t1"));

        let records = Fixture::records(&state_dir);
        assert_eq!(records[1].state, ThreadState::Failed);
        assert_eq!(
            records[1].end_reason.as_deref(),
            Some("@ghost is no longer an agent of the workspace")
        );
        assert_eq!(
            Fixture::system_bodies(&state_dir),
            ["a thread de @ghost não iniciou: @ghost is no longer an agent of the workspace"]
        );
        assert_eq!(Fixture::queue(&state_dir), (vec!["t3".into()], vec![]));
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // A change of the limit applies at the next admission point: a higher
    // limit starts queued threads at the folder check, and a lower one
    // stops nothing.
    #[tokio::test]
    async fn a_new_limit_applies_to_the_next_threads() {
        let mut fixture = Fixture::new("queue-limit");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@reviewer dois");

        Fixture::set_limit(&state_dir, 2);
        fixture.app.refresh_folder_connections();

        assert_eq!(
            Fixture::queue(&state_dir),
            (vec!["t1".into(), "t2".into()], vec![])
        );

        Fixture::set_limit(&state_dir, 1);
        fixture.app.refresh_folder_connections();
        fixture.mention(&workspace_id, "geral", "@chief três");
        assert_eq!(
            Fixture::queue(&state_dir),
            (vec!["t1".into(), "t2".into()], vec!["t3".into()]),
            "a lower limit stops nothing"
        );
        fixture.finish_turn(fixture.running_pane("t1"));
        assert_eq!(
            Fixture::queue(&state_dir).1,
            ["t3"],
            "two still hold a slot"
        );
        fixture.finish_turn(fixture.running_pane("t2"));
        assert_eq!(Fixture::queue(&state_dir), (vec!["t3".into()], vec![]));
        fixture.assert_queue_invariants(&state_dir, 1);
    }

    // `max_active_threads = 0` is invalid: the folder check reads no limit,
    // so the queue waits and starts nothing.
    #[tokio::test]
    async fn a_limit_of_zero_starts_nothing() {
        let mut fixture = Fixture::new("queue-zero");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@reviewer dois");
        Fixture::set_limit(&state_dir, 0);

        fixture.finish_turn(fixture.running_pane("t1"));

        assert_eq!(Fixture::queue(&state_dir), (vec![], vec!["t2".into()]));
        let root = state_dir.parent().unwrap();
        let err = crate::regents::folder::read_manifest(root).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_active_threads is 0, the minimum is 1"),
            "{err}"
        );
    }

    // A closed workspace fails its queued threads.
    #[tokio::test]
    async fn closing_the_workspace_fails_its_queued_threads() {
        let mut fixture = Fixture::new("queue-close-ws");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@reviewer dois");

        let closed = fixture.request("workspace.close", json!({"workspace_id": workspace_id}));

        assert!(closed.get("error").is_none(), "{closed}");
        let records = Fixture::records(&state_dir);
        assert_eq!(records[0].end_reason.as_deref(), Some(PANE_CLOSED));
        assert_eq!(records[1].state, ThreadState::Failed);
        assert_eq!(records[1].end_reason.as_deref(), Some(WORKSPACE_CLOSED));
        assert!(Fixture::system_bodies(&state_dir)
            .contains(&format!("a thread de @reviewer falhou: {WORKSPACE_CLOSED}")));
        assert!(fixture.app.running_threads.is_empty());
    }

    // A disconnected folder keeps its queue: it drains when it reconnects.
    #[tokio::test]
    async fn a_disconnected_folder_keeps_its_queue() {
        let mut fixture = Fixture::new("queue-disconnected");
        let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        fixture.mention(&workspace_id, "geral", "@chief um");
        fixture.mention(&workspace_id, "geral", "@reviewer dois");
        let manifest = state_dir.join("workspace.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::rename(&manifest, state_dir.join("moved.toml")).unwrap();
        fixture.app.refresh_folder_connections();

        fixture.exit(fixture.running_pane("t1"));

        assert_eq!(Fixture::queue(&state_dir), (vec![], vec!["t2".into()]));
        std::fs::write(&manifest, text).unwrap();
        fixture.app.refresh_folder_connections();
        assert_eq!(Fixture::queue(&state_dir), (vec!["t2".into()], vec![]));
    }

    // Restart and handoff: the queue stays in the registry, and the new
    // server drains it after it marks the threads of the old one
    // interrupted, once.
    #[tokio::test]
    async fn a_new_server_drains_the_queue_after_a_restart_or_a_handoff() {
        for handoff in [false, true] {
            let mut old = Fixture::new(&format!("queue-restart-old-{handoff}"));
            let (_, state_dir, workspace_id) = queue_workspace(&mut old, 1);
            old.mention(&workspace_id, "geral", "@chief um");
            old.mention(&workspace_id, "geral", "@reviewer dois");
            old.mention(&workspace_id, "geral", "@chief três");
            if handoff {
                old.app.interrupt_running_threads();
                assert_eq!(
                    Fixture::queue(&state_dir).1,
                    ["t2", "t3"],
                    "the old server drains nothing"
                );
            }
            shutdown_test_runtimes(&mut old.app);
            let folder = state_dir.parent().unwrap().to_path_buf();

            let mut new = Fixture::new(&format!("queue-restart-new-{handoff}"));
            let added = new.request(
                "workspace.add",
                json!({"path": folder.display().to_string(), "focus": true}),
            );

            assert_eq!(added["result"]["type"], "workspace_added", "{added}");
            let records = Fixture::records(&state_dir);
            assert_eq!(records[0].state, ThreadState::Interrupted);
            let reason = if handoff {
                SERVER_HANDOFF
            } else {
                SERVER_RESTARTED
            };
            assert_eq!(
                Fixture::system_bodies(&state_dir),
                [format!("a thread de @chief foi interrompida: {reason}")],
                "one interruption message"
            );
            assert_eq!(
                Fixture::queue(&state_dir),
                (vec!["t2".into()], vec!["t3".into()])
            );
            new.assert_queue_invariants(&state_dir, 1);
        }
    }

    // Identity: with adversarial pane, tab and workspace IDs before it, the
    // queue starts its threads in the hidden panes of its own workspace.
    #[tokio::test]
    async fn the_queue_keeps_the_identity_invariants_of_an_adversarial_state() {
        let mut fixture = Fixture::new("queue-adversarial");
        fixture.app.state = crate::app::AppState::test_with_adversarial_identity_state();
        fixture.app.state.assert_invariants_for_test();
        let (ws_idx, state_dir, workspace_id) = queue_workspace(&mut fixture, 1);
        assert_ne!(ws_idx, 0);
        fixture.mention(&workspace_id, "geral", "@chief um @reviewer dois");
        fixture.assert_queue_invariants(&state_dir, 1);

        fixture.finish_turn(fixture.running_pane("t1"));

        fixture.assert_queue_invariants(&state_dir, 1);
        let pane_id = fixture.running_pane("t2");
        assert_eq!(
            fixture.app.find_pane(pane_id).map(|(index, _)| index),
            Some(ws_idx)
        );
        assert!(fixture.app.state.workspaces[ws_idx]
            .background_panes
            .contains_key(&pane_id));
        fixture.finish_turn(pane_id);
        fixture.assert_queue_invariants(&state_dir, 1);
        assert!(fixture.app.state.workspaces[ws_idx]
            .background_panes
            .is_empty());
    }

    /// The reference model of the queue: FIFO, a slot counter and the
    /// absorption rule.
    #[derive(Default)]
    struct QueueModel {
        next_id: u32,
        limit: usize,
        /// Active thread numbers, oldest first.
        active: Vec<u32>,
        queue: std::collections::VecDeque<(u32, &'static str, &'static str)>,
    }

    impl QueueModel {
        fn mention(&mut self, agent: &'static str, channel: &'static str) {
            if !self
                .queue
                .iter()
                .any(|(_, a, c)| *a == agent && *c == channel)
            {
                self.next_id += 1;
                self.queue.push_back((self.next_id, agent, channel));
            }
            self.drain();
        }

        fn end(&mut self, index: usize) {
            self.active.remove(index);
            self.drain();
        }

        fn drain(&mut self) {
            while self.active.len() < self.limit {
                let Some((id, _, _)) = self.queue.pop_front() else {
                    return;
                };
                self.active.push(id);
            }
        }

        fn ids(&self) -> (Vec<String>, Vec<String>) {
            let mut active = self.active.clone();
            active.sort();
            (
                active.iter().map(|id| format!("t{id}")).collect(),
                self.queue
                    .iter()
                    .map(|(id, _, _)| format!("t{id}"))
                    .collect(),
            )
        }
    }

    // The model and the server take the same deterministic events and must
    // agree after each one; the invariants hold after each one.
    #[tokio::test]
    async fn the_queue_follows_its_reference_model() {
        const AGENTS: [&str; 3] = ["chief", "reviewer", "coder"];
        const CHANNELS: [&str; 2] = ["geral", "dev"];
        for seed in [1_u64, 7, 42] {
            let mut fixture = Fixture::new(&format!("queue-model-{seed}"));
            let (_, state_dir, workspace_id) = queue_workspace(&mut fixture, 2);
            Fixture::agent(&state_dir, "coder");
            fixture.request(
                "channel.create",
                json!({"workspace_id": workspace_id, "channel": "dev"}),
            );
            let mut model = QueueModel {
                limit: 2,
                ..QueueModel::default()
            };
            let mut state = seed;
            let mut next = |bound: u64| {
                // A linear congruential generator: deterministic, no crate.
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 33) % bound
            };
            for step in 0..60 {
                let event = next(10);
                let running = model.active.len();
                match event {
                    0..=4 => {
                        let agent = AGENTS[next(3) as usize];
                        let channel = CHANNELS[next(2) as usize];
                        fixture.mention(&workspace_id, channel, &format!("@{agent} passo {step}"));
                        model.mention(agent, channel);
                    }
                    5..=7 if running > 0 => {
                        let index = next(running as u64) as usize;
                        let thread_id = format!("t{}", {
                            let mut active = model.active.clone();
                            active.sort();
                            active[index]
                        });
                        let pane_id = fixture.running_pane(&thread_id);
                        match event {
                            5 => fixture.finish_turn(pane_id),
                            6 => fixture.exit(pane_id),
                            _ => {
                                let public_pane_id = fixture.public_pane_id(pane_id);
                                fixture.request("pane.close", json!({"pane_id": public_pane_id}));
                            }
                        }
                        let position = model
                            .active
                            .iter()
                            .position(|id| format!("t{id}") == thread_id)
                            .unwrap();
                        model.end(position);
                    }
                    8 | 9 => {
                        let limit = 1 + next(3) as usize;
                        Fixture::set_limit(&state_dir, limit as u32);
                        fixture.app.refresh_folder_connections();
                        model.limit = limit;
                        model.drain();
                    }
                    _ => {}
                }
                assert_eq!(
                    Fixture::queue(&state_dir),
                    model.ids(),
                    "seed {seed}, step {step}"
                );
                fixture.assert_queue_invariants(&state_dir, model.limit);
            }
        }
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
