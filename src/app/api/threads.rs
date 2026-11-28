//! Thread and agent definition commands (Regents): `thread.list`,
//! `thread.open` and `agent.definition.list`.

use crate::api::schema::{
    AgentApproval, AgentCliKind, AgentDefinitionInfo, AgentDefinitionListParams, AgentIsolation,
    ErrorBody, ResponseResult, ThreadInfo, ThreadListParams, ThreadOpenParams, ThreadOriginKind,
    ThreadState,
};
use crate::app::threads::ThreadPopupError;
use crate::app::App;
use crate::regents::agents::{self, AgentCli, AgentDefinition, Approval, Isolation};
use crate::regents::threads::{self, ThreadRecord};

use super::channels::error_body;
use super::responses::{encode_error_body, encode_success};

impl App {
    pub(super) fn handle_thread_list(&mut self, id: String, params: ThreadListParams) -> String {
        match self.list_threads(params) {
            Ok(result) => encode_success(id, result),
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_agent_definition_list(
        &mut self,
        id: String,
        params: AgentDefinitionListParams,
    ) -> String {
        match self.folder_state_dir(params.workspace_id) {
            Ok((index, state_dir)) => encode_success(
                id,
                ResponseResult::AgentDefinitionList {
                    workspace_id: self.public_workspace_id(index),
                    agents: agents::list(&state_dir)
                        .iter()
                        .map(agent_definition_info)
                        .collect(),
                },
            ),
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_thread_open(&mut self, id: String, params: ThreadOpenParams) -> String {
        let opened = self
            .folder_state_dir(params.workspace_id)
            .and_then(|(_, state_dir)| {
                self.open_thread_popup(&state_dir, &params.thread_id)
                    .map_err(|err| {
                        let (code, message) = match err {
                            ThreadPopupError::PopupOpen => {
                                ("popup_already_open", "another popup is open")
                            }
                            ThreadPopupError::NotFound => {
                                ("thread_not_found", "the workspace has no such thread")
                            }
                            ThreadPopupError::NoOutput => (
                                "thread_output_not_found",
                                "the thread is not running and recorded no output",
                            ),
                            ThreadPopupError::CannotStart => {
                                ("popup_failed", "cannot open the thread popup")
                            }
                        };
                        error_body(code, message.to_owned())
                    })
            });
        match opened {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(error) => encode_error_body(id, error),
        }
    }

    /// The thread index of the workspace at `index` for the client snapshot,
    /// newest first, from the cached registry.
    pub(crate) fn workspace_thread_infos(&self, index: usize) -> Vec<ThreadInfo> {
        let Some(binding) = self.state.workspaces[index].folder.as_ref() else {
            return Vec::new();
        };
        let state_dir = binding.root.join(crate::regents::folder::STATE_DIR);
        self.workspace_thread_records(index)
            .iter()
            .map(|record| self.live_thread_info(&state_dir, record.clone()))
            .collect()
    }

    fn live_thread_info(&self, state_dir: &std::path::Path, record: ThreadRecord) -> ThreadInfo {
        let pane_id = self.running_thread_pane_id(state_dir, &record.id);
        // A hidden pane is never seen, so its idle state is not "done".
        let agent_status = self
            .running_thread_agent_state(state_dir, &record.id)
            .map(|state| crate::app::api_helpers::pane_agent_status(state, true));
        ThreadInfo {
            agent_status,
            ..thread_info(record, pane_id)
        }
    }

    fn list_threads(&mut self, params: ThreadListParams) -> Result<ResponseResult, ErrorBody> {
        let (index, state_dir) = self.folder_state_dir(params.workspace_id)?;
        let records = threads::registry(&state_dir).list().map_err(|err| {
            tracing::warn!(%err, "thread registry error");
            error_body(err.code(), err.to_string())
        })?;
        let threads = records
            .into_iter()
            .map(|record| self.live_thread_info(&state_dir, record))
            .collect();
        Ok(ResponseResult::ThreadList {
            workspace_id: self.public_workspace_id(index),
            threads,
        })
    }
}

fn thread_info(record: ThreadRecord, pane_id: Option<String>) -> ThreadInfo {
    ThreadInfo {
        id: record.id,
        agent: record.agent,
        cli: record.cli.map_or(AgentCliKind::Unknown, cli_kind),
        origin: match record.origin {
            threads::ThreadOrigin::Channel => ThreadOriginKind::Channel,
            threads::ThreadOrigin::Direct => ThreadOriginKind::Direct,
            threads::ThreadOrigin::Unknown => ThreadOriginKind::Unknown,
        },
        channel: record.channel,
        origin_message_id: record.origin_message_id,
        opener_thread_id: record.opener_thread_id,
        depth: record.depth,
        isolation: record.isolation.map(isolation_kind),
        end_reason: record.end_reason,
        state: match record.state {
            threads::ThreadState::Queued => ThreadState::Queued,
            threads::ThreadState::Active => ThreadState::Active,
            threads::ThreadState::Done => ThreadState::Done,
            threads::ThreadState::Interrupted => ThreadState::Interrupted,
            threads::ThreadState::Failed => ThreadState::Failed,
            threads::ThreadState::Unknown => ThreadState::Unknown,
        },
        agent_status: None,
        session_id: record.session_id,
        pane_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

fn agent_definition_info(agent: &AgentDefinition) -> AgentDefinitionInfo {
    AgentDefinitionInfo {
        name: agent.name.clone(),
        cli: cli_kind(agent.cli),
        model: agent.model.clone(),
        isolation: isolation_kind(agent.isolation),
        approval: match agent.approval {
            Approval::Default => AgentApproval::Default,
            Approval::Auto => AgentApproval::Auto,
        },
        reserved: agent.reserved,
    }
}

fn cli_kind(cli: AgentCli) -> AgentCliKind {
    match cli {
        AgentCli::Claude => AgentCliKind::Claude,
        AgentCli::Codex => AgentCliKind::Codex,
        AgentCli::Agy => AgentCliKind::Agy,
    }
}

fn isolation_kind(isolation: Isolation) -> AgentIsolation {
    match isolation {
        Isolation::Shared => AgentIsolation::Shared,
        Isolation::Worktree => AgentIsolation::Worktree,
    }
}
