//! The thread registry of a workspace (Regents, ADR-0003):
//! `.regents/threads.sqlite`, and the pure rule that tells when a running
//! thread is done. The server is the only writer of the registry.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};

use super::agents::{AgentCli, Isolation};
use super::pending::Phase;
use super::store;
use crate::detect::AgentState;

const DATABASE_FILE: &str = "threads.sqlite";
/// Folder of `.regents` with the last output of each ended thread, one
/// `<id>.ansi` file per thread.
const OUTPUT_DIR: &str = "thread-output";
const ID_PREFIX: &str = "t";
const SCHEMA: store::Schema = store::Schema {
    steps: &[
        "CREATE TABLE IF NOT EXISTS threads (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             agent TEXT NOT NULL,
             cli TEXT NOT NULL,
             -- 'channel' (opened by a mention) or 'direct' (a direct conversation).
             origin_kind TEXT NOT NULL DEFAULT 'channel',
             channel TEXT,
             origin_message_id INTEGER,
             -- The thread whose agent mentioned this agent (delegation chain).
             opener_thread_id TEXT,
             depth INTEGER NOT NULL DEFAULT 0,
             -- 'shared' (the workspace folder) or 'worktree'.
             isolation TEXT,
             state TEXT NOT NULL,
             end_reason TEXT,
             session_id TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );",
        // Version 2: the turn of the CLI session, from 0, and the message
        // that started it (a resumed turn starts at a threaded reply).
        "ALTER TABLE threads ADD COLUMN turn INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE threads ADD COLUMN turn_message_id INTEGER;
         UPDATE threads SET turn_message_id = origin_message_id;",
        // Version 3: the thread queue (#16). `queue_seq` orders the entries
        // into the queue, and `queued_message_id` is the anchor of the
        // prompt of the queued turn. A queued thread that never ran has no
        // `turn_message_id`.
        "ALTER TABLE threads ADD COLUMN queue_seq INTEGER;
         ALTER TABLE threads ADD COLUMN queued_message_id INTEGER;
         CREATE UNIQUE INDEX threads_one_queued_per_pair
             ON threads(agent, channel) WHERE state = 'queued';",
        // Version 4: the pending mentions of active threads (#17). An open
        // mention ('waiting' or 'delivered') is one per thread; each phase
        // change is a compare-and-set, so a decision applies once.
        "CREATE TABLE pending_mentions (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             thread_id INTEGER NOT NULL,
             message_id INTEGER NOT NULL,
             phase TEXT NOT NULL,
             -- Unix seconds: past it an open mention times out.
             decide_by INTEGER NOT NULL,
             -- The thread that took the mention after a fork or a timeout.
             fork_thread_id INTEGER,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE UNIQUE INDEX pending_mentions_one_open_per_thread
             ON pending_mentions(thread_id) WHERE phase IN ('waiting', 'delivered');",
    ],
};

/// The threads that take a slot of `max_active_threads`: a direct
/// conversation takes none.
const SLOT_FILTER: &str = "state = 'active' AND origin_kind <> 'direct'";

/// The life cycle of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadState {
    Queued,
    /// The agent CLI runs.
    Active,
    /// The agent finished its turn; the CLI process ended.
    Done,
    /// The server stopped while the thread was active.
    Interrupted,
    /// The CLI did not finish its turn: it ended before any work, it did not
    /// start, or its pane closed. `end_reason` says which.
    Failed,
    /// A state written by a newer Regents.
    Unknown,
}

impl ThreadState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Done => "done",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "queued" => Self::Queued,
            "active" => Self::Active,
            "done" => Self::Done,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

/// What opened a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadOrigin {
    /// A mention in a channel.
    Channel,
    /// A direct conversation that the user opened.
    Direct,
    /// An origin written by a newer Regents.
    Unknown,
}

impl ThreadOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Direct => "direct",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "channel" => Self::Channel,
            "direct" => Self::Direct,
            _ => Self::Unknown,
        }
    }
}

/// A thread to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewThread {
    pub(crate) agent: String,
    pub(crate) cli: AgentCli,
    pub(crate) origin: ThreadOrigin,
    /// Channel and message of the mention that opened the thread.
    pub(crate) channel: Option<String>,
    pub(crate) origin_message_id: Option<u64>,
    /// The thread whose agent mentioned this agent; `None` when a human did.
    pub(crate) opener_thread_id: Option<String>,
    /// Length of the delegation chain before this thread: 0 when a human
    /// opened it.
    pub(crate) depth: u32,
    pub(crate) isolation: Isolation,
    /// Session of the agent CLI, to resume the thread.
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadRecord {
    /// `t<n>`, unique in the workspace.
    pub(crate) id: String,
    pub(crate) agent: String,
    /// `None` for a CLI that this Regents does not know.
    pub(crate) cli: Option<AgentCli>,
    pub(crate) origin: ThreadOrigin,
    pub(crate) channel: Option<String>,
    pub(crate) origin_message_id: Option<u64>,
    pub(crate) opener_thread_id: Option<String>,
    pub(crate) depth: u32,
    pub(crate) isolation: Option<Isolation>,
    pub(crate) state: ThreadState,
    /// Why the thread ended, when it did not end done.
    pub(crate) end_reason: Option<String>,
    pub(crate) session_id: Option<String>,
    /// Turn of the CLI session, from 0: each resume adds one.
    pub(crate) turn: u32,
    /// The message that started the turn: the opening mention, or the
    /// threaded reply that resumed the thread.
    pub(crate) turn_message_id: Option<u64>,
    /// Place in the queue of a queued thread, from 1.
    pub(crate) queue_position: Option<u32>,
    /// Anchor of the prompt of a queued turn: the newest absorbed mention,
    /// or the threaded reply of a queued resume.
    pub(crate) queued_message_id: Option<u64>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

/// What a mention did to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admission {
    /// A new queued thread.
    Queued(String),
    /// The queued thread of the same agent and channel took the mention.
    Absorbed(String),
    /// The mention waits for the decision of this active thread.
    Pending(String),
}

/// A mention that waits for, or got, the decision of an active thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingMention {
    pub(crate) id: i64,
    pub(crate) thread_id: String,
    pub(crate) message_id: u64,
    pub(crate) phase: Phase,
    /// Unix seconds: past it an open mention times out.
    pub(crate) decide_by: i64,
    /// The thread that took the mention after a fork or a timeout.
    pub(crate) fork_thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadError(String);

impl ThreadError {
    pub(crate) fn code(&self) -> &'static str {
        "thread_storage_error"
    }
}

impl std::fmt::Display for ThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "thread registry error: {}", self.0)
    }
}

impl From<rusqlite::Error> for ThreadError {
    fn from(err: rusqlite::Error) -> Self {
        Self(err.to_string())
    }
}

/// The thread registry of the workspace whose state directory is
/// `state_dir`. Opening it creates no file; the first thread does.
#[derive(Debug, Clone)]
pub(crate) struct ThreadRegistry {
    path: PathBuf,
}

pub(crate) fn registry(state_dir: &Path) -> ThreadRegistry {
    ThreadRegistry {
        path: state_dir.join(DATABASE_FILE),
    }
}

impl ThreadRegistry {
    /// Records a new active thread outside the queue: tests only, the
    /// server admits a channel thread with `admit_mention`.
    #[cfg(test)]
    pub(crate) fn create(
        &self,
        thread: NewThread,
        now: time::OffsetDateTime,
    ) -> Result<ThreadRecord, ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let origin_message = thread.origin_message_id.map(sql_message_id).transpose()?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        connection.execute(
            "INSERT INTO threads
                 (agent, cli, origin_kind, channel, origin_message_id, opener_thread_id, depth,
                  isolation, state, session_id, created_at, updated_at, turn_message_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?5)",
            params![
                thread.agent,
                thread.cli.as_str(),
                thread.origin.as_str(),
                thread.channel,
                origin_message,
                thread.opener_thread_id,
                thread.depth,
                thread.isolation.as_str(),
                ThreadState::Active.as_str(),
                thread.session_id,
                now,
            ],
        )?;
        Ok(ThreadRecord {
            id: format!("{ID_PREFIX}{}", connection.last_insert_rowid()),
            agent: thread.agent,
            cli: Some(thread.cli),
            origin: thread.origin,
            channel: thread.channel,
            origin_message_id: thread.origin_message_id,
            opener_thread_id: thread.opener_thread_id,
            depth: thread.depth,
            isolation: Some(thread.isolation),
            state: ThreadState::Active,
            end_reason: None,
            session_id: thread.session_id,
            turn: 0,
            turn_message_id: thread.origin_message_id,
            queue_position: None,
            queued_message_id: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Admission of a mention. The queued thread of the same agent and
    /// channel takes the mention as its new anchor. Otherwise, with a
    /// deadline `decide_by` (Unix seconds), the newest active channel thread
    /// of the pair without an open pending mention gets it as pending: it
    /// decides (#17). Otherwise a new thread enters the queue, and the drain
    /// starts it.
    // shortcut: an older mention that a queued thread absorbed can fall out
    // of the RECENT_MESSAGES of its prompt; keep every absorbed mention when
    // an agent needs them all.
    pub(crate) fn admit_mention(
        &self,
        thread: NewThread,
        decide_by: Option<i64>,
        now: time::OffsetDateTime,
    ) -> Result<Admission, ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let mut connection = store::open_writable(&self.path, &SCHEMA)?;
        // One writer at a time: the absorb, the pending mention and the
        // insert see the same queue.
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let admission = match (decide_by, thread.origin_message_id) {
            (Some(decide_by), Some(message)) if !has_queued(&transaction, &thread)? => {
                let active: Option<i64> = transaction
                    .query_row(
                        "SELECT id FROM threads
                         WHERE state = 'active' AND origin_kind = 'channel'
                           AND agent = ?1 AND channel IS ?2
                           AND id NOT IN (SELECT thread_id FROM pending_mentions
                                          WHERE phase IN ('waiting', 'delivered'))
                         ORDER BY id DESC LIMIT 1",
                        params![thread.agent, thread.channel],
                        |row| row.get(0),
                    )
                    .optional()?;
                match active {
                    Some(row) => {
                        transaction.execute(
                            "INSERT INTO pending_mentions
                                 (thread_id, message_id, phase, decide_by, created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                            params![
                                row,
                                sql_message_id(message)?,
                                Phase::Waiting.as_str(),
                                decide_by,
                                now
                            ],
                        )?;
                        Admission::Pending(format!("{ID_PREFIX}{row}"))
                    }
                    None => queue_in(&transaction, &thread, &now)?,
                }
            }
            _ => queue_in(&transaction, &thread, &now)?,
        };
        transaction.commit()?;
        Ok(admission)
    }

    /// The open pending mentions, oldest first.
    pub(crate) fn open_pending_mentions(&self) -> Result<Vec<PendingMention>, ThreadError> {
        self.pending_query("WHERE phase IN ('waiting', 'delivered') ORDER BY id", [])
    }

    /// The newest pending mention of the thread `thread_id`, open or not.
    pub(crate) fn latest_pending_mention(
        &self,
        thread_id: &str,
    ) -> Result<Option<PendingMention>, ThreadError> {
        let row = parse_id(thread_id)?;
        Ok(self
            .pending_query("WHERE thread_id = ?1 ORDER BY id DESC LIMIT 1", [row])?
            .pop())
    }

    /// Compare-and-set of a step of `pending`: it goes from its phase to
    /// `to`, with the change that `to` brings to its thread. An accept
    /// starts the decision turn at the mention; a fork or a timeout admits
    /// `fork` as a new thread, or gives the mention to the queued thread of
    /// the pair. False when another writer changed the mention first.
    pub(crate) fn advance_pending(
        &self,
        pending: &PendingMention,
        to: Phase,
        fork: Option<NewThread>,
        now: time::OffsetDateTime,
    ) -> Result<bool, ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let thread = parse_id(&pending.thread_id)?;
        let mut connection = store::open_writable(&self.path, &SCHEMA)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE pending_mentions SET phase = ?1, updated_at = ?2 WHERE id = ?3 AND phase = ?4",
            params![to.as_str(), now, pending.id, pending.phase.as_str()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        match to {
            Phase::Accepted => {
                transaction.execute(
                    "UPDATE threads SET turn_message_id = ?1, updated_at = ?2
                     WHERE id = ?3 AND state = 'active'",
                    params![sql_message_id(pending.message_id)?, now, thread],
                )?;
            }
            Phase::Forked | Phase::TimedOut => {
                if let Some(fork) = &fork {
                    let admitted = queue_in(&transaction, fork, &now)?;
                    if let Admission::Queued(id) | Admission::Absorbed(id) = &admitted {
                        transaction.execute(
                            "UPDATE pending_mentions SET fork_thread_id = ?1 WHERE id = ?2",
                            params![parse_id(id)?, pending.id],
                        )?;
                    }
                }
            }
            Phase::Waiting | Phase::Delivered => {}
        }
        transaction.commit()?;
        Ok(true)
    }

    /// The CLI session of the active thread `id` got a new prompt: the
    /// decision about a pending mention is its next turn.
    pub(crate) fn start_decision_turn(
        &self,
        id: &str,
        now: time::OffsetDateTime,
    ) -> Result<(), ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        store::open_writable(&self.path, &SCHEMA)?.execute(
            "UPDATE threads SET turn = turn + 1, updated_at = ?1 WHERE id = ?2 AND state = 'active'",
            params![now, parse_id(id)?],
        )?;
        Ok(())
    }

    fn pending_query<P: rusqlite::Params>(
        &self,
        clause: &str,
        params: P,
    ) -> Result<Vec<PendingMention>, ThreadError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let mut statement = connection.prepare(&format!(
            "SELECT id, thread_id, message_id, phase, decide_by, fork_thread_id
             FROM pending_mentions {clause}"
        ))?;
        let rows = statement.query_map(params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        let mut mentions = Vec::new();
        for row in rows {
            let (id, thread, message, phase, decide_by, fork) = row?;
            // A phase of a newer Regents is not this Regents' to step.
            let (Some(phase), Ok(message_id)) = (Phase::parse(&phase), u64::try_from(message))
            else {
                continue;
            };
            mentions.push(PendingMention {
                id,
                thread_id: format!("{ID_PREFIX}{thread}"),
                message_id,
                phase,
                decide_by,
                fork_thread_id: fork.map(|row| format!("{ID_PREFIX}{row}")),
            });
        }
        Ok(mentions)
    }

    /// Compare-and-set of the admission of a resume: the thread `id` enters
    /// the queue for its next turn, started by the message `message_id`,
    /// only when it is still done in the turn `turn`. `None` when another
    /// writer changed it first.
    pub(crate) fn admit_resume(
        &self,
        id: &str,
        turn: u32,
        message_id: u64,
        now: time::OffsetDateTime,
    ) -> Result<Option<ThreadRecord>, ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let row = parse_id(id)?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let changed = connection.execute(
            "UPDATE threads
             SET state = 'queued', end_reason = NULL, queued_message_id = ?1, updated_at = ?2,
                 queue_seq = (SELECT coalesce(max(queue_seq), 0) + 1 FROM threads)
             WHERE id = ?3 AND state = ?4 AND turn = ?5",
            params![
                sql_message_id(message_id)?,
                now,
                row,
                ThreadState::Done.as_str(),
                turn
            ],
        )?;
        drop(connection);
        if changed == 0 {
            return Ok(None);
        }
        self.get(id)
    }

    /// The queued thread of `agent` in `channel`.
    pub(crate) fn queued_thread(
        &self,
        agent: &str,
        channel: &str,
    ) -> Result<Option<String>, ThreadError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let row: Option<i64> = connection
            .query_row(
                "SELECT id FROM threads WHERE state = 'queued' AND agent = ?1 AND channel = ?2",
                params![agent, channel],
                |row| row.get(0),
            )
            .optional()?;
        Ok(row.map(|row| format!("{ID_PREFIX}{row}")))
    }

    /// Compare-and-set of the exit from the queue: the oldest queued thread
    /// becomes active when fewer than `limit` threads take a slot. Its turn
    /// starts at its anchor; a thread that ran before begins its next turn.
    /// The result is the thread as it was queued, which keeps the anchor of
    /// its previous turn, and as it is now.
    pub(crate) fn promote_next(
        &self,
        limit: u32,
        now: time::OffsetDateTime,
    ) -> Result<Option<(ThreadRecord, ThreadRecord)>, ThreadError> {
        let Some(queued) = self
            .query("WHERE state = 'queued' ORDER BY queue_seq, id", 1)?
            .pop()
        else {
            return Ok(None);
        };
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let anchor = queued.queued_message_id.map(sql_message_id).transpose()?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let changed = connection.execute(
            &format!(
                "UPDATE threads
                 SET state = 'active', end_reason = NULL,
                     turn = turn + (turn_message_id IS NOT NULL),
                     turn_message_id = queued_message_id, queued_message_id = NULL,
                     updated_at = ?1
                 WHERE id = ?2 AND state = 'queued' AND queued_message_id IS ?3
                   AND (SELECT count(*) FROM threads WHERE {SLOT_FILTER}) < ?4"
            ),
            params![now, parse_id(&queued.id)?, anchor, limit],
        )?;
        drop(connection);
        if changed == 0 {
            return Ok(None);
        }
        let promoted = self
            .get(&queued.id)?
            .ok_or_else(|| ThreadError(format!("thread {} disappeared", queued.id)))?;
        Ok(Some((queued, promoted)))
    }

    /// Ends every queued thread as failed, with `reason`; the result is the
    /// `(id, agent, channel)` of the threads that it ended.
    pub(crate) fn fail_queued(
        &self,
        reason: &str,
        now: time::OffsetDateTime,
    ) -> Result<Vec<(String, String, Option<String>)>, ThreadError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        // One statement: the list is exactly the threads that it ended.
        let mut statement = connection.prepare(
            "UPDATE threads SET state = 'failed', end_reason = ?1, updated_at = ?2
             WHERE state = 'queued'
             RETURNING id, agent, channel",
        )?;
        let failed = statement
            .query_map(params![reason, now], |row| {
                Ok((
                    format!("{ID_PREFIX}{}", row.get::<_, i64>(0)?),
                    row.get(1)?,
                    row.get(2)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(failed)
    }

    /// The threads that take a slot, oldest first.
    pub(crate) fn slot_threads(&self) -> Result<Vec<ThreadRecord>, ThreadError> {
        self.query(&format!("WHERE {SLOT_FILTER} ORDER BY id"), -1)
    }

    /// Compare-and-set of an interruption: the thread `id` becomes
    /// interrupted with `reason` only when it is still active. False when
    /// another writer ended it first, so one interruption says so once.
    pub(crate) fn interrupt(
        &self,
        id: &str,
        reason: &str,
        now: time::OffsetDateTime,
    ) -> Result<bool, ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let row = parse_id(id)?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let changed = connection.execute(
            "UPDATE threads SET state = 'interrupted', end_reason = ?1, updated_at = ?2
             WHERE id = ?3 AND state = 'active'",
            params![reason, now, row],
        )?;
        Ok(changed == 1)
    }

    /// The thread `id`, or `None` when the registry does not have it.
    pub(crate) fn get(&self, id: &str) -> Result<Option<ThreadRecord>, ThreadError> {
        let row = parse_id(id)?;
        // `row` is a parsed number, so the clause is not injectable.
        Ok(self.query(&format!("WHERE id = {row}"), 1)?.pop())
    }

    /// Undoes the resume of `previous`, the thread as it was queued, whose
    /// CLI did not start: the thread is done again in its previous turn, so
    /// the next reply resumes it.
    pub(crate) fn undo_resume(
        &self,
        previous: &ThreadRecord,
        now: time::OffsetDateTime,
    ) -> Result<(), ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let row = parse_id(&previous.id)?;
        let anchor = previous.turn_message_id.map(sql_message_id).transpose()?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        connection.execute(
            "UPDATE threads SET state = ?1, turn = ?2, turn_message_id = ?3, updated_at = ?4
             WHERE id = ?5 AND state = ?6 AND turn = ?7",
            params![
                ThreadState::Done.as_str(),
                previous.turn,
                anchor,
                now,
                row,
                ThreadState::Active.as_str(),
                previous.turn + 1,
            ],
        )?;
        Ok(())
    }

    /// Records a new state. `end_reason` says why a thread ended without
    /// being done.
    pub(crate) fn set_state(
        &self,
        id: &str,
        state: ThreadState,
        end_reason: Option<&str>,
        now: time::OffsetDateTime,
    ) -> Result<(), ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let row = parse_id(id)?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let changed = connection.execute(
            "UPDATE threads SET state = ?1, end_reason = ?2, updated_at = ?3 WHERE id = ?4",
            params![state.as_str(), end_reason, now, row],
        )?;
        if changed == 0 {
            return Err(ThreadError(format!("thread {id} does not exist")));
        }
        Ok(())
    }

    /// Moves the threads of the channel `from` to the channel `to`, after
    /// a rename of the channel. Without a registry file there is none.
    pub(crate) fn rename_channel(&self, from: &str, to: &str) -> Result<(), ThreadError> {
        if !self.path.exists() {
            return Ok(());
        }
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        connection.execute(
            "UPDATE threads SET channel = ?1 WHERE channel = ?2",
            params![to, from],
        )?;
        Ok(())
    }

    /// Every thread, oldest first. Without a registry file there is none.
    pub(crate) fn list(&self) -> Result<Vec<ThreadRecord>, ThreadError> {
        self.query("ORDER BY id ASC", -1)
    }

    /// The `limit` newest threads, newest first.
    pub(crate) fn recent(&self, limit: usize) -> Result<Vec<ThreadRecord>, ThreadError> {
        self.query("ORDER BY id DESC", i64::try_from(limit).unwrap_or(i64::MAX))
    }

    /// Threads that match `clause` (a filter or an order), at most `limit`
    /// (-1: all).
    fn query(&self, clause: &str, limit: i64) -> Result<Vec<ThreadRecord>, ThreadError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        // Only the server reads the registry, so a read can bring an older
        // database to the current schema.
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        let mut statement = connection.prepare(&format!(
            "SELECT id, agent, cli, origin_kind, channel, origin_message_id, opener_thread_id,
                    depth, isolation, state, end_reason, session_id, created_at, updated_at,
                    turn, turn_message_id, queued_message_id,
                    CASE WHEN state = 'queued' THEN
                        (SELECT count(*) FROM threads AS ahead
                         WHERE ahead.state = 'queued' AND ahead.queue_seq <= threads.queue_seq)
                    END
             FROM threads {clause} LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit], |row| {
            Ok(ThreadRecord {
                id: format!("{ID_PREFIX}{}", row.get::<_, i64>(0)?),
                agent: row.get(1)?,
                cli: AgentCli::parse(&row.get::<_, String>(2)?),
                origin: ThreadOrigin::parse(&row.get::<_, String>(3)?),
                channel: row.get(4)?,
                origin_message_id: row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|id| u64::try_from(id).ok()),
                opener_thread_id: row.get(6)?,
                depth: row.get(7)?,
                isolation: row
                    .get::<_, Option<String>>(8)?
                    .as_deref()
                    .and_then(Isolation::parse),
                state: ThreadState::parse(&row.get::<_, String>(9)?),
                end_reason: row.get(10)?,
                session_id: row.get(11)?,
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
                turn: row.get(14)?,
                turn_message_id: row
                    .get::<_, Option<i64>>(15)?
                    .and_then(|id| u64::try_from(id).ok()),
                queued_message_id: row
                    .get::<_, Option<i64>>(16)?
                    .and_then(|id| u64::try_from(id).ok()),
                queue_position: row.get(17)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn output_path(state_dir: &Path, id: &str) -> Option<PathBuf> {
    // Only a valid ID names a file, so an ID cannot leave the folder.
    parse_id(id).ok()?;
    Some(state_dir.join(OUTPUT_DIR).join(format!("{id}.ansi")))
}

/// Records `text`, the last output of the thread `id`, as it ended.
pub(crate) fn write_output(state_dir: &Path, id: &str, text: &str) -> std::io::Result<()> {
    let path = output_path(state_dir, id)
        .ok_or_else(|| std::io::Error::other(format!("invalid thread id {id:?}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

pub(crate) fn has_output(state_dir: &Path, id: &str) -> bool {
    output_path(state_dir, id).is_some_and(|path| path.is_file())
}

/// The recorded output of the thread `id` of the workspace folder that
/// holds `dir`.
pub(crate) fn read_output_from(dir: &Path, id: &str) -> Option<String> {
    let root = dir.ancestors().find(|ancestor| {
        ancestor
            .join(super::folder::STATE_DIR)
            .join(super::folder::MANIFEST_FILE)
            .is_file()
    })?;
    std::fs::read_to_string(output_path(&root.join(super::folder::STATE_DIR), id)?).ok()
}

/// True when the agent of `thread` has a queued thread in its channel.
fn has_queued(transaction: &rusqlite::Transaction, thread: &NewThread) -> rusqlite::Result<bool> {
    transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM threads WHERE state = 'queued' AND agent = ?1 AND channel IS ?2)",
        params![thread.agent, thread.channel],
        |row| row.get(0),
    )
}

/// The queue part of an admission: the queued thread of the same agent and
/// channel takes the mention as its new anchor, or `thread` enters the
/// queue.
fn queue_in(
    transaction: &rusqlite::Transaction,
    thread: &NewThread,
    now: &str,
) -> Result<Admission, ThreadError> {
    let message = thread.origin_message_id.map(sql_message_id).transpose()?;
    let absorbed: Option<i64> = transaction
        .query_row(
            "UPDATE threads
             SET queued_message_id = max(coalesce(queued_message_id, 0), ?1), updated_at = ?2
             WHERE state = 'queued' AND agent = ?3 AND channel IS ?4
             RETURNING id",
            params![message, now, thread.agent, thread.channel],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(row) = absorbed {
        return Ok(Admission::Absorbed(format!("{ID_PREFIX}{row}")));
    }
    transaction.execute(
        "INSERT INTO threads
             (agent, cli, origin_kind, channel, origin_message_id, opener_thread_id,
              depth, isolation, state, session_id, created_at, updated_at,
              queue_seq, queued_message_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9, ?10, ?10,
                 (SELECT coalesce(max(queue_seq), 0) + 1 FROM threads), ?5)",
        params![
            thread.agent,
            thread.cli.as_str(),
            thread.origin.as_str(),
            thread.channel,
            message,
            thread.opener_thread_id,
            thread.depth,
            thread.isolation.as_str(),
            thread.session_id,
            now,
        ],
    )?;
    Ok(Admission::Queued(format!(
        "{ID_PREFIX}{}",
        transaction.last_insert_rowid()
    )))
}

fn sql_message_id(id: u64) -> Result<i64, ThreadError> {
    i64::try_from(id).map_err(|_| ThreadError(format!("invalid message id {id}")))
}

fn parse_id(id: &str) -> Result<i64, ThreadError> {
    id.strip_prefix(ID_PREFIX)
        .and_then(|number| number.parse::<i64>().ok())
        .ok_or_else(|| ThreadError(format!("invalid thread id {id:?}")))
}

/// What a running thread does after an agent state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadStep {
    Running,
    /// The agent began to wait for an approval: announce it once.
    Blocked,
    /// The agent finished its turn: end the CLI process.
    Done,
}

/// Follows the detected agent state of a running thread. The agent starts
/// with its prompt, so the thread is done at the first idle state after the
/// agent worked. A block is not work: claude asks to trust the folder and
/// goes idle for a moment before its turn starts. Herdr's own `done` status means "idle and not seen", which
/// a hidden pane always is, so it cannot tell this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ThreadProgress {
    worked: bool,
    /// The last known state is blocked. `Unknown` keeps it, so a gap in
    /// detection does not announce the same block twice.
    blocked: bool,
}

impl ThreadProgress {
    pub(crate) fn observe(&mut self, state: AgentState) -> ThreadStep {
        match state {
            AgentState::Blocked => {
                if std::mem::replace(&mut self.blocked, true) {
                    ThreadStep::Running
                } else {
                    ThreadStep::Blocked
                }
            }
            AgentState::Working => {
                self.worked = true;
                self.blocked = false;
                ThreadStep::Running
            }
            AgentState::Idle if self.worked => {
                self.blocked = false;
                ThreadStep::Done
            }
            // shortcut: an idle after a block without work never ends the thread,
            // add a turn timeout if a CLI asks for an approval before it works.
            AgentState::Idle => {
                self.blocked = false;
                ThreadStep::Running
            }
            AgentState::Unknown => ThreadStep::Running,
        }
    }

    /// The final state of a thread whose CLI process exited by itself, and
    /// the reason when it is not done.
    pub(crate) fn state_after_exit(self) -> (ThreadState, Option<&'static str>) {
        if self.blocked {
            (
                ThreadState::Failed,
                Some("the agent CLI exited while it waited for approval"),
            )
        } else if self.worked {
            (ThreadState::Done, None)
        } else {
            (
                ThreadState::Failed,
                Some("the agent CLI exited before it did any work"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regents::store::TempFolder;

    fn mention(agent: &str, message: u64) -> NewThread {
        NewThread {
            agent: agent.into(),
            cli: AgentCli::Claude,
            origin: ThreadOrigin::Channel,
            channel: Some("geral".into()),
            origin_message_id: Some(message),
            opener_thread_id: None,
            depth: 0,
            isolation: Isolation::Shared,
            session_id: Some(format!("session-{message}")),
        }
    }

    fn at(seconds: i64) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_795_802_645 + seconds).unwrap()
    }

    #[test]
    fn a_new_thread_is_active_and_reads_back() {
        let folder = TempFolder::new("threads-create");
        let registry = registry(&folder.0);

        let created = registry.create(mention("chief", 3), at(0)).unwrap();

        assert_eq!(
            created,
            ThreadRecord {
                id: "t1".into(),
                agent: "chief".into(),
                cli: Some(AgentCli::Claude),
                origin: ThreadOrigin::Channel,
                channel: Some("geral".into()),
                origin_message_id: Some(3),
                opener_thread_id: None,
                depth: 0,
                isolation: Some(Isolation::Shared),
                state: ThreadState::Active,
                end_reason: None,
                session_id: Some("session-3".into()),
                turn: 0,
                turn_message_id: Some(3),
                queue_position: None,
                queued_message_id: None,
                created_at: "2026-11-27T18:04:05Z".into(),
                updated_at: "2026-11-27T18:04:05Z".into(),
            }
        );
        assert_eq!(registry.list().unwrap(), vec![created]);
    }

    #[test]
    fn a_channel_rename_moves_its_threads() {
        let folder = TempFolder::new("threads-rename");
        let registry = registry(&folder.0);
        registry.rename_channel("geral", "anuncios").unwrap();
        assert!(!folder.0.join(DATABASE_FILE).exists());
        registry.create(mention("chief", 1), at(0)).unwrap();
        registry
            .create(
                NewThread {
                    channel: Some("design".into()),
                    ..mention("chief", 2)
                },
                at(0),
            )
            .unwrap();

        registry.rename_channel("geral", "anuncios").unwrap();

        let channels: Vec<_> = registry
            .list()
            .unwrap()
            .into_iter()
            .map(|thread| thread.channel)
            .collect();
        assert_eq!(
            channels,
            [Some("anuncios".to_owned()), Some("design".to_owned())]
        );
    }

    #[test]
    fn the_delegation_fields_read_back() {
        let folder = TempFolder::new("threads-delegation");
        let registry = registry(&folder.0);

        registry
            .create(
                NewThread {
                    opener_thread_id: Some("t1".into()),
                    depth: 2,
                    isolation: Isolation::Worktree,
                    origin: ThreadOrigin::Direct,
                    cli: AgentCli::Codex,
                    ..mention("reviewer", 4)
                },
                at(0),
            )
            .unwrap();

        let listed = registry.list().unwrap();
        assert_eq!(listed[0].opener_thread_id.as_deref(), Some("t1"));
        assert_eq!(listed[0].depth, 2);
        assert_eq!(listed[0].isolation, Some(Isolation::Worktree));
        assert_eq!(listed[0].origin, ThreadOrigin::Direct);
        assert_eq!(listed[0].cli, Some(AgentCli::Codex));
    }

    #[test]
    fn a_state_change_keeps_the_session_and_survives_a_new_open() {
        let folder = TempFolder::new("threads-state");
        let first = registry(&folder.0)
            .create(mention("chief", 1), at(0))
            .unwrap();
        let second = registry(&folder.0)
            .create(mention("reviewer", 2), at(1))
            .unwrap();

        registry(&folder.0)
            .set_state(
                &first.id,
                ThreadState::Failed,
                Some("its pane closed"),
                at(5),
            )
            .unwrap();

        let listed = registry(&folder.0).list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].state, ThreadState::Failed);
        assert_eq!(listed[0].end_reason.as_deref(), Some("its pane closed"));
        assert_eq!(listed[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(listed[0].updated_at, "2026-11-27T18:04:10Z");
        assert_eq!(listed[1], second);
    }

    #[test]
    fn the_output_of_a_thread_reads_back_from_inside_the_folder() {
        let folder = TempFolder::new("threads-output");
        let state_dir = folder.0.join(super::super::folder::STATE_DIR);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join(super::super::folder::MANIFEST_FILE), "").unwrap();
        let inside = folder.0.join("src");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(!has_output(&state_dir, "t1"));

        write_output(&state_dir, "t1", "\x1b[1mfeito\x1b[0m").unwrap();

        assert!(has_output(&state_dir, "t1"));
        assert_eq!(
            read_output_from(&inside, "t1").as_deref(),
            Some("\x1b[1mfeito\x1b[0m")
        );
        assert_eq!(read_output_from(&inside, "t2"), None);
        for invalid in ["../t1", "t1/../t1", "x"] {
            assert!(write_output(&state_dir, invalid, "x").is_err());
            assert_eq!(read_output_from(&inside, invalid), None);
        }
    }

    #[test]
    fn recent_reads_the_newest_threads_first() {
        let folder = TempFolder::new("threads-recent");
        let registry = registry(&folder.0);
        for message in 1..=3 {
            registry.create(mention("chief", message), at(0)).unwrap();
        }

        let ids = |limit| -> Vec<String> {
            registry
                .recent(limit)
                .unwrap()
                .into_iter()
                .map(|thread| thread.id)
                .collect()
        };

        assert_eq!(ids(2), ["t3", "t2"]);
        assert_eq!(ids(9), ["t3", "t2", "t1"]);
        assert_eq!(registry.list().unwrap().len(), 3);
    }

    #[test]
    fn listing_creates_no_registry() {
        let folder = TempFolder::new("threads-lazy");

        assert_eq!(registry(&folder.0).list().unwrap(), vec![]);
        assert!(!folder.0.join(DATABASE_FILE).exists());
    }

    #[test]
    fn an_unknown_thread_cannot_change_state() {
        let folder = TempFolder::new("threads-unknown");
        registry(&folder.0)
            .create(mention("chief", 1), at(0))
            .unwrap();

        for id in ["t9", "9", "x1"] {
            assert!(registry(&folder.0)
                .set_state(id, ThreadState::Done, None, at(1))
                .is_err());
        }
    }

    #[test]
    fn a_resume_is_a_compare_and_set_of_a_done_turn_through_the_queue() {
        let folder = TempFolder::new("threads-resume");
        let registry = registry(&folder.0);
        let created = registry.create(mention("chief", 1), at(0)).unwrap();
        assert_eq!(
            registry.admit_resume(&created.id, 0, 4, at(1)).unwrap(),
            None,
            "active"
        );
        registry
            .set_state(&created.id, ThreadState::Done, None, at(2))
            .unwrap();

        let queued = registry
            .admit_resume(&created.id, 0, 4, at(3))
            .unwrap()
            .unwrap();
        assert_eq!(queued.state, ThreadState::Queued);
        assert_eq!(queued.queue_position, Some(1));
        assert_eq!((queued.turn, queued.turn_message_id), (0, Some(1)));
        assert_eq!(queued.queued_message_id, Some(4));
        assert_eq!(
            registry.admit_resume(&created.id, 0, 5, at(4)).unwrap(),
            None,
            "queued"
        );
        assert_eq!(registry.admit_resume("t9", 0, 5, at(4)).unwrap(), None);

        let (before, resumed) = registry.promote_next(1, at(5)).unwrap().unwrap();

        assert_eq!(before, queued);
        assert_eq!(resumed.state, ThreadState::Active);
        assert_eq!((resumed.turn, resumed.turn_message_id), (1, Some(4)));
        assert_eq!(resumed.queued_message_id, None);
        assert_eq!(resumed.origin_message_id, Some(1));
        assert_eq!(resumed.session_id, created.session_id);

        registry.undo_resume(&before, at(6)).unwrap();
        let undone = registry.get(&created.id).unwrap().unwrap();
        assert_eq!(undone.state, ThreadState::Done);
        assert_eq!((undone.turn, undone.turn_message_id), (0, Some(1)));
    }

    #[test]
    fn the_queue_is_fifo_under_the_limit_and_a_mention_is_absorbed_once_per_pair() {
        let folder = TempFolder::new("threads-queue");
        let registry = registry(&folder.0);
        let admit = |agent: &str, channel: &str, message| {
            registry
                .admit_mention(
                    NewThread {
                        channel: Some(channel.into()),
                        ..mention(agent, message)
                    },
                    None,
                    at(0),
                )
                .unwrap()
        };

        assert_eq!(admit("chief", "geral", 1), Admission::Queued("t1".into()));
        assert_eq!(
            admit("reviewer", "geral", 2),
            Admission::Queued("t2".into())
        );
        assert_eq!(admit("chief", "dev", 3), Admission::Queued("t3".into()));
        assert_eq!(admit("chief", "geral", 4), Admission::Absorbed("t1".into()));
        let positions: Vec<_> = registry
            .list()
            .unwrap()
            .iter()
            .map(|thread| (thread.queue_position, thread.queued_message_id))
            .collect();
        assert_eq!(
            positions,
            [(Some(1), Some(4)), (Some(2), Some(2)), (Some(3), Some(3))]
        );
        assert_eq!(registry.list().unwrap()[0].origin_message_id, Some(1));
        assert_eq!(registry.list().unwrap()[0].turn_message_id, None);

        let (_, first) = registry.promote_next(1, at(1)).unwrap().unwrap();
        assert_eq!((first.id.as_str(), first.turn), ("t1", 0));
        assert_eq!(
            first.turn_message_id,
            Some(4),
            "the turn starts at the anchor"
        );
        assert_eq!(
            registry.promote_next(1, at(1)).unwrap(),
            None,
            "at the limit"
        );
        assert_eq!(registry.get("t2").unwrap().unwrap().queue_position, Some(1));
        let (_, second) = registry.promote_next(2, at(1)).unwrap().unwrap();
        assert_eq!(second.id, "t2");
        assert_eq!(admit("chief", "geral", 5), Admission::Queued("t4".into()));
        assert_eq!(registry.get("t3").unwrap().unwrap().queue_position, Some(1));
        assert_eq!(registry.get("t4").unwrap().unwrap().queue_position, Some(2));
        assert_eq!(
            registry.queued_thread("chief", "geral").unwrap().as_deref(),
            Some("t4")
        );
        let direct = registry
            .create(
                NewThread {
                    origin: ThreadOrigin::Direct,
                    channel: None,
                    ..mention("chief", 9)
                },
                at(1),
            )
            .unwrap();
        assert_eq!(direct.state, ThreadState::Active);
        let (_, third) = registry.promote_next(3, at(1)).unwrap().unwrap();
        assert_eq!(third.id, "t3", "a direct conversation takes no slot");

        let failed = registry.fail_queued("the workspace closed", at(2)).unwrap();
        assert_eq!(
            failed,
            [(
                "t4".to_owned(),
                "chief".to_owned(),
                Some("geral".to_owned())
            )]
        );
        let t4 = registry.get("t4").unwrap().unwrap();
        assert_eq!(t4.state, ThreadState::Failed);
        assert_eq!(t4.end_reason.as_deref(), Some("the workspace closed"));
        assert_eq!(t4.queue_position, None);
    }

    #[test]
    fn one_interruption_wins() {
        let folder = TempFolder::new("threads-interrupt");
        let registry = registry(&folder.0);
        let created = registry.create(mention("chief", 1), at(0)).unwrap();

        assert!(registry.interrupt(&created.id, "a", at(1)).unwrap());
        assert!(!registry.interrupt(&created.id, "b", at(2)).unwrap());

        let record = registry.get(&created.id).unwrap().unwrap();
        assert_eq!(record.state, ThreadState::Interrupted);
        assert_eq!(record.end_reason.as_deref(), Some("a"));
    }

    #[test]
    fn two_concurrent_resumes_queue_a_thread_once() {
        let folder = TempFolder::new("threads-resume-race");
        let created = registry(&folder.0)
            .create(mention("chief", 1), at(0))
            .unwrap();
        registry(&folder.0)
            .set_state(&created.id, ThreadState::Done, None, at(1))
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));

        let winners: Vec<Option<u64>> = (0..8u64)
            .map(|reply| {
                let (path, barrier, id) = (folder.0.clone(), barrier.clone(), created.id.clone());
                std::thread::spawn(move || {
                    // Each writer has its own connection, as two servers would.
                    let registry = registry(&path);
                    barrier.wait();
                    registry
                        .admit_resume(&id, 0, 10 + reply, at(2))
                        .unwrap()
                        .and_then(|record| record.queued_message_id)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        let won: Vec<u64> = winners.into_iter().flatten().collect();
        assert_eq!(won.len(), 1, "{won:?}");
        let record = registry(&folder.0).get(&created.id).unwrap().unwrap();
        assert_eq!(record.state, ThreadState::Queued);
        assert_eq!(record.queued_message_id, Some(won[0]));
    }

    #[test]
    fn a_version_1_registry_reads_back_at_turn_zero_of_its_mention() {
        let folder = TempFolder::new("threads-upgrade");
        let v1 = rusqlite::Connection::open(folder.0.join(DATABASE_FILE)).unwrap();
        v1.execute_batch(
            "CREATE TABLE threads (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, agent TEXT NOT NULL, cli TEXT NOT NULL,
                 origin_kind TEXT NOT NULL DEFAULT 'channel', channel TEXT,
                 origin_message_id INTEGER, opener_thread_id TEXT,
                 depth INTEGER NOT NULL DEFAULT 0, isolation TEXT, state TEXT NOT NULL,
                 end_reason TEXT, session_id TEXT, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL);
             INSERT INTO threads (agent, cli, channel, origin_message_id, isolation, state,
                                  session_id, created_at, updated_at)
                 VALUES ('chief', 'claude', 'geral', 3, 'shared', 'done', 'sid', 'x', 'x');
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(v1);

        // Concurrent first opens migrate once.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let opens: Vec<_> = (0..4)
            .map(|_| {
                let (path, barrier) = (folder.0.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    registry(&path).list().map(|listed| listed.len())
                })
            })
            .collect();
        for open in opens {
            assert_eq!(open.join().unwrap(), Ok(1));
        }
        let listed = registry(&folder.0).list().unwrap();

        assert_eq!((listed[0].turn, listed[0].turn_message_id), (0, Some(3)));
        assert!(registry(&folder.0)
            .admit_resume("t1", 0, 5, at(0))
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_version_2_registry_gets_the_queue_and_keeps_its_threads() {
        let folder = TempFolder::new("threads-upgrade-v2");
        let v2 = rusqlite::Connection::open(folder.0.join(DATABASE_FILE)).unwrap();
        v2.execute_batch(&format!(
            "{}\n{}\n
             INSERT INTO threads (agent, cli, channel, origin_message_id, isolation, state,
                                  session_id, created_at, updated_at, turn, turn_message_id)
                 VALUES ('chief', 'claude', 'geral', 3, 'shared', 'done', 'sid', 'x', 'x', 1, 4);
             PRAGMA user_version = 2;",
            SCHEMA.steps[0], SCHEMA.steps[1]
        ))
        .unwrap();
        drop(v2);

        let listed = registry(&folder.0).list().unwrap();

        assert_eq!((listed[0].turn, listed[0].turn_message_id), (1, Some(4)));
        assert_eq!(listed[0].queue_position, None);
        assert_eq!(
            registry(&folder.0)
                .admit_mention(mention("chief", 5), None, at(0))
                .unwrap(),
            Admission::Queued("t2".into())
        );
    }

    // #17: a mention of the pair of an active thread is pending in it, once
    // per thread; another channel, a queued pair or no deadline queue.
    #[test]
    fn a_mention_of_an_active_pair_is_pending_once_per_thread() {
        let folder = TempFolder::new("threads-pending");
        let registry = registry(&folder.0);
        let active = registry.create(mention("chief", 1), at(0)).unwrap();
        let admit = |channel: &str, message, decide_by| {
            registry
                .admit_mention(
                    NewThread {
                        channel: Some(channel.into()),
                        ..mention("chief", message)
                    },
                    decide_by,
                    at(0),
                )
                .unwrap()
        };

        assert_eq!(
            admit("geral", 2, Some(300)),
            Admission::Pending(active.id.clone())
        );
        assert_eq!(
            admit("geral", 3, Some(300)),
            Admission::Queued("t2".into()),
            "one open pending mention per thread"
        );
        assert_eq!(
            admit("geral", 4, Some(300)),
            Admission::Absorbed("t2".into()),
            "the queued thread of the pair first"
        );
        assert_eq!(
            admit("dev", 5, Some(300)),
            Admission::Queued("t3".into()),
            "another channel"
        );
        let open = registry.open_pending_mentions().unwrap();
        assert_eq!(
            open,
            [PendingMention {
                id: 1,
                thread_id: active.id.clone(),
                message_id: 2,
                phase: Phase::Waiting,
                decide_by: 300,
                fork_thread_id: None,
            }]
        );
        let second_open = store::open_writable(&registry.path, &SCHEMA)
            .unwrap()
            .execute(
                "INSERT INTO pending_mentions
                     (thread_id, message_id, phase, decide_by, created_at, updated_at)
                 VALUES (1, 9, 'delivered', 0, 'x', 'x')",
                [],
            );
        assert!(second_open.is_err(), "the index holds the invariant");
        assert_eq!(
            registry.latest_pending_mention(&active.id).unwrap(),
            Some(open[0].clone())
        );
        assert_eq!(registry.latest_pending_mention("t3").unwrap(), None);
    }

    #[test]
    fn a_delivery_starts_the_next_turn_and_an_accept_anchors_it_at_the_mention() {
        let folder = TempFolder::new("threads-pending-accept");
        let registry = registry(&folder.0);
        let active = registry.create(mention("chief", 1), at(0)).unwrap();
        registry
            .admit_mention(mention("chief", 2), Some(300), at(0))
            .unwrap();
        let waiting = registry.open_pending_mentions().unwrap().remove(0);

        assert_eq!(
            registry.advance_pending(&waiting, Phase::Delivered, None, at(1)),
            Ok(true)
        );
        assert_eq!(
            registry.advance_pending(&waiting, Phase::Delivered, None, at(1)),
            Ok(false),
            "a stale phase loses"
        );
        let record = registry.get(&active.id).unwrap().unwrap();
        assert_eq!(
            (record.turn, record.turn_message_id),
            (0, Some(1)),
            "the turn waits for the CLI to get the prompt"
        );
        registry.start_decision_turn(&active.id, at(1)).unwrap();
        let record = registry.get(&active.id).unwrap().unwrap();
        assert_eq!((record.turn, record.turn_message_id), (1, Some(1)));

        let delivered = registry
            .latest_pending_mention(&active.id)
            .unwrap()
            .unwrap();
        assert!(registry
            .advance_pending(&delivered, Phase::Accepted, None, at(2))
            .unwrap());

        let record = registry.get(&active.id).unwrap().unwrap();
        assert_eq!((record.turn, record.turn_message_id), (1, Some(2)));
        assert_eq!(registry.list().unwrap().len(), 1, "no new thread");
        assert!(registry.open_pending_mentions().unwrap().is_empty());
        assert_eq!(
            registry
                .admit_mention(mention("chief", 3), Some(300), at(3))
                .unwrap(),
            Admission::Pending(active.id),
            "a decided mention frees the thread for the next one"
        );
    }

    #[test]
    fn concurrent_forks_open_one_thread() {
        let folder = TempFolder::new("threads-pending-race");
        registry(&folder.0)
            .create(mention("chief", 1), at(0))
            .unwrap();
        registry(&folder.0)
            .admit_mention(mention("chief", 2), Some(300), at(0))
            .unwrap();
        let waiting = registry(&folder.0)
            .open_pending_mentions()
            .unwrap()
            .remove(0);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));

        let won = (0..8)
            .map(|_| {
                let (path, barrier, waiting) = (folder.0.clone(), barrier.clone(), waiting.clone());
                std::thread::spawn(move || {
                    let registry = registry(&path);
                    barrier.wait();
                    registry
                        .advance_pending(&waiting, Phase::Forked, Some(mention("chief", 2)), at(1))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|&won| won)
            .count();

        assert_eq!(won, 1);
        let records = registry(&folder.0).list().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].origin_message_id, Some(2));
        assert_eq!(records[1].state, ThreadState::Queued);
        let forked = registry(&folder.0)
            .latest_pending_mention("t1")
            .unwrap()
            .unwrap();
        assert_eq!(forked.phase, Phase::Forked);
        assert_eq!(forked.fork_thread_id.as_deref(), Some("t2"));
    }

    #[test]
    fn a_thread_is_done_at_the_first_idle_after_work() {
        let mut progress = ThreadProgress::default();

        assert_eq!(progress.observe(AgentState::Unknown), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Idle), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Working), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Blocked), ThreadStep::Blocked);
        assert_eq!(progress.observe(AgentState::Idle), ThreadStep::Done);
    }

    #[test]
    fn an_idle_after_a_block_without_work_is_not_the_end_of_the_turn() {
        let mut progress = ThreadProgress::default();

        // claude 2.1.332: the folder trust prompt, then idle, then the turn.
        assert_eq!(progress.observe(AgentState::Blocked), ThreadStep::Blocked);
        assert_eq!(progress.observe(AgentState::Idle), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Blocked), ThreadStep::Blocked);
        assert_eq!(progress.observe(AgentState::Working), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Idle), ThreadStep::Done);
    }

    #[test]
    fn each_entry_into_blocked_is_announced_once() {
        let mut progress = ThreadProgress::default();
        let steps: Vec<ThreadStep> = [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Blocked,
            AgentState::Unknown,
            AgentState::Blocked,
            AgentState::Working,
            AgentState::Blocked,
        ]
        .into_iter()
        .map(|state| progress.observe(state))
        .collect();

        let announced = steps
            .iter()
            .filter(|step| **step == ThreadStep::Blocked)
            .count();
        assert_eq!(announced, 2, "{steps:?}");
    }

    #[test]
    fn an_exit_while_blocked_is_a_failure() {
        let mut progress = ThreadProgress::default();
        progress.observe(AgentState::Working);
        progress.observe(AgentState::Blocked);

        let (state, reason) = progress.state_after_exit();
        assert_eq!(state, ThreadState::Failed);
        assert_eq!(
            reason,
            Some("the agent CLI exited while it waited for approval")
        );

        progress.observe(AgentState::Working);
        assert_eq!(progress.state_after_exit(), (ThreadState::Done, None));
    }

    #[test]
    fn an_exit_before_any_work_is_a_failure() {
        let mut progress = ThreadProgress::default();
        let (state, reason) = progress.state_after_exit();
        assert_eq!(state, ThreadState::Failed);
        assert!(reason.is_some());

        progress.observe(AgentState::Working);

        assert_eq!(progress.state_after_exit(), (ThreadState::Done, None));
    }
}
