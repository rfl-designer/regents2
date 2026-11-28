//! The thread registry of a workspace (Regents, ADR-0003):
//! `.regents/threads.sqlite`, and the pure rule that tells when a running
//! thread is done. The server is the only writer of the registry.

use std::path::{Path, PathBuf};

use rusqlite::params;

use super::agents::{AgentCli, Isolation};
use super::store;
use crate::detect::AgentState;

const DATABASE_FILE: &str = "threads.sqlite";
/// Folder of `.regents` with the last output of each ended thread, one
/// `<id>.ansi` file per thread.
const OUTPUT_DIR: &str = "thread-output";
const ID_PREFIX: &str = "t";
const SCHEMA: store::Schema = store::Schema {
    version: 1,
    sql: "CREATE TABLE IF NOT EXISTS threads (
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
};

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
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
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
    /// Records a new active thread.
    pub(crate) fn create(
        &self,
        thread: NewThread,
        now: time::OffsetDateTime,
    ) -> Result<ThreadRecord, ThreadError> {
        let now = super::utc_seconds(now).map_err(ThreadError)?;
        let origin_message = thread
            .origin_message_id
            .map(|id| {
                i64::try_from(id).map_err(|_| ThreadError(format!("invalid message id {id}")))
            })
            .transpose()?;
        let connection = store::open_writable(&self.path, &SCHEMA)?;
        connection.execute(
            "INSERT INTO threads
                 (agent, cli, origin_kind, channel, origin_message_id, opener_thread_id, depth,
                  isolation, state, session_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
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
            created_at: now.clone(),
            updated_at: now,
        })
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

    /// Threads in `order`, at most `limit` (-1: all).
    fn query(&self, order: &str, limit: i64) -> Result<Vec<ThreadRecord>, ThreadError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = store::open_read_only(&self.path)?;
        let mut statement = connection.prepare(&format!(
            "SELECT id, agent, cli, origin_kind, channel, origin_message_id, opener_thread_id,
                    depth, isolation, state, end_reason, session_id, created_at, updated_at
             FROM threads {order} LIMIT ?1"
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

fn parse_id(id: &str) -> Result<i64, ThreadError> {
    id.strip_prefix(ID_PREFIX)
        .and_then(|number| number.parse::<i64>().ok())
        .ok_or_else(|| ThreadError(format!("invalid thread id {id:?}")))
}

/// What a running thread does after an agent state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadStep {
    Running,
    /// The agent finished its turn: end the CLI process.
    Done,
}

/// Follows the detected agent state of a running thread. The agent starts
/// with its prompt, so the thread is done at the first idle state after the
/// agent worked. Herdr's own `done` status means "idle and not seen", which
/// a hidden pane always is, so it cannot tell this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ThreadProgress {
    worked: bool,
}

impl ThreadProgress {
    pub(crate) fn observe(&mut self, state: AgentState) -> ThreadStep {
        match state {
            AgentState::Working | AgentState::Blocked => {
                self.worked = true;
                ThreadStep::Running
            }
            AgentState::Idle if self.worked => ThreadStep::Done,
            AgentState::Idle | AgentState::Unknown => ThreadStep::Running,
        }
    }

    /// The final state of a thread whose CLI process exited by itself, and
    /// the reason when it is not done.
    pub(crate) fn state_after_exit(self) -> (ThreadState, Option<&'static str>) {
        if self.worked {
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
    fn a_thread_is_done_at_the_first_idle_after_work() {
        let mut progress = ThreadProgress::default();

        assert_eq!(progress.observe(AgentState::Unknown), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Idle), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Working), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Blocked), ThreadStep::Running);
        assert_eq!(progress.observe(AgentState::Idle), ThreadStep::Done);
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
