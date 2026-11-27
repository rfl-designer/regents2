//! Channel messages (Regents, ADR-0003). Each channel is a subfolder of
//! `.regents/channels/`, and its messages live in a SQLite database in that
//! subfolder. The database appears on the first message. The server is the
//! only process that writes it; agents and clients post through the API.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

const CHANNELS_DIR: &str = "channels";
const DATABASE_FILE: &str = "channel.sqlite";
const SCHEMA_VERSION: i64 = 1;
/// Time that an access waits for a lock that another connection holds. The
/// server is the only writer, so a lock is rare and short. The channel I/O runs
/// on the server loop: keep this small until it moves to a worker, as the
/// folder check of #5 must too.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

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
        if message.body.trim().is_empty() {
            return Err(ChannelError::EmptyBody);
        }
        let reply_to = message.reply_to.map(to_sql_id).transpose()?;
        let created_at = now
            .to_offset(time::UtcOffset::UTC)
            .replace_nanosecond(0)
            .map_err(|err| ChannelError::Storage(err.to_string()))?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|err| ChannelError::Storage(err.to_string()))?;
        let mentions = serde_json::to_string(&message.mentions)
            .map_err(|err| ChannelError::Storage(err.to_string()))?;
        let path = self.database_path();
        let existed = path.exists();
        if let (Some(id), false) = (message.reply_to, existed) {
            // Without a database the channel has no message to reply to.
            return Err(self.reply_not_found(id));
        }
        let inserted = self.insert(&path, &message, reply_to, &mentions, &created_at);
        if inserted.is_err() && !existed {
            remove_database(&path);
        }
        Ok(Message {
            id: inserted?,
            channel: self.name.clone(),
            author: message.author,
            body: message.body,
            reply_to: message.reply_to,
            origin_thread_id: message.origin_thread_id,
            mentions: message.mentions,
            created_at,
        })
    }

    fn insert(
        &self,
        path: &Path,
        message: &NewMessage,
        reply_to: Option<i64>,
        mentions: &str,
        created_at: &str,
    ) -> Result<u64, ChannelError> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        migrate(&connection)?;
        let transaction = connection.transaction()?;
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
                 (author_kind, author_id, body, reply_to, origin_thread_id, mentions, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                message.author.kind.as_str(),
                message.author.id,
                message.body,
                reply_to,
                message.origin_thread_id,
                mentions,
                created_at,
            ],
        )?;
        let id = from_sql_id(transaction.last_insert_rowid())?;
        transaction.commit()?;
        Ok(id)
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
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        // The most recent `limit` messages before `before`, oldest first.
        let mut statement = connection.prepare(
            "SELECT id, author_kind, author_id, body, reply_to, origin_thread_id, mentions,
                    created_at
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
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (id, kind, author_id, body, reply_to, origin_thread_id, mentions, created_at) =
                row?;
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
            });
        }
        Ok(messages)
    }
}

fn migrate(connection: &Connection) -> Result<(), ChannelError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS messages (
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
         PRAGMA user_version = 1;
         COMMIT;",
    )?;
    Ok(())
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

    struct Folder(PathBuf);

    impl Folder {
        /// A workspace state directory with the channel `geral`.
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let state = std::env::temp_dir()
                .join(format!(
                    "regents-channel-{name}-{}-{stamp}",
                    std::process::id()
                ))
                .join(".regents");
            std::fs::create_dir_all(state.join("channels").join("geral")).unwrap();
            Self(state)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            if let Some(parent) = self.0.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
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
        let channel = open(&folder.0, "geral").unwrap();

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
        };
        assert_eq!(posted, expected);
        assert_eq!(channel.read(ReadRange::default()).unwrap(), vec![expected]);
    }

    #[test]
    fn the_database_appears_only_with_the_first_message() {
        let folder = Folder::new("lazy");
        let channel = open(&folder.0, "geral").unwrap();
        let database = folder.0.join("channels/geral/channel.sqlite");

        assert_eq!(channel.read(ReadRange::default()).unwrap(), vec![]);
        assert!(!database.exists());

        channel.post(human("oi"), afternoon_in_sao_paulo()).unwrap();

        assert!(database.is_file());
    }

    #[test]
    fn messages_survive_a_new_open_in_posting_order() {
        let folder = Folder::new("reopen");
        let now = afternoon_in_sao_paulo();
        let first = open(&folder.0, "geral").unwrap();
        first.post(human("um"), now).unwrap();
        first.post(human("dois"), now).unwrap();

        let bodies: Vec<String> = open(&folder.0, "geral")
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
        let channel = open(&folder.0, "geral").unwrap();
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
        let channel = open(&folder.0, "geral").unwrap();
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
        let channel = open(&folder.0, "geral").unwrap();

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
        assert!(!folder.0.join("channels/geral/channel.sqlite").exists());
    }

    #[test]
    fn a_message_id_beyond_the_storage_range_is_invalid_input() {
        let folder = Folder::new("overflow");
        let channel = open(&folder.0, "geral").unwrap();
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
        let channel = open(&folder.0, "geral").unwrap();

        let err = channel
            .post(human("  \n"), afternoon_in_sao_paulo())
            .unwrap_err();

        assert_eq!(err.code(), "invalid_message");
        assert!(!folder.0.join("channels/geral/channel.sqlite").exists());
    }

    #[test]
    fn a_missing_channel_is_not_found_and_creates_nothing() {
        let folder = Folder::new("missing");

        let err = open(&folder.0, "outro").unwrap_err();

        assert_eq!(err.code(), "channel_not_found");
        assert!(err.to_string().contains("#outro"), "{err}");
        assert!(!folder.0.join("channels/outro").exists());
    }

    #[test]
    fn a_name_that_is_not_a_slug_is_refused() {
        let folder = Folder::new("slug");

        for name in ["", "Geral", "a b", "../geral", "a--b", "-a", "a-", "ã"] {
            let err = open(&folder.0, name).unwrap_err();
            assert_eq!(err.code(), "invalid_channel_name", "{name:?}");
        }
        assert!(is_slug("code-review2"));
    }
}
