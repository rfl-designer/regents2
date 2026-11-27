//! Channel commands (Regents): `channel.post` and `channel.read`. The store
//! lives in `crate::regents::channel`; this module resolves the workspace
//! folder, maps the store to the API and emits `channel.message_posted`.

use crate::api::schema::{
    ChannelAuthorInfo, ChannelAuthorKind, ChannelMessageInfo, ChannelPostParams, ChannelReadParams,
    ErrorBody, EventData, EventEnvelope, EventKind, ResponseResult,
};
use crate::app::App;
use crate::regents::channel::{
    self, Author, AuthorKind, ChannelError, Message, NewMessage, ReadRange,
};
use crate::regents::{agents, mention};

use super::responses::{encode_error_body, encode_success};
use super::workspace_folders::FolderWorkspaceError;

/// Author ID of human messages when the server has no user name.
const UNNAMED_HUMAN: &str = "human";

impl App {
    pub(super) fn handle_channel_post(&mut self, id: String, params: ChannelPostParams) -> String {
        match self.post_channel_message(params) {
            Ok(result) => encode_success(id, result),
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_channel_read(&mut self, id: String, params: ChannelReadParams) -> String {
        match self.read_channel_messages(params) {
            Ok(result) => encode_success(id, result),
            Err(error) => encode_error_body(id, error),
        }
    }

    fn post_channel_message(
        &mut self,
        params: ChannelPostParams,
    ) -> Result<ResponseResult, ErrorBody> {
        let ChannelPostParams {
            workspace_id,
            channel,
            body,
            reply_to,
            origin_thread_id,
        } = params;
        let origin_thread_id = origin_thread_id
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        let (index, state_dir) = self.folder_state_dir(workspace_id)?;
        let channel = channel::open(&state_dir, &channel)?;
        let workspace_id = self.public_workspace_id(index);
        let agents = agents::list(&state_dir);
        let mentions = mention::resolve(&body, agents.iter().map(|agent| agent.name.as_str()));
        let posted = channel.post(
            NewMessage {
                author: Author {
                    kind: AuthorKind::Human,
                    id: local_human_id(),
                },
                body,
                reply_to,
                origin_thread_id,
                mentions,
            },
            time::OffsetDateTime::now_utc(),
        )?;
        self.open_mention_threads(index, &state_dir, &channel, &posted, &agents);
        let message = message_info(posted);
        self.emit_event(EventEnvelope {
            event: EventKind::ChannelMessagePosted,
            data: EventData::ChannelMessagePosted {
                workspace_id: workspace_id.clone(),
                message: message.clone(),
            },
        });
        Ok(ResponseResult::ChannelMessagePosted {
            workspace_id,
            message,
        })
    }

    fn read_channel_messages(
        &mut self,
        params: ChannelReadParams,
    ) -> Result<ResponseResult, ErrorBody> {
        let ChannelReadParams {
            workspace_id,
            channel,
            before,
            limit,
        } = params;
        let (index, state_dir) = self.folder_state_dir(workspace_id)?;
        let channel = channel::open(&state_dir, &channel)?;
        let workspace_id = self.public_workspace_id(index);
        let messages = channel.read(ReadRange { before, limit })?;
        Ok(ResponseResult::ChannelMessages {
            workspace_id,
            channel: channel.name().to_owned(),
            messages: messages.into_iter().map(message_info).collect(),
        })
    }

    /// The index and the state directory (`<folder>/.regents`) of a folder
    /// workspace. `workspace_id` is a public workspace ID or the stable ID of
    /// an open folder; absent means the active workspace.
    pub(super) fn folder_state_dir(
        &mut self,
        workspace_id: Option<String>,
    ) -> Result<(usize, std::path::PathBuf), ErrorBody> {
        if !self.has_folder_workspaces() {
            return Err(error_body(
                "unsupported",
                "channels, agents and threads are available only in regents",
            ));
        }
        let workspace_id = match workspace_id {
            Some(workspace_id) => self
                .public_id_for_folder_id(&workspace_id)
                .unwrap_or(workspace_id),
            None => match self.state.active {
                Some(index) => self.public_workspace_id(index),
                None => return Err(error_body("workspace_not_found", "no active workspace")),
            },
        };
        let (index, binding) = self.folder_workspace(&workspace_id)?;
        let state_dir = binding.root.join(crate::regents::folder::STATE_DIR);
        Ok((index, state_dir))
    }

    /// Public ID of the open workspace whose folder has the stable ID `id`.
    fn public_id_for_folder_id(&self, id: &str) -> Option<String> {
        let index = self.state.workspaces.iter().position(|workspace| {
            workspace
                .folder
                .as_ref()
                .is_some_and(|folder| folder.id == id)
        })?;
        Some(self.public_workspace_id(index))
    }
}

pub(super) fn error_body(code: &str, message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: code.into(),
        message: message.into(),
    }
}

impl From<ChannelError> for ErrorBody {
    fn from(err: ChannelError) -> Self {
        if let ChannelError::Storage(message) = &err {
            tracing::warn!(%message, "channel storage error");
        }
        error_body(err.code(), err.to_string())
    }
}

impl From<FolderWorkspaceError> for ErrorBody {
    fn from(err: FolderWorkspaceError) -> Self {
        error_body(err.code(), err.to_string())
    }
}

/// Identification of the human who uses this server: the login name of the
/// server user. Regents v1 has one human.
fn local_human_id() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| UNNAMED_HUMAN.to_owned())
}

fn message_info(message: Message) -> ChannelMessageInfo {
    ChannelMessageInfo {
        id: message.id,
        channel: message.channel,
        author: ChannelAuthorInfo {
            kind: match message.author.kind {
                AuthorKind::Human => ChannelAuthorKind::Human,
                AuthorKind::Agent => ChannelAuthorKind::Agent,
                AuthorKind::System => ChannelAuthorKind::System,
                AuthorKind::Unknown => ChannelAuthorKind::Unknown,
            },
            id: message.author.id,
        },
        body: message.body,
        reply_to: message.reply_to,
        origin_thread_id: message.origin_thread_id,
        mentions: message.mentions,
        created_at: message.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::api::schema::{SuccessResponse, WorkspaceAddParams};
    use serde_json::{json, Value};
    use std::path::PathBuf;

    struct Fixture {
        app: App,
        base: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir().join(format!(
                "regents-chan-{name}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&base).unwrap();
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                crate::app::AppPolicy::TEST,
                None,
                api_rx,
                crate::api::EventHub::default(),
            );
            app.state.default_shell = exiting_test_command().into();
            app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
            app.workspace_registry_path = Some(base.join("config").join("workspaces.toml"));
            Self { app, base }
        }

        /// Adds a new folder as a workspace and returns its folder and
        /// public workspace ID.
        fn workspace(&mut self, name: &str) -> (PathBuf, String) {
            let path = self.base.join(name);
            std::fs::create_dir_all(&path).unwrap();
            let path = crate::platform::canonical_path(&path).unwrap();
            let response = self.app.handle_workspace_add(
                "add".into(),
                WorkspaceAddParams {
                    path: path.display().to_string(),
                    focus: true,
                },
            );
            let added: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::WorkspaceAdded { workspace, .. } = added.result else {
                panic!("unexpected {response}");
            };
            (path, workspace.workspace_id)
        }

        fn request(&mut self, method: &str, params: Value) -> Value {
            let request = serde_json::from_value(json!({
                "id": "test", "method": method, "params": params
            }))
            .unwrap();
            serde_json::from_str(&self.app.handle_api_request(request)).unwrap()
        }

        fn posted_events(&self) -> Vec<EventData> {
            self.app
                .event_hub
                .events_after(0)
                .into_iter()
                .filter(|(_, event)| event.event == EventKind::ChannelMessagePosted)
                .map(|(_, event)| event.data)
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown_test_runtimes(&mut self.app);
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[tokio::test]
    async fn a_posted_message_reads_back_and_is_announced_as_an_event() {
        let mut fixture = Fixture::new("post");
        let (_, workspace_id) = fixture.workspace("projeto");

        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "geral", "body": "oi"}),
        );
        let read = fixture.request(
            "channel.read",
            json!({"workspace_id": workspace_id, "channel": "geral"}),
        );

        let message = &posted["result"]["message"];
        assert_eq!(
            posted["result"]["type"], "channel_message_posted",
            "{posted}"
        );
        assert_eq!(message["body"], "oi");
        assert_eq!(message["channel"], "geral");
        assert_eq!(message["author"]["kind"], "human");
        assert_eq!(message["author"]["id"], local_human_id());
        assert!(message["created_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(message.get("reply_to"), None);
        assert_eq!(message.get("origin_thread_id"), None);
        assert_eq!(read["result"]["messages"], json!([message]));
        let message: ChannelMessageInfo = serde_json::from_value(message.clone()).unwrap();
        assert_eq!(
            fixture.posted_events(),
            vec![EventData::ChannelMessagePosted {
                workspace_id,
                message
            }]
        );
    }

    #[tokio::test]
    async fn without_a_workspace_id_the_active_workspace_is_used() {
        let mut fixture = Fixture::new("active");
        fixture.workspace("primeiro");
        let (folder, workspace_id) = fixture.workspace("segundo");

        let posted = fixture.request("channel.post", json!({"channel": "geral", "body": "oi"}));

        assert_eq!(
            posted["result"]["workspace_id"],
            json!(workspace_id),
            "{posted}"
        );
        assert!(folder
            .join(".regents/channels/geral/channel.sqlite")
            .is_file());
    }

    #[tokio::test]
    async fn the_stable_folder_id_names_the_workspace() {
        let mut fixture = Fixture::new("stable-id");
        let (folder, workspace_id) = fixture.workspace("primeiro");
        fixture.workspace("segundo");
        let stable_id = crate::regents::folder::read_manifest(&folder).unwrap().id;

        let posted = fixture.request(
            "channel.post",
            json!({"workspace_id": stable_id, "channel": "geral", "body": "oi"}),
        );

        assert_eq!(
            posted["result"]["workspace_id"],
            json!(workspace_id),
            "{posted}"
        );
        assert!(folder
            .join(".regents/channels/geral/channel.sqlite")
            .is_file());
    }

    #[tokio::test]
    async fn posting_to_a_missing_channel_is_a_clear_error() {
        let mut fixture = Fixture::new("missing");
        let (folder, workspace_id) = fixture.workspace("projeto");

        let response = fixture.request(
            "channel.post",
            json!({"workspace_id": workspace_id, "channel": "outro", "body": "oi"}),
        );

        assert_eq!(response["error"]["code"], "channel_not_found", "{response}");
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("#outro"), "{message}");
        assert!(!folder.join(".regents/channels/outro").exists());
        assert_eq!(fixture.posted_events(), vec![]);
    }

    #[tokio::test]
    async fn a_disconnected_workspace_refuses_channel_commands() {
        let mut fixture = Fixture::new("disconnected");
        let (folder, workspace_id) = fixture.workspace("projeto");
        std::fs::rename(&folder, fixture.base.join("movido")).unwrap();

        for method in ["channel.post", "channel.read"] {
            let response = fixture.request(
                method,
                json!({"workspace_id": workspace_id, "channel": "geral", "body": "oi"}),
            );
            assert_eq!(
                response["error"]["code"], "workspace_disconnected",
                "{method}: {response}"
            );
        }
    }

    #[tokio::test]
    async fn herdr_has_no_channels() {
        let mut fixture = Fixture::new("herdr");
        fixture.app.workspace_registry_path = None;

        let response = fixture.request("channel.post", json!({"channel": "geral", "body": "oi"}));

        assert_eq!(response["error"]["code"], "unsupported", "{response}");
    }
}
