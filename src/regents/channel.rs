//! Channel messages (Regents, ADR-0003). Each channel is a subfolder of
//! `.regents/channels/`, and its messages live in a SQLite database in that
//! subfolder. The database appears on the first message. The server is the
//! only process that writes it; agents and clients post through the API.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::store;

const CHANNELS_DIR: &str = "channels";
const DATABASE_FILE: &str = "channel.sqlite";
/// The SQL brings a database of any older version to this one: the table
/// keeps its version 1 form and each version appends its changes.
const SCHEMA: store::Schema = store::Schema {
    version: 2,
    sql: "CREATE TABLE IF NOT EXISTS messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             author_kind TEXT NOT NULL,
             author_id TEXT NOT NULL,
             body TEXT NOT NULL,
             reply_to INTEGER,
             origin_thread_id TEXT,
             -- JSON array of the mentioned agent names.
             mentions TEXT NOT NULL DEFAULT '[]',
             created_at TEXT NOT NULL
         );
         -- Version 2: `<thread>#<turn>` of a captured reply, NULL for a post.
         ALTER TABLE messages ADD COLUMN capture_key TEXT;
         CREATE UNIQUE INDEX messages_capture_key ON messages(capture_key);",
};

/// The channel that a chat pane shows: the pane runs the chat view of this
/// channel (`regents::chat_panes`), and a restart restores it in the same
/// place of the layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct ChannelView {
    /// Stable ID of the workspace folder, from `workspace.toml`.
    pub(crate) folder_id: String,
    pub(crate) channel: String,
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorKind {
    Human,
    Agent,
    System,
    /// A kind written by a newer Regents.
    Unknown,
}

impl AuthorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "human" => Self::Human,
            "agent" => Self::Agent,
            "system" => Self::System,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Author {
    pub(crate) kind: AuthorKind,
    /// Identification of the author: the user name for a human, the agent
    /// name for an agent.
    pub(crate) id: String,
}

/// A message to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewMessage {
    pub(crate) author: Author,
    pub(crate) body: String,
    /// Message of the same channel that this message replies to.
    pub(crate) reply_to: Option<u64>,
    /// Thread that produced the message (agent messages).
    pub(crate) origin_thread_id: Option<String>,
    /// Names of the agents that the message mentions.
    pub(crate) mentions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    /// Increases with each message of the channel.
    pub(crate) id: u64,
    pub(crate) channel: String,
    pub(crate) author: Author,
    pub(crate) body: String,
    pub(crate) reply_to: Option<u64>,
    pub(crate) origin_thread_id: Option<String>,
    pub(crate) mentions: Vec<String>,
    /// RFC 3339 time in UTC, in whole seconds.
    pub(crate) created_at: String,
    /// The Regents copied the message from the reply of a thread that did not
    /// post (ADR-0004); false for a post.
    pub(crate) captured: bool,
}

/// The reply of a thread turn that posted nothing, to post as its agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    pub(crate) agent: String,
    pub(crate) thread_id: String,
    /// Index of the turn in the CLI session.
    pub(crate) turn: u32,
    /// The message that started the turn: a post of the thread after it
    /// is the reply of the turn.
    pub(crate) after: u64,
    pub(crate) body: String,
}

/// Which messages `Channel::read` returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReadRange {
    /// Only messages older than this message ID.
    pub(crate) before: Option<u64>,
    /// At most this many messages, the most recent ones of the range.
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelError {
    /// The name is not a slug (lowercase letters, digits and single hyphens).
    InvalidName(String),
    NotFound(String),
    /// A channel with this name exists.
    AlreadyExists(String),
    EmptyBody,
    /// `reply_to` names no message of the channel.
    ReplyNotFound {
        channel: String,
        id: u64,
    },
    /// A message ID that no message can have.
    InvalidMessageId(u64),
    Storage(String),
}

impl ChannelError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidName(_) => "invalid_channel_name",
            Self::NotFound(_) => "channel_not_found",
            Self::AlreadyExists(_) => "channel_exists",
            Self::EmptyBody => "invalid_message",
            Self::ReplyNotFound { .. } => "message_not_found",
            Self::InvalidMessageId(_) => "invalid_params",
            Self::Storage(_) => "channel_storage_error",
        }
    }
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "invalid channel name {name:?}: use lowercase letters, digits and hyphens"
            ),
            Self::NotFound(name) => write!(f, "channel #{name} does not exist in this workspace"),
            Self::AlreadyExists(name) => {
                write!(f, "channel #{name} already exists in this workspace")
            }
            Self::EmptyBody => write!(f, "the message is empty"),
            Self::ReplyNotFound { channel, id } => {
                write!(f, "message {id} does not exist in channel #{channel}")
            }
            Self::InvalidMessageId(id) => write!(f, "invalid message id {id}"),
            Self::Storage(message) => write!(f, "channel storage error: {message}"),
        }
    }
}

impl From<rusqlite::Error> for ChannelError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

/// An existing channel of a workspace.
#[derive(Debug, Clone)]
pub(crate) struct Channel {
    name: String,
    dir: PathBuf,
}

/// Opens the channel `name` of the workspace whose state directory is
/// `state_dir` (`<folder>/.regents`). It creates no file.
pub(crate) fn open(state_dir: &Path, name: &str) -> Result<Channel, ChannelError> {
    if !is_slug(name) {
        return Err(ChannelError::InvalidName(name.to_owned()));
    }
    let dir = state_dir.join(CHANNELS_DIR).join(name);
    if !dir.is_dir() {
        return Err(ChannelError::NotFound(name.to_owned()));
    }
    Ok(Channel {
        name: name.to_owned(),
        dir,
    })
}

/// Creates the channel `name`: an empty subfolder of `channels/`. The
/// database appears with the first message.
pub(crate) fn create(state_dir: &Path, name: &str) -> Result<Channel, ChannelError> {
    if !is_slug(name) {
        return Err(ChannelError::InvalidName(name.to_owned()));
    }
    let parent = state_dir.join(CHANNELS_DIR);
    std::fs::create_dir_all(&parent).map_err(storage_error)?;
    let dir = parent.join(name);
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok(Channel {
            name: name.to_owned(),
            dir,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ChannelError::AlreadyExists(name.to_owned()))
        }
        Err(err) => Err(storage_error(err)),
    }
}

/// Renames the channel `from` to `to`. The subfolder moves with its
/// database, so the history stays.
pub(crate) fn rename(state_dir: &Path, from: &str, to: &str) -> Result<Channel, ChannelError> {
    let channel = open(state_dir, from)?;
    if !is_slug(to) {
        return Err(ChannelError::InvalidName(to.to_owned()));
    }
    let dir = state_dir.join(CHANNELS_DIR).join(to);
    // A Unix rename replaces an empty folder, so check first. The server is
    // the only writer of `.regents/`.
    if dir.exists() {
        return Err(ChannelError::AlreadyExists(to.to_owned()));
    }
    std::fs::rename(&channel.dir, &dir).map_err(storage_error)?;
    Ok(Channel {
        name: to.to_owned(),
        dir,
    })
}

fn storage_error(err: std::io::Error) -> ChannelError {
    ChannelError::Storage(err.to_string())
}

/// Names of the channels of the workspace whose state directory is
/// `state_dir`, in name order: the subfolders of `channels/` whose name is a
/// slug. It reads one directory; keep it out of render and layout paths.
pub(crate) fn list(state_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(state_dir.join(CHANNELS_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_slug(name))
        .collect();
    names.sort();
    names
}

/// A channel name: lowercase ASCII letters and digits in groups separated by
/// one hyphen, for example `geral` or `code-review`.
pub(crate) fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

impl Channel {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    fn database_path(&self) -> PathBuf {
        self.dir.join(DATABASE_FILE)
    }

    /// Appends a message. The first message creates the database; a post
    /// that fails leaves no database behind.
    pub(crate) fn post(
        &self,
        message: NewMessage,
        now: time::OffsetDateTime,
    ) -> Result<Message, ChannelError> {
        self.append(message, now, None)?
            .ok_or_else(|| ChannelError::Storage("a post without a capture key was skipped".into()))
    }

    /// Posts `capture` as a message of its agent, with no mentions, unless
    /// the thread posted after `capture.after` or this turn is already
    /// captured. One transaction checks and inserts, so a late post and the
    /// capture never make two messages. `None` when it posted nothing.
    pub(crate) fn post_capture(
        &self,
        capture: Capture,
        now: time::OffsetDateTime,
    ) -> Result<Option<Message>, ChannelError> {
        let key = format!("{}#{}", capture.thread_id, capture.turn);
        let message = NewMessage {
            author: Author {
                kind: AuthorKind::Agent,
                id: capture.agent,
            },
            body: capture.body,
            reply_to: None,
            origin_thread_id: Some(capture.thread_id),
            mentions: Vec::new(),
        };
        self.append(message, now, Some((key, to_sql_id(capture.after)?)))
    }

    /// Appends `message`; with a capture key and a message ID, only when
    /// the thread posted nothing after that message and the key is new.
    fn append(
        &self,
        message: NewMessage,
        now: time::OffsetDateTime,
        capture: Option<(String, i64)>,
    ) -> Result<Option<Message>, ChannelError> {
        if message.body.trim().is_empty() {
            return Err(ChannelError::EmptyBody);
        }
        let reply_to = message.reply_to.map(to_sql_id).transpose()?;
        let created_at = super::utc_seconds(now).map_err(ChannelError::Storage)?;
        let mentions = serde_json::to_string(&message.mentions)
            .map_err(|err| ChannelError::Storage(err.to_string()))?;
        let path = self.database_path();
        let existed = path.exists();
        if let (Some(id), false) = (message.reply_to, existed) {
            // Without a database the channel has no message to reply to.
            return Err(self.reply_not_found(id));
        }
        let inserted = self.insert(
            &path,
            &message,
            reply_to,
            &mentions,
            &created_at,
            capture.as_ref(),
        );
        if inserted.is_err() && !existed {
            remove_database(&path);
        }
        Ok(inserted?.map(|id| Message {
            id,
            channel: self.name.clone(),
            author: message.author,
            body: message.body,
            reply_to: message.reply_to,
            origin_thread_id: message.origin_thread_id,
            mentions: message.mentions,
            created_at,
            captured: capture.is_some(),
        }))
    }

    fn insert(
        &self,
        path: &Path,
        message: &NewMessage,
        reply_to: Option<i64>,
        mentions: &str,
        created_at: &str,
        capture: Option<&(String, i64)>,
    ) -> Result<Option<u64>, ChannelError> {
        let mut connection = store::open_writable(path, &SCHEMA)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((key, after)) = capture {
            let answered = transaction
                .query_row(
                    "SELECT 1 FROM messages WHERE origin_thread_id = ?1
                         AND ((capture_key IS NULL AND id > ?2) OR capture_key = ?3)",
                    params![message.origin_thread_id, after, key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if answered {
                return Ok(None);
            }
        }
        if let (Some(id), Some(sql_id)) = (message.reply_to, reply_to) {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM messages WHERE id = ?1",
                    params![sql_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(self.reply_not_found(id));
            }
        }
        transaction.execute(
            "INSERT INTO messages
                 (author_kind, author_id, body, reply_to, origin_thread_id, mentions, created_at,
                  capture_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message.author.kind.as_str(),
                message.author.id,
                message.body,
                reply_to,
                message.origin_thread_id,
                mentions,
                created_at,
                capture.map(|(key, _)| key),
            ],
        )?;
        let id = from_sql_id(transaction.last_insert_rowid())?;
        transaction.commit()?;
        Ok(Some(id))
    }

    fn reply_not_found(&self, id: u64) -> ChannelError {
        ChannelError::ReplyNotFound {
            channel: self.name.clone(),
            id,
        }
    }

    /// Messages of the channel in the order of posting. A channel without
    /// messages has no database yet; reading it creates none.
    pub(crate) fn read(&self, range: ReadRange) -> Result<Vec<Message>, ChannelError> {
        let before = range.before.map(to_sql_id).transpose()?.unwrap_or(i64::MAX);
        let limit = range.limit.map(i64::from).unwrap_or(-1);
        let path = self.database_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        // Only the server reads a channel, so a read can bring an older
        // database to the current schema.
        let connection = store::open_writable(&path, &SCHEMA)?;
        // The most recent `limit` messages before `before`, oldest first.
        let mut statement = connection.prepare(
            "SELECT id, author_kind, author_id, body, reply_to, origin_thread_id, mentions,
                    created_at, capture_key IS NOT NULL
             FROM (SELECT * FROM messages WHERE id < ?1 ORDER BY id DESC LIMIT ?2)
             ORDER BY id ASC",
        )?;
        let rows = statement.query_map(params![before, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                author_id,
                body,
                reply_to,
                origin_thread_id,
                mentions,
                created_at,
                captured,
            ) = row?;
            let mentions = serde_json::from_str(&mentions)
                .map_err(|err| ChannelError::Storage(format!("invalid mentions: {err}")))?;
            messages.push(Message {
                id: from_sql_id(id)?,
                channel: self.name.clone(),
                author: Author {
                    kind: AuthorKind::parse(&kind),
                    id: author_id,
                },
                body,
                reply_to: reply_to.map(from_sql_id).transpose()?,
                origin_thread_id,
                mentions,
                created_at,
                captured,
            });
        }
        Ok(messages)
    }
}

/// Removes a database that a failed first post created.
fn remove_database(path: &Path) {
    for file in [path.to_path_buf(), path.with_extension("sqlite-journal")] {
        if let Err(err) = std::fs::remove_file(&file) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %file.display(), %err, "cannot remove a new channel database");
            }
        }
    }
}

/// A message ID from a client. SQLite IDs are signed.
fn to_sql_id(id: u64) -> Result<i64, ChannelError> {
    i64::try_from(id).map_err(|_| ChannelError::InvalidMessageId(id))
}

fn from_sql_id(id: i64) -> Result<u64, ChannelError> {
    u64::try_from(id).map_err(|_| ChannelError::Storage(format!("invalid message id {id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace state directory with the channel `geral`.
    struct Folder {
        _root: store::TempFolder,
        state: PathBuf,
    }

    impl Folder {
        fn new(name: &str) -> Self {
            let root = store::TempFolder::new(&format!("channel-{name}"));
            let state = root.0.join(".regents");
            std::fs::create_dir_all(state.join("channels").join("geral")).unwrap();
            Self { _root: root, state }
        }
    }

    fn human(body: &str) -> NewMessage {
        NewMessage {
            author: Author {
                kind: AuthorKind::Human,
                id: "rafael".into(),
            },
            body: body.into(),
            reply_to: None,
            origin_thread_id: None,
            mentions: Vec::new(),
        }
    }

    /// 2026-11-27T15:04:05.25-03:00, the same instant as 2026-11-27T18:04:05.25Z.
    fn afternoon_in_sao_paulo() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp_nanos(1_795_802_645_250_000_000)
            .unwrap()
            .to_offset(time::UtcOffset::from_hms(-3, 0, 0).unwrap())
    }

    #[test]
    fn a_posted_message_reads_back_with_its_author_and_time() {
        let folder = Folder::new("post");
        let channel = open(&folder.state, "geral").unwrap();

        let posted = channel.post(human("oi"), afternoon_in_sao_paulo()).unwrap();

        let expected = Message {
            id: 1,
            channel: "geral".into(),
            author: Author {
                kind: AuthorKind::Human,
                id: "rafael".into(),
            },
            body: "oi".into(),
            reply_to: None,
            origin_thread_id: None,
            mentions: Vec::new(),
            created_at: "2026-11-27T18:04:05Z".into(),
            captured: false,
        };
        assert_eq!(posted, expected);
        assert_eq!(channel.read(ReadRange::default()).unwrap(), vec![expected]);
    }

    #[test]
    fn the_database_appears_only_with_the_first_message() {
        let folder = Folder::new("lazy");
        let channel = open(&folder.state, "geral").unwrap();
        let database = folder.state.join("channels/geral/channel.sqlite");

        assert_eq!(channel.read(ReadRange::default()).unwrap(), vec![]);
        assert!(!database.exists());

        channel.post(human("oi"), afternoon_in_sao_paulo()).unwrap();

        assert!(database.is_file());
    }

    #[test]
    fn messages_survive_a_new_open_in_posting_order() {
        let folder = Folder::new("reopen");
        let now = afternoon_in_sao_paulo();
        let first = open(&folder.state, "geral").unwrap();
        first.post(human("um"), now).unwrap();
        first.post(human("dois"), now).unwrap();

        let bodies: Vec<String> = open(&folder.state, "geral")
            .unwrap()
            .read(ReadRange::default())
            .unwrap()
            .into_iter()
            .map(|message| message.body)
            .collect();

        assert_eq!(bodies, ["um", "dois"]);
    }

    #[test]
    fn read_returns_the_most_recent_messages_before_an_id() {
        let folder = Folder::new("range");
        let channel = open(&folder.state, "geral").unwrap();
        for body in ["1", "2", "3", "4", "5"] {
            channel.post(human(body), afternoon_in_sao_paulo()).unwrap();
        }
        let bodies = |range| -> Vec<String> {
            channel
                .read(range)
                .unwrap()
                .into_iter()
                .map(|message| message.body)
                .collect()
        };

        assert_eq!(
            bodies(ReadRange {
                before: None,
                limit: Some(2)
            }),
            ["4", "5"]
        );
        assert_eq!(
            bodies(ReadRange {
                before: Some(4),
                limit: Some(2)
            }),
            ["2", "3"]
        );
        assert_eq!(
            bodies(ReadRange {
                before: Some(3),
                limit: None
            }),
            ["1", "2"]
        );
    }

    #[test]
    fn the_model_keeps_the_author_kind_reply_and_origin_thread() {
        let folder = Folder::new("model");
        let channel = open(&folder.state, "geral").unwrap();
        let now = afternoon_in_sao_paulo();
        let question = channel.post(human("pergunta"), now).unwrap();

        let answer = channel
            .post(
                NewMessage {
                    author: Author {
                        kind: AuthorKind::Agent,
                        id: "chief-of-staff".into(),
                    },
                    body: "resposta".into(),
                    reply_to: Some(question.id),
                    origin_thread_id: Some("t1".into()),
                    mentions: vec!["reviewer".into()],
                },
                now,
            )
            .unwrap();
        let note = channel
            .post(
                NewMessage {
                    author: Author {
                        kind: AuthorKind::System,
                        id: "regents".into(),
                    },
                    ..human("aviso")
                },
                now,
            )
            .unwrap();

        let read = channel.read(ReadRange::default()).unwrap();
        assert_eq!(read, vec![question, answer.clone(), note]);
        assert_eq!(answer.reply_to, Some(1));
        assert_eq!(answer.origin_thread_id.as_deref(), Some("t1"));
        assert_eq!(read[1].mentions, ["reviewer"]);
        assert!(read[0].mentions.is_empty());
        assert_eq!(read[2].author.kind, AuthorKind::System);
    }

    #[test]
    fn a_reply_to_a_missing_message_is_refused() {
        let folder = Folder::new("reply");
        let channel = open(&folder.state, "geral").unwrap();

        let err = channel
            .post(
                NewMessage {
                    reply_to: Some(7),
                    ..human("oi")
                },
                afternoon_in_sao_paulo(),
            )
            .unwrap_err();

        assert_eq!(err.code(), "message_not_found");
        assert_eq!(channel.read(ReadRange::default()).unwrap(), vec![]);
        assert!(!folder.state.join("channels/geral/channel.sqlite").exists());
    }

    #[test]
    fn a_message_id_beyond_the_storage_range_is_invalid_input() {
        let folder = Folder::new("overflow");
        let channel = open(&folder.state, "geral").unwrap();
        channel.post(human("oi"), afternoon_in_sao_paulo()).unwrap();

        let reply = channel
            .post(
                NewMessage {
                    reply_to: Some(u64::MAX),
                    ..human("oi")
                },
                afternoon_in_sao_paulo(),
            )
            .unwrap_err();
        let read = channel
            .read(ReadRange {
                before: Some(u64::MAX),
                limit: None,
            })
            .unwrap_err();

        assert_eq!(reply.code(), "invalid_params");
        assert_eq!(read.code(), "invalid_params");
    }

    #[test]
    fn an_empty_message_is_refused() {
        let folder = Folder::new("empty");
        let channel = open(&folder.state, "geral").unwrap();

        let err = channel
            .post(human("  \n"), afternoon_in_sao_paulo())
            .unwrap_err();

        assert_eq!(err.code(), "invalid_message");
        assert!(!folder.state.join("channels/geral/channel.sqlite").exists());
    }

    #[test]
    fn a_missing_channel_is_not_found_and_creates_nothing() {
        let folder = Folder::new("missing");

        let err = open(&folder.state, "outro").unwrap_err();

        assert_eq!(err.code(), "channel_not_found");
        assert!(err.to_string().contains("#outro"), "{err}");
        assert!(!folder.state.join("channels/outro").exists());
    }

    #[test]
    fn a_name_that_is_not_a_slug_is_refused() {
        let folder = Folder::new("slug");

        for name in ["", "Geral", "a b", "../geral", "a--b", "-a", "a-", "ã"] {
            let err = open(&folder.state, name).unwrap_err();
            assert_eq!(err.code(), "invalid_channel_name", "{name:?}");
        }
        assert!(is_slug("code-review2"));
    }

    #[test]
    fn list_names_the_channel_folders_in_order() {
        let folder = Folder::new("list");
        for name in ["design", "Not-A-Slug", "ops"] {
            std::fs::create_dir_all(folder.state.join("channels").join(name)).unwrap();
        }
        std::fs::write(folder.state.join("channels").join("notes"), "a file").unwrap();

        assert_eq!(list(&folder.state), ["design", "geral", "ops"]);
    }

    #[test]
    fn a_created_channel_is_listed_and_a_second_create_is_refused() {
        let folder = Folder::new("create");

        let created = create(&folder.state, "deploy").unwrap();

        assert_eq!(created.name(), "deploy");
        assert_eq!(list(&folder.state), ["deploy", "geral"]);
        assert_eq!(
            create(&folder.state, "deploy").unwrap_err().code(),
            "channel_exists"
        );
    }

    #[test]
    fn create_refuses_a_name_that_is_not_a_slug() {
        let folder = Folder::new("create-slug");

        for name in ["Deploy", "a b", "../x", ""] {
            let err = create(&folder.state, name).unwrap_err();
            assert_eq!(err.code(), "invalid_channel_name", "{name:?}");
        }
        assert_eq!(list(&folder.state), ["geral"]);
    }

    #[test]
    fn a_renamed_channel_keeps_its_history_under_the_new_name() {
        let folder = Folder::new("rename");
        let deploy = create(&folder.state, "deploy").unwrap();
        deploy
            .post(human("antes"), afternoon_in_sao_paulo())
            .unwrap();

        let release = rename(&folder.state, "deploy", "release").unwrap();

        assert_eq!(list(&folder.state), ["geral", "release"]);
        let read = release.read(ReadRange::default()).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].body, "antes");
        assert_eq!(read[0].channel, "release");
        assert_eq!(
            open(&folder.state, "deploy").unwrap_err().code(),
            "channel_not_found"
        );
    }

    #[test]
    fn rename_refuses_a_taken_name_a_bad_name_and_a_missing_channel() {
        let folder = Folder::new("rename-refused");
        create(&folder.state, "deploy").unwrap();

        let code = |from, to| rename(&folder.state, from, to).unwrap_err().code();

        assert_eq!(code("deploy", "geral"), "channel_exists");
        assert_eq!(code("deploy", "Release"), "invalid_channel_name");
        assert_eq!(code("outro", "release"), "channel_not_found");
        assert_eq!(list(&folder.state), ["deploy", "geral"]);
    }

    fn capture(thread_id: &str, turn: u32, after: u64, body: &str) -> Capture {
        Capture {
            agent: "chief".into(),
            thread_id: thread_id.into(),
            turn,
            after,
            body: body.into(),
        }
    }

    fn from_thread(thread_id: &str, body: &str) -> NewMessage {
        NewMessage {
            author: Author {
                kind: AuthorKind::Agent,
                id: "chief".into(),
            },
            origin_thread_id: Some(thread_id.into()),
            ..human(body)
        }
    }

    #[test]
    fn a_capture_posts_once_per_turn_as_the_agent_without_mentions() {
        let folder = Folder::new("capture-turns");
        let channel = open(&folder.state, "geral").unwrap();
        let mention = channel
            .post(human("@chief oi"), afternoon_in_sao_paulo())
            .unwrap();
        let now = afternoon_in_sao_paulo();

        let first = channel
            .post_capture(capture("t1", 0, mention.id, "feito @reviewer"), now)
            .unwrap()
            .unwrap();
        let again = channel
            .post_capture(capture("t1", 0, mention.id, "feito @reviewer"), now)
            .unwrap();
        let second_turn = channel
            .post_capture(capture("t1", 1, mention.id, "segundo"), now)
            .unwrap();

        assert_eq!(first.author.kind, AuthorKind::Agent);
        assert_eq!(first.author.id, "chief");
        assert_eq!(first.origin_thread_id.as_deref(), Some("t1"));
        assert!(first.mentions.is_empty());
        assert!(first.captured);
        assert_eq!(again, None);
        assert_eq!(
            second_turn.map(|message| message.body).as_deref(),
            Some("segundo")
        );
        let read = channel.read(ReadRange::default()).unwrap();
        assert_eq!(
            read.iter()
                .map(|message| message.captured)
                .collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(read[1], first);
    }

    #[test]
    fn a_post_of_the_thread_after_the_turn_start_skips_the_capture() {
        let folder = Folder::new("capture-posted");
        let channel = open(&folder.state, "geral").unwrap();
        let now = afternoon_in_sao_paulo();
        let mention = channel.post(human("@chief oi"), now).unwrap();
        channel
            .post(from_thread("t2", "outra thread"), now)
            .unwrap();

        let unanswered = channel
            .post_capture(capture("t1", 0, mention.id, "captura"), now)
            .unwrap();
        channel.post(from_thread("t3", "resposta"), now).unwrap();
        let answered = channel
            .post_capture(capture("t3", 0, mention.id, "captura"), now)
            .unwrap();
        let older_post = channel
            .post_capture(capture("t3", 0, 99, "captura"), now)
            .unwrap();

        assert!(unanswered.is_some(), "a post of another thread counts not");
        assert_eq!(answered, None);
        assert!(
            older_post.is_some(),
            "a post before the turn start counts not"
        );
    }

    #[test]
    fn a_version_1_database_reads_back_and_takes_captures() {
        let folder = Folder::new("capture-upgrade");
        let path = folder
            .state
            .join("channels")
            .join("geral")
            .join(DATABASE_FILE);
        let v1 = rusqlite::Connection::open(&path).unwrap();
        v1.execute_batch(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, author_kind TEXT NOT NULL,
                 author_id TEXT NOT NULL, body TEXT NOT NULL, reply_to INTEGER,
                 origin_thread_id TEXT, mentions TEXT NOT NULL DEFAULT '[]',
                 created_at TEXT NOT NULL);
             INSERT INTO messages (author_kind, author_id, body, created_at)
                 VALUES ('human', 'rafael', 'antigo', '2026-11-27T18:04:05Z');
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(v1);
        let channel = open(&folder.state, "geral").unwrap();

        let read = channel.read(ReadRange::default()).unwrap();
        let captured = channel
            .post_capture(capture("t1", 0, 1, "captura"), afternoon_in_sao_paulo())
            .unwrap();

        assert_eq!(read[0].body, "antigo");
        assert!(!read[0].captured);
        assert_eq!(captured.map(|message| message.id), Some(2));
    }

    #[test]
    fn a_state_dir_without_channels_lists_none() {
        let folder = Folder::new("none");
        std::fs::remove_dir_all(folder.state.join("channels")).unwrap();

        assert_eq!(list(&folder.state), Vec::<String>::new());
    }
}
