//! Regents channels in the client shell: a workspace expands in the
//! "spaces" section and shows its channels as child rows, and a click on a
//! channel opens it as a chat pane (ticket #7).

use super::*;
use crate::api::schema::Method;
use crossterm::event::{MouseButton, MouseEventKind};

fn frame_text(frame: &crate::client::frame_output::ComposedFrame) -> String {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn state_with_channels(channels: &[&str]) -> ClientShellState {
    let mut projected = snapshot();
    projected.workspaces[0].label = "projeto".into();
    projected.workspaces[0].channels = channels.iter().map(|name| (*name).to_owned()).collect();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

fn compose_text(state: &mut ClientShellState) -> String {
    let frame = state.compose(106, 24).expect("composed frame");
    frame_text(&frame)
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })])
}

/// The sidebar part of each composed line: the text before the divider.
fn sidebar_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.chars().take_while(|ch| *ch != '│').collect::<String>())
        .collect()
}

fn click_channel_toggle(state: &mut ClientShellState) -> ClientShellInput {
    let toggle = state
        .hits
        .channel_toggles
        .first()
        .map(|(rect, _)| *rect)
        .expect("a channel toggle");
    click(state, toggle)
}

fn endpoint_methods(outcome: &ClientShellInput) -> Vec<&Method> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(&request.method),
            _ => None,
        })
        .collect()
}

#[test]
fn a_workspace_with_channels_starts_collapsed_and_expands_to_show_them() {
    let mut state = state_with_channels(&["design", "geral"]);

    let collapsed = compose_text(&mut state);
    assert!(!collapsed.contains("#geral"), "{collapsed}");
    let row = collapsed
        .lines()
        .find(|line| line.contains("projeto"))
        .unwrap_or_else(|| panic!("no workspace row:\n{collapsed}"));
    assert!(row.contains('▸'), "{collapsed}");

    click_channel_toggle(&mut state);
    let expanded = compose_text(&mut state);

    let lines = sidebar_lines(&expanded);
    let workspace = lines
        .iter()
        .position(|line| line.contains("projeto"))
        .unwrap_or_else(|| panic!("no workspace row:\n{expanded}"));
    assert!(lines[workspace].contains('▾'), "{expanded}");
    let design = lines
        .iter()
        .position(|line| line.contains("#design"))
        .unwrap_or_else(|| panic!("no #design row:\n{expanded}"));
    let geral = lines
        .iter()
        .position(|line| line.contains("#geral"))
        .unwrap_or_else(|| panic!("no #geral row:\n{expanded}"));
    assert!(workspace < design && design + 1 == geral, "{expanded}");

    click_channel_toggle(&mut state);
    let collapsed_again = compose_text(&mut state);
    assert!(!collapsed_again.contains("#geral"), "{collapsed_again}");
}

#[test]
fn a_workspace_without_channels_has_no_toggle() {
    let mut state = state_with_channels(&[]);

    let text = compose_text(&mut state);

    assert!(state.hits.channel_toggles.is_empty());
    let row = text
        .lines()
        .find(|line| line.contains("projeto"))
        .unwrap_or_else(|| panic!("no workspace row:\n{text}"));
    assert!(!row.contains('▸'), "{text}");
}

#[test]
fn a_click_on_a_channel_asks_the_server_to_open_it() {
    let mut state = state_with_channels(&["geral"]);
    compose_text(&mut state);
    click_channel_toggle(&mut state);
    compose_text(&mut state);
    let hit = state
        .hits
        .channels
        .iter()
        .find(|hit| hit.workspace_index == 0 && hit.channel_index == 0)
        .copied()
        .expect("a #geral hit");

    let outcome = click(&mut state, hit.rect);

    assert_eq!(
        endpoint_methods(&outcome),
        [&Method::ChannelOpen(
            crate::api::schema::ChannelOpenParams {
                workspace_id: Some("ws_1".into()),
                channel: "geral".into(),
            }
        )]
    );
}

#[test]
fn a_click_on_the_toggle_sends_no_request() {
    let mut state = state_with_channels(&["geral"]);
    compose_text(&mut state);

    let outcome = click_channel_toggle(&mut state);

    assert!(endpoint_methods(&outcome).is_empty());
}

#[test]
fn the_parent_of_a_worktree_group_expands_its_channels_too() {
    let mut projected = snapshot();
    projected.workspaces[0].label = "projeto".into();
    projected.workspaces[0].channels = vec!["geral".into()];
    projected.workspaces[0].worktree = Some(ClientShellWorktree {
        key: "repo".into(),
        label: "repo".into(),
        is_linked_worktree: false,
    });
    let mut child = projected.workspaces[0].clone();
    child.workspace_id = "ws_2".into();
    child.label = "projeto-feature".into();
    child.focused = false;
    child.channels = Vec::new();
    child.worktree = Some(ClientShellWorktree {
        key: "repo".into(),
        label: "repo".into(),
        is_linked_worktree: true,
    });
    projected.workspaces.push(child);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    compose_text(&mut state);
    let parent = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_1")
        .expect("the parent row");
    assert!(parent.group_toggle.is_some(), "the worktree toggle stays");

    click_channel_toggle(&mut state);
    let expanded = compose_text(&mut state);

    assert!(
        sidebar_lines(&expanded)
            .iter()
            .any(|line| line.contains("#geral")),
        "{expanded}"
    );
}

#[test]
fn an_unread_channel_row_shows_a_dot() {
    let mut state = state_with_channels(&["deploy", "geral"]);
    if let Some(snapshot) = state.snapshot.as_deref() {
        let mut projected = snapshot.clone();
        projected.workspaces[0].unread_channels = vec!["deploy".into()];
        state.set_snapshot(Box::new(projected));
    }
    compose_text(&mut state);
    click_channel_toggle(&mut state);

    let lines = sidebar_lines(&compose_text(&mut state));

    let row = |name: &str| {
        lines
            .iter()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("no {name} row: {lines:?}"))
            .clone()
    };
    assert!(row("#deploy").contains("●#deploy"), "{lines:?}");
    assert!(!row("#geral").contains('●'), "{lines:?}");
}
