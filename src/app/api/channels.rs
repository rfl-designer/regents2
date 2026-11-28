//! Channel commands (Regents): `channel.create`, `channel.rename`,
//! `channel.post`, `channel.read` and `channel.open`. The store lives in
//! `crate::regents::channel`; this module resolves the workspace folder, maps
//! the store to the API, emits `channel.message_posted` and `channel.renamed`,
//! keeps the unread channels and opens chat panes
//! (`crate::regents::chat_panes`).

use crate::api::schema::{
    ChannelAuthorInfo, ChannelAuthorKind, ChannelCreateParams, ChannelMessageInfo,
    ChannelOpenParams, ChannelPostParams, ChannelReadParams, ChannelRenameParams, ErrorBody,
    EventData, EventEnvelope, EventKind, ResponseResult,
};
use crate::app::App;
use crate::layout::PaneId;
use crate::regents::channel::{
    self, Author, AuthorKind, ChannelError, ChannelView, Message, NewMessage, ReadRange,
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

    pub(super) fn handle_channel_open(&mut self, id: String, params: ChannelOpenParams) -> String {
        match self.open_channel_view(params) {
            Ok(result) => encode_success(id, result),
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_channel_create(
        &mut self,
        id: String,
        params: ChannelCreateParams,
    ) -> String {
        match self.create_channel(params) {
            Ok(result) => encode_success(id, result),
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_channel_rename(
        &mut self,
        id: String,
        params: ChannelRenameParams,
    ) -> String {
        match self.rename_channel(params) {
            Ok(result) => encode_success(id, result),
            Err(error) => encode_error_body(id, error),
        }
    }

    fn create_channel(&mut self, params: ChannelCreateParams) -> Result<ResponseResult, ErrorBody> {
        let (index, state_dir) = self.folder_state_dir(params.workspace_id)?;
        let channel = channel::create(&state_dir, &params.channel)?;
        self.refresh_folder_channels();
        Ok(ResponseResult::ChannelCreated {
            workspace_id: self.public_workspace_id(index),
            channel: channel.name().to_owned(),
        })
    }

    /// Renames a channel folder. The thread registry, the open chat panes,
    /// their saved session binding and the unread mark follow the new name,
    /// and `channel.renamed` tells the running chat views.
    fn rename_channel(&mut self, params: ChannelRenameParams) -> Result<ResponseResult, ErrorBody> {
        let (index, state_dir) = self.folder_state_dir(params.workspace_id)?;
        let folder_id = self.folder_id(index)?;
        let renamed = channel::rename(&state_dir, &params.channel, &params.new_name)?;
        let (from, to) = (params.channel, renamed.name().to_owned());
        if let Err(err) = crate::regents::threads::registry(&state_dir).rename_channel(&from, &to) {
            // The channel folder moved already; the index keeps the old name.
            tracing::warn!(%err, from, to, "cannot move the threads of a renamed channel");
        }
        let old_label = format!("#{from}");
        let argv = crate::regents::chat_panes::view_argv(&ChannelView {
            folder_id: folder_id.clone(),
            channel: to.clone(),
        })
        .ok();
        let chat_panes: Vec<PaneId> = self
            .state
            .channel_views
            .iter_mut()
            .filter(|(_, view)| view.folder_id == folder_id && view.channel == from)
            .map(|(pane_id, view)| {
                view.channel.clone_from(&to);
                *pane_id
            })
            .collect();
        for pane_id in chat_panes {
            let Some(terminal) = self
                .find_pane(pane_id)
                .map(|(_, pane)| pane.attached_terminal_id.clone())
                .and_then(|terminal_id| self.state.terminals.get_mut(&terminal_id))
            else {
                continue;
            };
            // A live handoff rebuilds the chat pane registry from this argv.
            terminal.launch_argv.clone_from(&argv);
            // A label that the user changed stays.
            if terminal.manual_label.as_deref() == Some(old_label.as_str()) {
                terminal.set_manual_label(format!("#{to}"));
            }
            self.state.mark_session_dirty();
        }
        if let Some(unread) = self.unread_channels.get_mut(&folder_id) {
            if unread.remove(&from) {
                unread.insert(to.clone());
            }
        }
        self.refresh_folder_channels();
        let workspace_id = self.public_workspace_id(index);
        self.emit_event(EventEnvelope {
            event: EventKind::ChannelRenamed,
            data: EventData::ChannelRenamed {
                workspace_id: workspace_id.clone(),
                from: from.clone(),
                channel: to.clone(),
            },
        });
        Ok(ResponseResult::ChannelRenamed {
            workspace_id,
            from,
            channel: to,
        })
    }

    fn folder_id(&self, index: usize) -> Result<String, ErrorBody> {
        self.state.workspaces[index]
            .folder
            .as_ref()
            .map(|binding| binding.id.clone())
            .ok_or_else(|| error_body("workspace_legacy", "the workspace has no folder"))
    }

    /// Gives focus to the chat pane of a channel. When no workspace has a
    /// chat pane for the channel, it opens one next to the focused pane of
    /// the channel's workspace.
    fn open_channel_view(
        &mut self,
        params: ChannelOpenParams,
    ) -> Result<ResponseResult, ErrorBody> {
        let (index, state_dir) = self.folder_state_dir(params.workspace_id)?;
        let channel = channel::open(&state_dir, &params.channel)?;
        let binding = self.state.workspaces[index]
            .folder
            .as_ref()
            .ok_or_else(|| error_body("workspace_legacy", "the workspace has no folder"))?;
        let view = ChannelView {
            folder_id: binding.id.clone(),
            channel: channel.name().to_owned(),
        };
        let root = binding.root.clone();
        let view_key = view.clone();
        let (pane_index, pane_id, opened) = match self.channel_view_pane(&view) {
            Some((pane_index, pane_id)) => (pane_index, pane_id, false),
            None => (index, self.spawn_channel_view(index, view, root)?, true),
        };
        self.state.focus_pane_in_workspace(pane_index, pane_id);
        self.state.mark_active_tab_seen();
        self.mark_channel_read(&view_key);
        self.state.mode = crate::app::Mode::Terminal;
        let pane = self
            .pane_info(pane_index, pane_id)
            .ok_or_else(|| error_body("channel_open_failed", "the chat pane disappeared"))?;
        Ok(ResponseResult::ChannelOpened {
            workspace_id: pane.workspace_id.clone(),
            channel: channel.name().to_owned(),
            opened,
            pane,
        })
    }

    /// The chat pane that shows `view` and the index of its workspace. A
    /// moved chat pane can be in any workspace.
    fn channel_view_pane(&self, view: &ChannelView) -> Option<(usize, PaneId)> {
        let pane_id = self
            .state
            .channel_views
            .iter()
            .find_map(|(pane_id, shown)| (shown == view).then_some(*pane_id))?;
        let index = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.find_tab_index_for_pane(pane_id).is_some())?;
        Some((index, pane_id))
    }

    /// Opens a chat pane for `view` to the right of the focused pane of
    /// workspace `index`. The chat view is not an agent, so the pane has no
    /// agent detection.
    fn spawn_channel_view(
        &mut self,
        index: usize,
        view: ChannelView,
        root: std::path::PathBuf,
    ) -> Result<PaneId, ErrorBody> {
        let open_failed = |err: std::io::Error| error_body("channel_open_failed", err.to_string());
        let argv = crate::regents::chat_panes::view_argv(&view).map_err(open_failed)?;
        let target = self
            .state
            .workspaces
            .get(index)
            .and_then(|workspace| workspace.focused_pane_id())
            .ok_or_else(|| error_body("pane_not_found", "the workspace has no pane"))?;
        let direction = ratatui::layout::Direction::Horizontal;
        let (rows, cols) = self
            .state
            .new_pane_size(crate::ui::NewPanePlacement::Split {
                ws_idx: index,
                target,
                direction,
                ratio: 0.5,
            });
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.pane_host_terminal_appearance();
        let workspace = &mut self.state.workspaces[index];
        let (tab_index, mut new_pane) = workspace
            .split_pane_argv_command(
                target,
                direction,
                rows,
                cols,
                Some(root),
                &argv,
                Vec::new(),
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                true,
                crate::pane::AgentDetection::Disabled,
            )
            .ok_or_else(|| error_body("pane_not_found", "the focused pane disappeared"))?
            .map_err(open_failed)?;
        new_pane
            .terminal
            .set_manual_label(format!("#{}", view.channel));
        let pane_id = new_pane.pane_id;
        self.state.channel_views.insert(pane_id, view);
        self.adopt_split_pane(index, tab_index, new_pane);
        Ok(pane_id)
    }

    /// Marks a channel that just got a message as unread, unless its chat
    /// pane is the focused pane of the active workspace.
    fn mark_channel_unread(&mut self, view: ChannelView) {
        let in_focus = self
            .state
            .active
            .and_then(|index| self.state.workspaces.get(index))
            .and_then(|workspace| workspace.focused_pane_id())
            .is_some_and(|pane_id| self.state.channel_views.get(&pane_id) == Some(&view));
        if !in_focus
            && self
                .unread_channels
                .entry(view.folder_id)
                .or_default()
                .insert(view.channel)
        {
            self.render_dirty.request_generic();
        }
    }

    fn mark_channel_read(&mut self, view: &ChannelView) {
        if let Some(unread) = self.unread_channels.get_mut(&view.folder_id) {
            if unread.remove(&view.channel) {
                self.render_dirty.request_generic();
            }
        }
    }

    /// Unread channels of the workspace at `index`, in name order. It reads
    /// no file.
    pub(crate) fn workspace_unread_channels(&self, index: usize) -> Vec<String> {
        let Some(unread) = self
            .state
            .workspaces
            .get(index)
            .and_then(|workspace| workspace.folder.as_ref())
            .and_then(|binding| self.unread_channels.get(&binding.id))
        else {
            return Vec::new();
        };
        self.workspace_channel_names(index)
            .iter()
            .filter(|name| unread.contains(*name))
            .cloned()
            .collect()
    }

    /// Registers the chat panes of a restored session: the panes whose
    /// command line is a chat view command.
    pub(crate) fn register_restored_channel_views(&mut self) {
        self.state.channel_views = crate::regents::chat_panes::restored_views(
            &self.state.workspaces,
            &self.state.terminals,
        );
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
            thread_token,
        } = params;
        let origin_thread_id = origin_thread_id
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        let thread_token = thread_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty());
        let (index, state_dir) = self.folder_state_dir(workspace_id)?;
        let author = self.post_author(&state_dir, origin_thread_id.as_deref(), thread_token)?;
        let channel = channel::open(&state_dir, &channel)?;
        let agents = agents::list(&state_dir);
        let mentions = mention::resolve(&body, agents.iter().map(|agent| agent.name.as_str()));
        let unknown = mention::unknown(&body, agents.iter().map(|agent| agent.name.as_str()));
        let posted = channel.post(
            NewMessage {
                author,
                body,
                reply_to,
                origin_thread_id,
                mentions,
            },
            time::OffsetDateTime::now_utc(),
        )?;
        // The reply comes first, then the events that it causes.
        let (workspace_id, message) = self.announce_channel_message(index, posted.clone());
        let replied_agent =
            self.resume_replied_thread(index, &state_dir, &channel, &posted, &agents);
        self.open_mention_threads(
            index,
            &state_dir,
            &channel,
            &posted,
            &agents,
            replied_agent.as_deref(),
        );
        let channel_name = posted.channel.clone();
        if !unknown.is_empty() {
            let names: Vec<String> = unknown.iter().map(|name| format!("@{name}")).collect();
            let verb = if names.len() == 1 {
                "não é um agente"
            } else {
                "não são agentes"
            };
            self.post_system_message(
                Some(index),
                &state_dir,
                &channel_name,
                None,
                format!("{} {verb} deste workspace", names.join(", ")),
            );
        }
        Ok(ResponseResult::ChannelMessagePosted {
            workspace_id,
            message,
        })
    }

    /// Marks the channel of `posted`, a new message of the workspace at
    /// `index`, unread and announces it as an event.
    pub(crate) fn announce_channel_message(
        &mut self,
        index: usize,
        posted: Message,
    ) -> (String, ChannelMessageInfo) {
        let workspace_id = self.public_workspace_id(index);
        if let Ok(folder_id) = self.folder_id(index) {
            self.mark_channel_unread(ChannelView {
                folder_id,
                channel: posted.channel.clone(),
            });
        }
        let message = message_info(posted);
        self.emit_event(EventEnvelope {
            event: EventKind::ChannelMessagePosted,
            data: EventData::ChannelMessagePosted {
                workspace_id: workspace_id.clone(),
                message: message.clone(),
            },
        });
        (workspace_id, message)
    }

    /// The author of a post: the local human without an origin thread, the
    /// thread agent with an origin thread and its credential. Any other
    /// combination is refused, so no client can post as an agent or a thread.
    fn post_author(
        &self,
        state_dir: &std::path::Path,
        origin_thread_id: Option<&str>,
        thread_token: Option<&str>,
    ) -> Result<Author, ErrorBody> {
        match (origin_thread_id, thread_token) {
            (None, None) => Ok(Author {
                kind: AuthorKind::Human,
                id: local_human_id(),
            }),
            (Some(thread_id), Some(token)) => self
                .thread_agent(state_dir, thread_id, token)
                .map(|agent| Author {
                    kind: AuthorKind::Agent,
                    id: agent.to_owned(),
                })
                .ok_or_else(|| {
                    error_body(
                        "invalid_thread_credential",
                        format!(
                            "the thread credential is not valid for thread {thread_id} of this \
                             workspace, or the thread is no longer running"
                        ),
                    )
                }),
            (Some(thread_id), None) => Err(error_body(
                "invalid_thread_credential",
                format!(
                    "a post from thread {thread_id} needs its credential in {}",
                    crate::regents::THREAD_TOKEN_ENV_VAR
                ),
            )),
            (None, Some(_)) => Err(error_body(
                "invalid_thread_credential",
                format!(
                    "a thread credential needs its thread ID in {}",
                    crate::regents::THREAD_ID_ENV_VAR
                ),
            )),
        }
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

    /// Lists the channels and the threads of each connected open folder
    /// workspace again. It reads one directory and one database per folder
    /// workspace: call it with the folder
    /// checks (load, list, bind and the periodic check), never from render.
    pub(crate) fn refresh_folder_channels(&mut self) {
        let channels: std::collections::HashMap<String, Vec<String>> = self
            .state
            .workspaces
            .iter()
            .filter_map(|workspace| workspace.folder.as_ref())
            .filter(|binding| !self.disconnected_folders.contains(&binding.id))
            .map(|binding| {
                let state_dir = binding.root.join(crate::regents::folder::STATE_DIR);
                (binding.id.clone(), channel::list(&state_dir))
            })
            .collect();
        if channels != self.folder_channels {
            self.folder_channels = channels;
            self.render_dirty.request_generic();
        }
        self.refresh_folder_threads();
    }

    /// Channel names of the workspace at `index`, as of the last refresh. It
    /// reads no file, so snapshot and render code can call it.
    pub(crate) fn workspace_channel_names(&self, index: usize) -> &[String] {
        self.state
            .workspaces
            .get(index)
            .and_then(|workspace| workspace.folder.as_ref())
            .and_then(|binding| self.folder_channels.get(&binding.id))
            .map_or(&[], Vec::as_slice)
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
        captured: message.captured,
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
    async fn a_post_with_an_incomplete_or_unknown_thread_credential_is_refused() {
        let mut fixture = Fixture::new("credential");
        let (_, workspace_id) = fixture.workspace("projeto");

        for extra in [
            json!({"origin_thread_id": "t1"}),
            json!({"thread_token": "abc"}),
            json!({"origin_thread_id": "t1", "thread_token": "abc"}),
        ] {
            let mut params =
                json!({"workspace_id": workspace_id, "channel": "geral", "body": "oi"});
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let refused = fixture.request("channel.post", params);
            assert_eq!(
                refused["error"]["code"], "invalid_thread_credential",
                "{extra}: {refused}"
            );
        }
        let read = fixture.request(
            "channel.read",
            json!({"workspace_id": workspace_id, "channel": "geral"}),
        );
        assert_eq!(read["result"]["messages"], json!([]), "{read}");
        assert!(fixture.posted_events().is_empty());
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

    #[tokio::test]
    async fn a_folder_workspace_shows_its_channels() {
        let mut fixture = Fixture::new("channels");
        let (folder, workspace_id) = fixture.workspace("projeto");
        let channels = |fixture: &mut Fixture| {
            fixture.request("workspace.get", json!({"workspace_id": workspace_id}))["result"]
                ["workspace"]["channels"]
                .clone()
        };

        assert_eq!(channels(&mut fixture), json!(["geral"]));

        std::fs::create_dir_all(folder.join(".regents/channels/design")).unwrap();
        fixture.app.refresh_folder_connections();

        assert_eq!(channels(&mut fixture), json!(["design", "geral"]));
    }

    #[tokio::test]
    async fn a_herdr_workspace_shows_no_channels_field() {
        let mut fixture = Fixture::new("herdr-channels");
        fixture.app.workspace_registry_path = None;
        let created = fixture.request("workspace.create", json!({}));
        let workspace = &created["result"]["workspace"];

        assert!(workspace.get("channels").is_none(), "{created}");
    }

    fn pane_count(fixture: &mut Fixture, workspace_id: &str) -> u64 {
        fixture.request("workspace.get", json!({"workspace_id": workspace_id}))["result"]
            ["workspace"]["pane_count"]
            .as_u64()
            .unwrap()
    }

    #[tokio::test]
    async fn opening_a_channel_adds_a_chat_pane_and_a_second_open_only_focuses_it() {
        let mut fixture = Fixture::new("open");
        let (folder, workspace_id) = fixture.workspace("projeto");
        let folder_id = crate::regents::folder::read_manifest(&folder).unwrap().id;

        let first = fixture.request(
            "channel.open",
            json!({"workspace_id": workspace_id, "channel": "geral"}),
        );

        let result = &first["result"];
        assert_eq!(result["type"], "channel_opened", "{first}");
        assert_eq!(result["opened"], true);
        assert_eq!(result["channel"], "geral");
        assert_eq!(result["pane"]["focused"], true);
        assert_eq!(
            result["pane"]["channel"],
            json!({"folder_id": folder_id, "channel": "geral"})
        );
        let chat_pane = result["pane"]["pane_id"].as_str().unwrap().to_owned();
        assert_eq!(pane_count(&mut fixture, &workspace_id), 2);

        let other_pane = fixture.request("pane.list", json!({"workspace_id": workspace_id}))
            ["result"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| pane["pane_id"].as_str().unwrap().to_owned())
            .find(|pane_id| *pane_id != chat_pane)
            .unwrap();
        fixture.request("pane.focus", json!({"pane_id": other_pane}));

        let second = fixture.request("channel.open", json!({"channel": "geral"}));

        assert_eq!(second["result"]["opened"], false, "{second}");
        assert_eq!(second["result"]["pane"]["pane_id"], json!(chat_pane));
        assert_eq!(second["result"]["pane"]["focused"], true);
        assert_eq!(pane_count(&mut fixture, &workspace_id), 2);
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn opening_a_missing_channel_adds_no_pane() {
        let mut fixture = Fixture::new("open-missing");
        let (_, workspace_id) = fixture.workspace("projeto");

        let response = fixture.request(
            "channel.open",
            json!({"workspace_id": workspace_id, "channel": "outro"}),
        );

        assert_eq!(response["error"]["code"], "channel_not_found", "{response}");
        assert_eq!(pane_count(&mut fixture, &workspace_id), 1);
    }

    #[tokio::test]
    async fn a_terminal_pane_has_no_channel_field() {
        let mut fixture = Fixture::new("plain-pane");
        let (_, workspace_id) = fixture.workspace("projeto");

        let panes = fixture.request("pane.list", json!({"workspace_id": workspace_id}));

        assert!(
            panes["result"]["panes"][0].get("channel").is_none(),
            "{panes}"
        );
    }

    #[tokio::test]
    async fn a_chat_pane_moved_to_another_workspace_is_focused_not_duplicated() {
        let mut fixture = Fixture::new("open-moved");
        let (_, projeto) = fixture.workspace("projeto");
        let (_, outro) = fixture.workspace("outro");
        let opened = fixture.request(
            "channel.open",
            json!({"workspace_id": projeto, "channel": "geral"}),
        );
        let chat_pane = opened["result"]["pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let moved = fixture.request(
            "pane.move",
            json!({
                "pane_id": chat_pane,
                "destination": {"type": "new_tab", "workspace_id": outro},
            }),
        );
        assert!(moved.get("error").is_none(), "{moved}");
        let moved_pane = moved["result"]["move_result"]["pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let again = fixture.request(
            "channel.open",
            json!({"workspace_id": projeto, "channel": "geral"}),
        );

        assert_eq!(again["result"]["opened"], false, "{again}");
        assert_eq!(again["result"]["pane"]["pane_id"], json!(moved_pane));
        assert_eq!(again["result"]["workspace_id"], json!(outro));
        assert_eq!(pane_count(&mut fixture, &projeto), 1);
        fixture.app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn the_session_saves_the_channel_of_a_chat_pane() {
        let mut fixture = Fixture::new("save");
        let (folder, workspace_id) = fixture.workspace("projeto");
        let folder_id = crate::regents::folder::read_manifest(&folder).unwrap().id;
        fixture.request(
            "channel.open",
            json!({"workspace_id": workspace_id, "channel": "geral"}),
        );
        let mut snapshot = crate::persist::capture(
            &fixture.app.state.workspaces,
            &fixture.app.state.terminals,
            &fixture.app.terminal_runtimes,
            fixture.app.state.active,
            fixture.app.state.selected,
        );

        crate::regents::chat_panes::record_in_snapshot(
            &mut snapshot,
            &fixture.app.state.workspaces,
            &fixture.app.state.channel_views,
        );

        let saved: Vec<_> = snapshot.workspaces[0].tabs[0]
            .panes
            .values()
            .filter_map(|pane| pane.channel_view.clone())
            .collect();
        assert_eq!(
            saved,
            [ChannelView {
                folder_id,
                channel: "geral".into()
            }]
        );
    }

    fn workspace_field(fixture: &mut Fixture, workspace_id: &str, field: &str) -> Value {
        fixture.request("workspace.get", json!({"workspace_id": workspace_id}))["result"]
            ["workspace"][field]
            .clone()
    }

    #[tokio::test]
    async fn a_created_channel_gets_a_folder_and_shows_in_the_workspace() {
        let mut fixture = Fixture::new("create");
        let (folder, workspace_id) = fixture.workspace("projeto");

        let created = fixture.request(
            "channel.create",
            json!({"workspace_id": workspace_id, "channel": "deploy"}),
        );

        assert_eq!(created["result"]["type"], "channel_created", "{created}");
        assert_eq!(created["result"]["channel"], "deploy");
        assert!(folder.join(".regents/channels/deploy").is_dir());
        assert_eq!(
            workspace_field(&mut fixture, &workspace_id, "channels"),
            json!(["deploy", "geral"])
        );
        let again = fixture.request("channel.create", json!({"channel": "deploy"}));
        assert_eq!(again["error"]["code"], "channel_exists", "{again}");
    }

    #[tokio::test]
    async fn a_channel_name_that_is_not_a_slug_is_refused() {
        let mut fixture = Fixture::new("create-slug");
        let (folder, workspace_id) = fixture.workspace("projeto");

        let created = fixture.request("channel.create", json!({"channel": "Deploy Final"}));
        let renamed = fixture.request(
            "channel.rename",
            json!({"channel": "geral", "new_name": "Geral"}),
        );

        assert_eq!(
            created["error"]["code"], "invalid_channel_name",
            "{created}"
        );
        assert_eq!(
            renamed["error"]["code"], "invalid_channel_name",
            "{renamed}"
        );
        assert_eq!(
            workspace_field(&mut fixture, &workspace_id, "channels"),
            json!(["geral"])
        );
        assert!(folder.join(".regents/channels/geral").is_dir());
    }

    #[tokio::test]
    async fn a_rename_keeps_the_history_and_moves_the_open_chat_pane() {
        let mut fixture = Fixture::new("rename");
        let (folder, workspace_id) = fixture.workspace("projeto");
        let folder_id = crate::regents::folder::read_manifest(&folder).unwrap().id;
        fixture.request("channel.create", json!({"channel": "deploy"}));
        fixture.request(
            "channel.post",
            json!({"channel": "deploy", "body": "antes"}),
        );
        let opened = fixture.request("channel.open", json!({"channel": "deploy"}));
        let chat_pane = opened["result"]["pane"]["pane_id"].clone();

        let renamed = fixture.request(
            "channel.rename",
            json!({"workspace_id": workspace_id, "channel": "deploy", "new_name": "release"}),
        );

        assert_eq!(renamed["result"]["type"], "channel_renamed", "{renamed}");
        assert_eq!(renamed["result"]["from"], "deploy");
        assert_eq!(renamed["result"]["channel"], "release");
        assert!(!folder.join(".regents/channels/deploy").exists());
        let read = fixture.request("channel.read", json!({"channel": "release"}));
        assert_eq!(read["result"]["messages"][0]["body"], "antes", "{read}");
        assert_eq!(
            workspace_field(&mut fixture, &workspace_id, "channels"),
            json!(["geral", "release"])
        );
        let pane = fixture.request("pane.get", json!({"pane_id": chat_pane}));
        assert_eq!(
            pane["result"]["pane"]["channel"],
            json!({"folder_id": folder_id, "channel": "release"}),
            "{pane}"
        );
        assert_eq!(pane["result"]["pane"]["label"], "#release", "{pane}");
        // A live handoff registers the chat panes again from their argv.
        fixture.app.register_restored_channel_views();
        let view = fixture.app.state.channel_views.values().next().cloned();
        assert_eq!(view.map(|view| view.channel).as_deref(), Some("release"));
        let events: Vec<EventData> = fixture
            .app
            .event_hub
            .events_after(0)
            .into_iter()
            .filter(|(_, event)| event.event == EventKind::ChannelRenamed)
            .map(|(_, event)| event.data)
            .collect();
        assert_eq!(
            events,
            [EventData::ChannelRenamed {
                workspace_id,
                from: "deploy".into(),
                channel: "release".into(),
            }]
        );
        let again = fixture.request("channel.open", json!({"channel": "release"}));
        assert_eq!(again["result"]["opened"], false, "{again}");
    }

    #[tokio::test]
    async fn a_message_in_a_channel_out_of_focus_is_unread_until_the_channel_opens() {
        let mut fixture = Fixture::new("unread");
        let (_, workspace_id) = fixture.workspace("projeto");
        fixture.request("channel.create", json!({"channel": "deploy"}));
        let unread =
            |fixture: &mut Fixture| workspace_field(fixture, &workspace_id, "unread_channels");

        fixture.request("channel.post", json!({"channel": "deploy", "body": "oi"}));
        assert_eq!(unread(&mut fixture), json!(["deploy"]));

        fixture.request("channel.open", json!({"channel": "deploy"}));
        assert_eq!(unread(&mut fixture), Value::Null);

        // The chat pane of #deploy has focus: its new messages are seen.
        fixture.request(
            "channel.post",
            json!({"channel": "deploy", "body": "visto"}),
        );
        assert_eq!(unread(&mut fixture), Value::Null);

        fixture.request("channel.post", json!({"channel": "geral", "body": "oi"}));
        fixture.request(
            "channel.rename",
            json!({"channel": "geral", "new_name": "anuncios"}),
        );
        assert_eq!(unread(&mut fixture), json!(["anuncios"]));
    }

    #[tokio::test]
    async fn closing_a_chat_pane_drops_its_channel_record() {
        let mut fixture = Fixture::new("close-chat");
        let (_, workspace_id) = fixture.workspace("projeto");
        let opened = fixture.request(
            "channel.open",
            json!({"workspace_id": workspace_id, "channel": "geral"}),
        );
        let chat_pane = opened["result"]["pane"]["pane_id"].clone();

        let closed = fixture.request("pane.close", json!({"pane_id": chat_pane}));

        assert!(closed.get("error").is_none(), "{closed}");
        assert!(fixture.app.state.channel_views.is_empty());
        let reopened = fixture.request("channel.open", json!({"channel": "geral"}));
        assert_eq!(reopened["result"]["opened"], true, "{reopened}");
    }
}
