//! Regents workspaces in the client shell: the legacy mark in the sidebar and
//! the screen when no workspace is open.

use super::*;

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

fn composed_text(snapshot: ClientShellSnapshot) -> String {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    let frame = state.compose(106, 20).expect("composed frame");
    frame_text(&frame)
}

#[test]
fn a_legacy_workspace_is_marked_in_the_sidebar() {
    let mut plain = snapshot();
    plain.workspaces[0].label = "old".into();
    let mut legacy = plain.clone();
    legacy.workspaces[0].legacy = true;

    let marked = composed_text(legacy);
    let unmarked = composed_text(plain);

    let row = marked
        .lines()
        .find(|line| line.contains("old"))
        .unwrap_or_else(|| panic!("no workspace row:\n{marked}"));
    assert!(row.contains("legacy"), "{marked}");
    assert!(!unmarked.contains("legacy"), "{unmarked}");
}

/// Composed text of a server snapshot with no workspace.
fn empty_state_text(workspace_add_command: Option<&str>) -> String {
    let mut empty = snapshot();
    empty.workspaces.clear();
    empty.tabs.clear();
    empty.panes.clear();
    empty.focused_workspace_id = None;
    empty.focused_tab_id = None;
    empty.focused_pane_id = None;
    empty.workspace_add_command = workspace_add_command.map(str::to_owned);
    let mut surface = surface();
    surface.panes.clear();
    surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 18)),
        None,
        &[],
    );
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(empty));
    state.set_pane_surface(surface);
    let frame = state.compose(106, 20).expect("composed frame");
    frame_text(&frame)
}

#[test]
fn with_no_workspace_the_pane_area_shows_the_server_command_to_add_one() {
    let text = empty_state_text(Some("regents workspace add <folder>"));

    assert!(text.contains("No workspace is open."), "{text}");
    assert!(
        text.contains("Run `regents workspace add <folder>` to open one."),
        "{text}"
    );
}

#[test]
fn a_server_without_an_add_command_shows_only_the_empty_state() {
    let text = empty_state_text(None);

    assert!(text.contains("No workspace is open."), "{text}");
    assert!(!text.contains("workspace add"), "{text}");
}
