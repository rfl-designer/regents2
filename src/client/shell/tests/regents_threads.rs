//! Regents threads in the client shell: the agents section lists the threads
//! of the focused workspace grouped by channel, a click opens a thread as a
//! popup, and a click outside a thread popup closes it (ticket #11).

use super::*;
use crate::api::schema::{AgentCliKind, AgentStatus, Method, ThreadInfo, ThreadState};
use crossterm::event::{MouseButton, MouseEventKind};

fn thread(id: &str, agent: &str, channel: &str, state: ThreadState) -> ThreadInfo {
    ThreadInfo {
        id: id.into(),
        agent: agent.into(),
        cli: AgentCliKind::Claude,
        origin: Default::default(),
        channel: Some(channel.into()),
        origin_message_id: Some(1),
        opener_thread_id: None,
        depth: 0,
        isolation: None,
        state,
        agent_status: None,
        end_reason: None,
        session_id: None,
        pane_id: None,
        created_at: "2026-11-27T18:04:05Z".into(),
        updated_at: "2026-11-27T18:04:05Z".into(),
    }
}

fn state_with_threads(threads: Vec<ThreadInfo>, surface: PaneSurfaceFrame) -> ClientShellState {
    let mut projected = snapshot();
    projected.workspaces[0].threads = threads;
    projected.thread_popup = surface.popup.is_some();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface);
    state
}

fn sidebar_lines(state: &mut ClientShellState) -> Vec<String> {
    let frame = state.compose(106, 30).expect("composed frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .chars()
                .take_while(|ch| *ch != '│')
                .collect::<String>()
        })
        .collect()
}

/// A press and a release at the same cell.
fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    let event = |kind| {
        crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };
    state.handle_raw_events(vec![
        event(MouseEventKind::Down(MouseButton::Left)),
        event(MouseEventKind::Up(MouseButton::Left)),
    ])
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
fn threads_list_under_their_channel_with_their_live_state() {
    let working = ThreadInfo {
        agent_status: Some(AgentStatus::Working),
        ..thread("t3", "reviewer", "geral", ThreadState::Active)
    };
    let mut state = state_with_threads(
        vec![
            working,
            thread("t2", "chief", "deploy", ThreadState::Active),
            thread("t1", "chief", "geral", ThreadState::Done),
        ],
        surface(),
    );

    let lines = sidebar_lines(&mut state);

    let row = |text: &str| {
        lines
            .iter()
            .position(|line| line.contains(text))
            .unwrap_or_else(|| panic!("no {text} row: {lines:#?}"))
    };
    let geral = row("#geral");
    let deploy = row("#deploy");
    assert!(lines[row("@reviewer t3")].contains("working"), "{lines:#?}");
    assert!(lines[row("@chief t1")].contains("done"), "{lines:#?}");
    assert!(lines[row("@chief t2")].contains("idle"), "{lines:#?}");
    assert_eq!(
        [row("@reviewer t3"), row("@chief t1")],
        [geral + 1, geral + 2],
        "{lines:#?}"
    );
    assert_eq!(row("@chief t2"), deploy + 1, "{lines:#?}");
    assert!(geral < deploy, "the newest thread's channel is first");
}

#[test]
fn a_click_on_a_thread_asks_the_server_to_open_it() {
    let mut state = state_with_threads(
        vec![thread("t1", "chief", "geral", ThreadState::Done)],
        surface(),
    );
    sidebar_lines(&mut state);
    let (rect, _, _) = state.hits.threads.first().cloned().expect("a thread hit");

    let outcome = click(&mut state, rect.x, rect.y);

    assert_eq!(
        endpoint_methods(&outcome),
        [&Method::ThreadOpen(crate::api::schema::ThreadOpenParams {
            workspace_id: Some("ws_1".into()),
            thread_id: "t1".into(),
        })]
    );
}

#[test]
fn a_click_outside_a_thread_popup_closes_it_and_inside_does_not() {
    let mut state = state_with_threads(
        vec![thread("t1", "chief", "geral", ThreadState::Active)],
        surface_with_popup(),
    );
    sidebar_lines(&mut state);
    let popup = state.hits.popup.clone().expect("a popup hit");

    let inside = click(&mut state, popup.inner_rect.x, popup.inner_rect.y);
    let outside = click(&mut state, 0, 0);

    assert!(!endpoint_methods(&inside)
        .iter()
        .any(|method| matches!(method, Method::PopupClose(_))));
    assert!(matches!(
        endpoint_methods(&outside).as_slice(),
        [Method::PopupClose(_)]
    ));
}

#[test]
fn a_click_outside_another_popup_keeps_it() {
    let mut state = state_with_threads(Vec::new(), surface_with_popup());
    if let Some(snapshot) = state.snapshot.as_deref() {
        let mut projected = snapshot.clone();
        projected.thread_popup = false;
        state.set_snapshot(Box::new(projected));
    }
    sidebar_lines(&mut state);

    let outside = click(&mut state, 0, 0);

    assert!(endpoint_methods(&outside).is_empty());
}

#[test]
fn a_blocked_thread_stands_out_in_the_index_until_it_works_again() {
    let agent_color = |status| {
        let mut state = state_with_threads(
            vec![ThreadInfo {
                agent_status: Some(status),
                ..thread("t1", "chief", "geral", ThreadState::Active)
            }],
            surface(),
        );
        let frame = state.compose(106, 30).expect("composed frame");
        let width = frame.width as usize;
        let lines = sidebar_lines(&mut state);
        let y = lines
            .iter()
            .position(|line| line.contains("@chief t1"))
            .unwrap_or_else(|| panic!("{lines:#?}"));
        let x = lines[y].chars().position(|ch| ch == '@').unwrap();
        frame.cells[y * width + x].fg
    };

    let blocked = agent_color(AgentStatus::Blocked);
    let working = agent_color(AgentStatus::Working);
    let idle = agent_color(AgentStatus::Idle);

    assert_ne!(blocked, working);
    assert_eq!(working, idle, "only a block changes the agent color");
}
