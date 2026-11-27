//! Chat panes in the layout (Regents, ADR-0001). A chat pane is a normal
//! pane that runs the chat view of one channel (`regents::chat`). The server
//! keeps the pane-to-channel binding in `AppState.channel_views`, like the
//! plugin pane records, so herdr's terminal state stays unchanged. The session
//! file saves the binding in the optional `PaneSnapshot.channel_view`; the
//! restore runs the chat view again, and the pane command line carries the
//! binding back into the registry.

use std::collections::HashMap;

use crate::layout::PaneId;
use crate::persist::SessionSnapshot;
use crate::regents::channel::ChannelView;
use crate::workspace::Workspace;

/// `channel view`: the command that a chat pane runs.
pub(crate) const VIEW_COMMAND: &str = "view";
const CHANNEL_GROUP: &str = "channel";
const WORKSPACE_OPTION: &str = "--workspace";

/// Command line of the process that a chat pane runs: the running binary's
/// `channel view` command for the channel of `view`.
pub(crate) fn view_argv(view: &ChannelView) -> std::io::Result<Vec<String>> {
    // Unit tests run in the test binary, which must not start itself again.
    #[cfg(test)]
    let program = std::path::PathBuf::from(if cfg!(windows) {
        "C:\\Windows\\System32\\whoami.exe"
    } else {
        "/usr/bin/true"
    });
    #[cfg(not(test))]
    let program = crate::platform::launch_executable()?;
    Ok(vec![
        program.display().to_string(),
        CHANNEL_GROUP.into(),
        VIEW_COMMAND.into(),
        view.channel.clone(),
        WORKSPACE_OPTION.into(),
        view.folder_id.clone(),
    ])
}

/// The channel of a command line that `view_argv` built, whatever binary path
/// it names.
pub(crate) fn view_from_argv(argv: &[String]) -> Option<ChannelView> {
    match argv {
        [_, group, command, channel, option, folder_id]
            if group == CHANNEL_GROUP && command == VIEW_COMMAND && option == WORKSPACE_OPTION =>
        {
            Some(ChannelView {
                folder_id: folder_id.clone(),
                channel: channel.clone(),
            })
        }
        _ => None,
    }
}

/// Writes the chat pane bindings into a captured session. `snapshot` must come
/// from `persist::capture` of the same `workspaces`, so its workspaces and tabs
/// are in the same order and its panes use the raw pane IDs.
pub(crate) fn record_in_snapshot(
    snapshot: &mut SessionSnapshot,
    workspaces: &[Workspace],
    channel_views: &HashMap<PaneId, ChannelView>,
) {
    if channel_views.is_empty() {
        return;
    }
    for (workspace, saved) in workspaces.iter().zip(&mut snapshot.workspaces) {
        for (tab, saved) in workspace.tabs.iter().zip(&mut saved.tabs) {
            for pane_id in tab.panes.keys() {
                if let (Some(view), Some(pane)) = (
                    channel_views.get(pane_id),
                    saved.panes.get_mut(&pane_id.raw()),
                ) {
                    pane.channel_view = Some(view.clone());
                }
            }
        }
    }
}

/// Chat panes of restored workspaces: the panes whose command line is a chat
/// view command.
pub(crate) fn restored_views(
    workspaces: &[Workspace],
    terminals: &HashMap<crate::terminal::TerminalId, crate::terminal::TerminalState>,
) -> HashMap<PaneId, ChannelView> {
    workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.panes)
        .filter_map(|(pane_id, pane)| {
            let argv = terminals
                .get(&pane.attached_terminal_id)?
                .launch_argv
                .as_deref()?;
            Some((*pane_id, view_from_argv(argv)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geral() -> ChannelView {
        ChannelView {
            folder_id: "f1".into(),
            channel: "geral".into(),
        }
    }

    #[test]
    fn the_view_command_line_names_its_channel_again() {
        let argv = view_argv(&geral()).unwrap();

        assert_eq!(
            &argv[1..],
            ["channel", "view", "geral", "--workspace", "f1"]
        );
        assert_eq!(view_from_argv(&argv), Some(geral()));
    }

    #[test]
    fn other_command_lines_are_not_chat_views() {
        let argv = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();

        assert_eq!(view_from_argv(&argv(&["just", "dev"])), None);
        assert_eq!(
            view_from_argv(&argv(&[
                "r",
                "channel",
                "read",
                "geral",
                "--workspace",
                "f1"
            ])),
            None
        );
    }
}
