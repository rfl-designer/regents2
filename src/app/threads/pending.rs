//! The runtime of a pending mention (#17): a new mention of the agent of an
//! active thread, in the channel of that thread, waits for the decision of
//! the thread. `regents::pending::step` is the rule; this module feeds it the
//! events of the running thread and the clock, and applies its effects. The
//! thread registry makes every step a compare-and-set.

use std::path::Path;

use bytes::Bytes;

use super::read_message;
use crate::app::App;
use crate::layout::PaneId;
use crate::regents::adapter;
use crate::regents::agents::{self, AgentCli, Isolation};
use crate::regents::channel::{self, Message};
use crate::regents::pending::{self, Effect, Event, Phase};
use crate::regents::threads::{self, NewThread, PendingMention, ThreadOrigin, ThreadProgress};

/// The delay between the text and the Enter of a delivery, as for
/// `agent.prompt`: a CLI reads a quick Enter as part of a paste.
const DELIVERY_SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

impl App {
    /// The deadline (Unix seconds) of a mention of an agent of `cli` that
    /// would wait for an active thread: `None` when the CLI cannot get a
    /// mention in its running session, or the manifest is not valid.
    pub(super) fn mention_decision_deadline(
        &self,
        state_dir: &Path,
        cli: AgentCli,
        now: time::OffsetDateTime,
    ) -> Option<i64> {
        if !adapter::has_mid_session_delivery(cli) {
            return None;
        }
        let root = state_dir.parent().unwrap_or(state_dir);
        let timeout = crate::regents::folder::read_manifest(root)
            .inspect_err(|err| tracing::warn!(%err, "a mention cannot wait for a thread"))
            .ok()?
            .mention_decision_timeout_seconds;
        Some(
            now.unix_timestamp()
                .saturating_add(i64::try_from(timeout).unwrap_or(i64::MAX)),
        )
    }

    /// The first idle after work of the thread in `pane_id`, where a thread
    /// without a pending mention is done. True when the thread goes on: it
    /// just got its pending mention.
    pub(super) fn pending_mention_at_idle(&mut self, pane_id: PaneId) -> bool {
        let Some(thread) = self.running_threads.get(&pane_id) else {
            return false;
        };
        let (state_dir, thread_id) = (thread.state_dir.clone(), thread.thread_id.clone());
        match open_pending(&state_dir, &thread_id) {
            Some(pending) => {
                self.step_pending(&state_dir, pending, Event::Idle) == Phase::Delivered
            }
            None => false,
        }
    }

    /// The thread `thread_id` stopped running: its open pending mention goes
    /// to a new thread.
    pub(super) fn end_pending_mention(&mut self, state_dir: &Path, thread_id: &str) {
        if let Some(pending) = open_pending(state_dir, thread_id) {
            self.step_pending(state_dir, pending, Event::Ended);
        }
    }

    /// The clock of the open pending mentions of `state_dir`, with the
    /// folder checks. A mention whose thread no process runs, after a
    /// restart or a handoff, goes to a new thread.
    pub(super) fn tick_pending_mentions(&mut self, state_dir: &Path) {
        let open = threads::registry(state_dir)
            .open_pending_mentions()
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "cannot list the pending mentions");
                Vec::new()
            });
        for pending in open {
            let event = if self
                .running_thread_pane_id(state_dir, &pending.thread_id)
                .is_some()
            {
                Event::Tick
            } else {
                Event::Ended
            };
            self.step_pending(state_dir, pending, event);
        }
    }

    /// `thread.accept` or `thread.fork` of the running thread `thread_id`,
    /// whose credential the caller checked. The result is the newest
    /// pending mention of the thread after the decision; `None` when it has
    /// none.
    pub(crate) fn decide_pending_mention(
        &mut self,
        state_dir: &Path,
        thread_id: &str,
        decision: Event,
    ) -> Result<Option<PendingMention>, threads::ThreadError> {
        let registry = threads::registry(state_dir);
        let Some(pending) = registry.latest_pending_mention(thread_id)? else {
            return Ok(None);
        };
        if pending.phase.is_open() {
            self.step_pending(state_dir, pending, decision);
        }
        registry.latest_pending_mention(thread_id)
    }

    /// One step of `pending` with `event` at the current time, applied to
    /// the registry and then to the running thread. The result is the phase
    /// of the mention after the step; another writer can win the step.
    fn step_pending(&mut self, state_dir: &Path, pending: PendingMention, event: Event) -> Phase {
        let now = time::OffsetDateTime::now_utc();
        let (to, effect) = pending::step(pending.phase, pending.decide_by, event, now);
        if to == pending.phase {
            return to;
        }
        let pane_id = self
            .running_threads
            .iter()
            .find(|(_, thread)| {
                thread.thread_id == pending.thread_id && thread.state_dir == state_dir
            })
            .map(|(&pane_id, _)| pane_id);
        let mention = match effect {
            Effect::Deliver => match pane_id.and_then(|id| self.pending_message(id, &pending)) {
                Some(mention) => Some(mention),
                // The mention or the pane is gone: a new thread takes it.
                None => return self.step_pending(state_dir, pending, Event::Ended),
            },
            _ => None,
        };
        let registry = threads::registry(state_dir);
        let fork = match effect {
            Effect::OpenThread => fork_thread(state_dir, &pending),
            _ => None,
        };
        match registry.advance_pending(&pending, to, fork, now) {
            Ok(true) => {}
            Ok(false) => {
                return registry
                    .latest_pending_mention(&pending.thread_id)
                    .ok()
                    .flatten()
                    .map_or(to, |latest| latest.phase);
            }
            Err(err) => {
                tracing::warn!(thread = %pending.thread_id, %err, "cannot record the pending mention");
                return pending.phase;
            }
        }
        tracing::info!(thread = %pending.thread_id, message = pending.message_id,
            phase = to.as_str(), "a pending mention changed");
        match (effect, pane_id, mention) {
            (Effect::Deliver, Some(pane_id), Some(mention)) => {
                if !self.deliver(pane_id, &mention) {
                    // The CLI did not get the mention: a new thread takes it,
                    // and the idle thread is done.
                    let delivered = PendingMention {
                        phase: Phase::Delivered,
                        ..pending
                    };
                    return self.step_pending(state_dir, delivered, Event::Ended);
                }
                // Only now: the registry turn counts the prompts of the CLI session.
                if let Err(err) = registry.start_decision_turn(&pending.thread_id, now) {
                    tracing::warn!(thread = %pending.thread_id, %err, "cannot record the decision turn");
                }
            }
            (Effect::Link, Some(pane_id), _) => {
                if let Some(thread) = self.running_threads.get_mut(&pane_id) {
                    thread.turn_message_id = Some(pending.message_id);
                    thread.decision_turn = false;
                }
            }
            (Effect::OpenThread, _, _) => {
                if to == Phase::TimedOut {
                    self.post_timeout(state_dir, &pending);
                }
                // A decision turn that already ended gets no other idle:
                // the thread is done now.
                if let Some(pane_id) = pane_id.filter(|&id| self.idle_in_decision_turn(id)) {
                    self.end_thread(pane_id, threads::ThreadState::Done, None);
                }
                self.drain_thread_queue(state_dir);
            }
            _ => {}
        }
        self.refresh_folder_threads();
        to
    }

    /// The message of `pending`, when the thread in `pane_id` runs and can
    /// get it.
    fn pending_message(&self, pane_id: PaneId, pending: &PendingMention) -> Option<Message> {
        let thread = self.running_threads.get(&pane_id)?;
        self.terminal_runtimes.get(&thread.terminal_id)?;
        let channel = channel::open(&thread.state_dir, thread.channel.as_deref()?).ok()?;
        read_message(&channel, pending.message_id)
    }

    /// Types the decision prompt about `mention` into the thread CLI in
    /// `pane_id`, at an idle. The reply of the turn that ends is captured
    /// first; the decision is the next turn of the session, as the registry
    /// records. False when the CLI did not get it.
    fn deliver(&mut self, pane_id: PaneId, mention: &Message) -> bool {
        let Some(mut thread) = self.running_threads.remove(&pane_id) else {
            return false;
        };
        if !thread.decision_turn {
            self.capture_reply(pane_id, &thread);
        }
        thread.turn += 1;
        thread.decision_turn = true;
        // The next idle after new work ends the decision turn.
        thread.progress = ThreadProgress::default();
        let submitted = self
            .terminal_runtimes
            .get(&thread.terminal_id)
            .is_some_and(|runtime| {
                let (text, enter) = crate::app::api_helpers::encode_api_submission_parts(
                    runtime,
                    &adapter::decision_prompt(mention),
                );
                runtime
                    .queue_user_input_submission(
                        Bytes::from(text),
                        Bytes::from(enter),
                        DELIVERY_SUBMIT_DELAY,
                        None,
                    )
                    .inspect_err(|err| {
                        tracing::warn!(thread = %thread.thread_id, %err, "cannot deliver a pending mention");
                    })
                    .is_ok()
            });
        self.running_threads.insert(pane_id, thread);
        submitted
    }

    /// True when the thread in `pane_id` runs its decision turn and its
    /// agent is idle.
    fn idle_in_decision_turn(&self, pane_id: PaneId) -> bool {
        self.running_threads.get(&pane_id).is_some_and(|thread| {
            thread.decision_turn
                && self
                    .state
                    .terminals
                    .get(&thread.terminal_id)
                    .is_some_and(|terminal| terminal.state == crate::detect::AgentState::Idle)
        })
    }

    fn post_timeout(&mut self, state_dir: &Path, pending: &PendingMention) {
        let Ok(Some(record)) = threads::registry(state_dir).get(&pending.thread_id) else {
            return;
        };
        let Some(channel) = record.channel else {
            return;
        };
        self.post_system_message(
            self.folder_workspace_index(state_dir),
            state_dir,
            &channel,
            Some(&pending.thread_id),
            format!(
                "@{} não decidiu sobre a mensagem {} a tempo; uma thread nova a recebe",
                record.agent, pending.message_id
            ),
        );
    }
}

fn open_pending(state_dir: &Path, thread_id: &str) -> Option<PendingMention> {
    threads::registry(state_dir)
        .latest_pending_mention(thread_id)
        .inspect_err(|err| tracing::warn!(%err, "cannot read the pending mention"))
        .ok()
        .flatten()
        .filter(|pending| pending.phase.is_open())
}

/// The new thread that takes `pending` after a fork or a timeout: a thread
/// of the same agent and channel, opened by the mention. A fork is no
/// delegation: its depth is 0. `None` when the agent is gone.
fn fork_thread(state_dir: &Path, pending: &PendingMention) -> Option<NewThread> {
    let record = threads::registry(state_dir)
        .get(&pending.thread_id)
        .ok()
        .flatten()?;
    let Some(definition) = agents::list(state_dir)
        .into_iter()
        .find(|definition| definition.name == record.agent)
    else {
        tracing::warn!(thread = %record.id, "the agent of a pending mention is gone");
        return None;
    };
    Some(NewThread {
        agent: record.agent,
        cli: definition.cli,
        origin: ThreadOrigin::Channel,
        channel: record.channel,
        origin_message_id: Some(pending.message_id),
        opener_thread_id: None,
        depth: 0,
        // Worktree isolation is #24: every thread runs in the folder.
        isolation: Isolation::Shared,
        session_id: Some(adapter::new_session_id()),
    })
}
