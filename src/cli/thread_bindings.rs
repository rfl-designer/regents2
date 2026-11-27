//! `regents thread` and `regents agent list` (Regents, ADR-0004). In the
//! regents binary, `agent list` lists the agent definitions of the workspace;
//! the other `agent` commands stay herdr's commands for agent processes.

use std::path::Path;

use clap::Command;

use crate::api::schema::threads::{AgentDefinitionListProjection, ThreadListProjection};
use crate::api::schema::{Method, Request};

use super::projection::{self, Leaf, ParseError};

const THREAD_LEAVES: [Leaf; 1] = [Leaf {
    name: "list",
    about: "List the threads of the workspace",
    id: "cli:thread:list",
    arguments: ThreadListProjection::arguments,
    method: |matches| ThreadListProjection::from_matches(matches).map(Method::ThreadList),
}];

const AGENT_LEAVES: [Leaf; 1] = [Leaf {
    name: "list",
    about: "List the agents of the workspace",
    id: "cli:agent:list",
    arguments: AgentDefinitionListProjection::arguments,
    method: |matches| {
        AgentDefinitionListProjection::from_matches(matches).map(Method::AgentDefinitionList)
    },
}];

pub(crate) fn command() -> Command {
    Command::new("thread")
        .about("Show the threads of the workspace over the socket API")
        .subcommand_required(true)
        .subcommands(THREAD_LEAVES.iter().map(Leaf::command))
}

fn agent_command() -> Command {
    Command::new("agent").subcommands(AGENT_LEAVES.iter().map(Leaf::command))
}

/// True for the `agent` commands that Regents replaces.
pub(crate) fn is_regents_agent_command(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("list")
}

pub(crate) fn thread_request(args: &[String]) -> Result<Request, ParseError> {
    let cwd = std::env::current_dir().map_err(ParseError::Io)?;
    with_folder_workspace(
        projection::group_request(command(), &THREAD_LEAVES, args)?,
        &cwd,
    )
}

pub(crate) fn agent_request(args: &[String]) -> Result<Request, ParseError> {
    let cwd = std::env::current_dir().map_err(ParseError::Io)?;
    with_folder_workspace(
        projection::group_request(agent_command(), &AGENT_LEAVES, args)?,
        &cwd,
    )
}

/// Without `--workspace`, the workspace is the folder that holds `cwd`, as
/// for `channel` commands.
fn with_folder_workspace(mut request: Request, cwd: &Path) -> Result<Request, ParseError> {
    let workspace_id = match &mut request.method {
        Method::ThreadList(params) => &mut params.workspace_id,
        Method::AgentDefinitionList(params) => &mut params.workspace_id,
        _ => return Ok(request),
    };
    if workspace_id.is_none() {
        *workspace_id = super::channel_bindings::folder_workspace_id(cwd);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn thread_list_and_agent_list_build_their_requests() {
        let outside = std::env::temp_dir();
        let thread = with_folder_workspace(
            projection::group_request(command(), &THREAD_LEAVES, &owned(&["list"])).unwrap(),
            &outside,
        )
        .unwrap();
        let agent = with_folder_workspace(
            projection::group_request(
                agent_command(),
                &AGENT_LEAVES,
                &owned(&["list", "--workspace", "w1"]),
            )
            .unwrap(),
            &outside,
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(thread).unwrap()["method"],
            "thread.list"
        );
        let agent = serde_json::to_value(agent).unwrap();
        assert_eq!(agent["method"], "agent.definition.list");
        assert_eq!(agent["params"]["workspace_id"], "w1");
        assert!(is_regents_agent_command(&owned(&["list"])));
        assert!(!is_regents_agent_command(&owned(&["start", "x"])));
    }
}
