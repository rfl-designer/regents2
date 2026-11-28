//! `regents thread` and `regents agent list` (Regents, ADR-0004). `thread
//! output` is no API command: it shows the recorded output of an ended
//! thread, and the popup of an ended thread runs it. In the
//! regents binary, `agent list` lists the agent definitions of the workspace;
//! the other `agent` commands stay herdr's commands for agent processes.

use std::path::Path;

use clap::Command;

use crate::api::schema::threads::{
    AgentDefinitionListProjection, ThreadListProjection, ThreadOpenProjection,
};
use crate::api::schema::{Method, Request};

use super::projection::{self, Input, Leaf, ParseError};

/// `thread output`: the command that the popup of an ended thread runs.
pub(crate) const OUTPUT_COMMAND: &str = "output";

const THREAD_LEAVES: [Leaf; 2] = [
    Leaf {
        name: "list",
        about: "List the threads of the workspace",
        id: "cli:thread:list",
        arguments: ThreadListProjection::arguments,
        method: |matches| ThreadListProjection::from_matches(matches).map(Method::ThreadList),
    },
    Leaf {
        name: "open",
        about: "Open a thread as a popup over the layout",
        id: "cli:thread:open",
        arguments: ThreadOpenProjection::arguments,
        method: |matches| ThreadOpenProjection::from_matches(matches).map(Method::ThreadOpen),
    },
];

pub(crate) fn thread_id() -> Input<String> {
    Input::positional("thread_id", 0, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name("THREAD_ID")
}

fn output_command() -> Command {
    Command::new(OUTPUT_COMMAND)
        .about("Show the recorded output of an ended thread of the workspace folder")
        .arg(
            clap::Arg::new("thread_id")
                .value_name("THREAD_ID")
                .required(true),
        )
}

/// `thread output <id>`: prints the output that the server recorded when the
/// thread ended. On a terminal it then waits for `q`, Esc or Ctrl-C and
/// ignores every other key, so the popup stays read only.
pub(crate) fn run_output(args: &[String]) -> std::io::Result<i32> {
    let matches = match output_command()
        .bin_name(format!(
            "{} thread {OUTPUT_COMMAND}",
            crate::build_info::product_name()
        ))
        .no_binary_name(true)
        .try_get_matches_from(args)
    {
        Ok(matches) => matches,
        Err(error) => return super::print_clap_error(&error),
    };
    let thread_id = matches
        .get_one::<String>("thread_id")
        .cloned()
        .unwrap_or_default();
    let cwd = std::env::current_dir()?;
    let Some(text) = crate::regents::threads::read_output_from(&cwd, &thread_id) else {
        eprintln!("no recorded output for thread {thread_id}");
        return Ok(1);
    };
    show_output(&text)?;
    Ok(0)
}

fn show_output(text: &str) -> std::io::Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::io::{IsTerminal, Write};

    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return stdout.write_all(text.as_bytes());
    }
    crossterm::terminal::enable_raw_mode()?;
    // Raw mode does not turn a line feed into a new line.
    let shown = text.replace('\n', "\r\n");
    let result = write!(stdout, "{shown}\x1b[0m\r\n[q: close]")
        .and_then(|()| stdout.flush())
        .and_then(|()| loop {
            if let Event::Key(key) = crossterm::event::read()? {
                let quit = key.kind != KeyEventKind::Release
                    && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)));
                if quit {
                    break Ok(());
                }
            }
        });
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

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
        .subcommand(output_command())
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
        Method::ThreadOpen(params) => &mut params.workspace_id,
        Method::AgentDefinitionList(params) => &mut params.workspace_id,
        _ => return Ok(request),
    };
    if workspace_id.is_none() {
        *workspace_id = crate::regents::folder::folder_workspace_id(cwd);
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
