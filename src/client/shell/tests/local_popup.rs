use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crossterm::event::KeyModifiers;

const LOCAL_POPUP_CONFIG: &str = r#"
[[keys.command]]
key = "ctrl+alt+u"
command = "devtools switch"
type = "popup"
run_on = "local"
"#;

fn remote_profile() -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    }
}

fn local_popup_command() -> crate::protocol::ClientShellCommand {
    crate::protocol::ClientShellCommand {
        command_id: "cmd_local_switch".into(),
        binding_label: "ctrl+alt+u".into(),
        binding_labels: vec!["ctrl+alt+u".into()],
        action: crate::protocol::ClientShellCommandAction::Popup,
        description: None,
    }
}

fn local_state() -> ClientShellState {
    let config: Config = toml::from_str(LOCAL_POPUP_CONFIG).unwrap();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_endpoint_catalog(&[remote_profile()]);
    let mut local = snapshot();
    local.commands.push(local_popup_command());
    state.set_snapshot(Box::new(local));
    state.set_endpoint_methods(Some(vec![
        "command.invoke".into(),
        POPUP_OVERLAY_OPEN_METHOD.into(),
    ]));
    state.set_pane_surface(surface());
    state
}

fn state_viewing_remote() -> (ClientShellState, ClientEndpointId) {
    let mut state = local_state();
    let remote = ClientEndpointId::Ssh(remote_profile().id);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    let mut projection = snapshot();
    projection.boot_id = "remote-boot".into();
    projection.panes[0].foreground_cwd = Some("/srv/remote".into());
    state.set_endpoint_snapshot(&remote, Box::new(projection));
    assert!(state.activate_endpoint_projection(&remote));
    let mut remote_surface = surface();
    remote_surface.boot_id = "remote-boot".into();
    state.set_pane_surface(remote_surface);
    state.compose(100, 28).unwrap();
    (state, remote)
}

fn press(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code, modifiers,
    ))])
}

fn press_local_popup_key(state: &mut ClientShellState) -> ClientShellInput {
    press(
        state,
        KeyCode::Char('u'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )
}

fn overlay_surface(popup: Option<&str>) -> PaneSurfaceFrame {
    let mut surface = surface_with_popup();
    surface.frame = FrameData {
        cells: Vec::new(),
        width: 0,
        height: 0,
        cursor: None,
        hyperlinks: Vec::new(),
        graphics: Vec::new(),
    };
    surface.panes.clear();
    match popup {
        Some(text) => {
            let popup = surface.popup.as_mut().unwrap();
            popup.terminal_id = "terminal-local".into();
            popup.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                &Buffer::with_lines([text, "", ""]),
                None,
                &[],
            );
        }
        None => surface.popup = None,
    }
    surface
}

fn open_overlay(state: &mut ClientShellState) -> String {
    let outcome = press_local_popup_key(state);
    let [ClientShellAction::LocalPopupOverlay(request)] = outcome.actions.as_slice() else {
        panic!(
            "expected a local overlay request, got {:?}",
            outcome.actions
        );
    };
    let request_id = request.request.id.clone();
    assert!(!state.handle_local_popup_overlay_result(
        &request_id,
        Ok(crate::api::schema::ResponseResult::Ok {})
    ));
    assert!(state.receive_local_popup_overlay_surface(1, overlay_surface(Some("LOCALPOP"))));
    request_id
}

#[test]
fn local_popup_key_requests_an_overlay_with_the_presented_context() {
    let (mut state, remote) = state_viewing_remote();
    assert!(state
        .config
        .keybinds
        .keybinds
        .custom_commands
        .iter()
        .any(|command| command.run_on == crate::config::CommandRunOn::Local));

    let outcome = press_local_popup_key(&mut state);

    let [ClientShellAction::LocalPopupOverlay(request)] = outcome.actions.as_slice() else {
        panic!(
            "expected a local overlay request, got {:?}",
            outcome.actions
        );
    };
    assert_eq!(request.boot_id, "boot-1");
    let crate::api::schema::Method::ClientShellPopupOverlayOpen(params) = &request.request.method
    else {
        panic!("expected popup overlay open");
    };
    assert_eq!(params.command_id, "cmd_local_switch");
    let ClientEndpointId::Ssh(profile_id) = &remote else {
        unreachable!();
    };
    assert_eq!(
        params.view,
        crate::api::schema::ClientShellPopupOverlayView {
            endpoint_id: profile_id.to_string(),
            endpoint_label: "Build".into(),
            workspace_id: Some("ws_1".into()),
            tab_id: Some("tab_1".into()),
            pane_id: Some("pane_1".into()),
            pane_cwd: Some("/srv/remote".into()),
        }
    );
    // The presented machine keeps its surface and endpoint; only Local is asked.
    assert_eq!(state.active_endpoint_id, remote);
    assert!(state.popup_pending);
    assert!(outcome.requests.is_empty());
}

#[test]
fn local_popup_overlay_draws_over_the_remote_view_and_takes_input() {
    let (mut state, remote) = state_viewing_remote();
    open_overlay(&mut state);

    let frame = state.compose(100, 28).unwrap();
    let rows = frame_rows(&frame.frame).join("\n");
    assert!(rows.contains("LOCALPOP"), "{rows}");
    assert!(rows.contains("LIVE"), "{rows}");
    assert_eq!(state.active_endpoint_id, remote);
    assert!(!state.popup_pending);

    let input = press(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(matches!(
        input.requests.as_slice(),
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == "terminal-local"
    ));

    // Remote surface updates neither hide the overlay nor steal its input.
    let mut remote_surface = surface();
    remote_surface.boot_id = "remote-boot".into();
    remote_surface.surface_revision = 2;
    state.set_pane_surface(remote_surface);
    let rows = frame_rows(&state.compose(100, 28).unwrap().frame).join("\n");
    assert!(rows.contains("LOCALPOP"), "{rows}");
    let input = press(&mut state, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(matches!(
        input.requests.as_slice(),
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == "terminal-local"
    ));
}

#[test]
fn closed_local_popup_returns_input_to_the_remote_view() {
    let (mut state, _) = state_viewing_remote();
    open_overlay(&mut state);

    assert!(state.receive_local_popup_overlay_surface(1, overlay_surface(None)));

    assert!(!state.local_popup_overlay_active());
    let rows = frame_rows(&state.compose(100, 28).unwrap().frame).join("\n");
    assert!(!rows.contains("LOCALPOP"), "{rows}");
    let input = press(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(matches!(
        input.requests.as_slice(),
        [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "pane_1"
    ));
}

#[test]
fn local_disconnect_clears_the_overlay() {
    let (mut state, _) = state_viewing_remote();
    open_overlay(&mut state);

    state.mark_endpoint_disconnected(&ClientEndpointId::Local);

    assert!(!state.local_popup_overlay_active());
    assert!(!state.accepts_local_popup_overlay_surface(1));
    let input = press(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(matches!(
        input.requests.as_slice(),
        [ClientMessage::ClientShellPaneInput { .. }]
    ));
}

#[test]
fn rejected_or_cancelled_overlay_requests_release_pane_input() {
    let (mut state, _) = state_viewing_remote();
    let outcome = press_local_popup_key(&mut state);
    let [ClientShellAction::LocalPopupOverlay(request)] = outcome.actions.as_slice() else {
        panic!("expected a local overlay request");
    };

    assert!(state.cancel_endpoint_request(&request.request.id));

    assert!(!state.local_popup_overlay_active());
    assert!(!state.popup_pending);
    assert!(state.visible_endpoint_notice.is_some());
}

#[test]
fn closing_for_a_machine_switch_asks_local_to_end_the_popup() {
    let (mut state, _) = state_viewing_remote();
    open_overlay(&mut state);

    let close = state.close_local_popup_overlay().expect("close request");

    assert!(matches!(
        close.request.method,
        crate::api::schema::Method::ClientShellPopupOverlayClose(_)
    ));
    assert_eq!(close.boot_id, "boot-1");
    assert!(!state.local_popup_overlay_active());
    assert!(state.close_local_popup_overlay().is_none());
}

#[test]
fn unavailable_local_server_shows_the_unavailable_notice() {
    let (mut state, _) = state_viewing_remote();
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);

    let outcome = press_local_popup_key(&mut state);

    assert!(outcome.actions.is_empty());
    assert!(!state.local_popup_overlay_active());
    let notice = state.visible_endpoint_notice.as_ref().expect("notice");
    assert_eq!(notice.title, "Endpoint unavailable");
}

#[test]
fn local_server_without_overlay_support_disables_only_this_action() {
    let (mut state, _) = state_viewing_remote();
    state.set_endpoint_methods_for(
        &ClientEndpointId::Local,
        Some(vec!["command.invoke".into()]),
    );

    let outcome = press_local_popup_key(&mut state);

    assert!(outcome.actions.is_empty());
    assert!(!state.local_popup_overlay_active());
    let notice = state.visible_endpoint_notice.as_ref().expect("notice");
    assert_eq!(notice.title, "Action unavailable");
}

#[test]
fn presenting_local_runs_the_ordinary_popup_command() {
    let mut state = local_state();

    let outcome = press_local_popup_key(&mut state);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::Endpoint { endpoint_id, request, .. }]
            if endpoint_id.is_local()
                && matches!(&request.method, crate::api::schema::Method::CommandInvoke(params)
                    if params.command_id == "cmd_local_switch")
    ));
    assert!(!state.local_popup_overlay_active());
}

#[test]
fn focus_requests_close_the_local_popup_before_navigating() {
    let (mut state, remote) = state_viewing_remote();
    open_overlay(&mut state);

    let mut outcome = ClientShellInput::default();
    state.focus_or_activate(
        remote,
        ClientEndpointFocusTarget::Workspace("ws_1".into()),
        &mut outcome,
    );

    assert!(!state.local_popup_overlay_active());
    assert!(matches!(
        outcome.actions.first(),
        Some(ClientShellAction::LocalPopupOverlay(close))
            if matches!(close.request.method, crate::api::schema::Method::ClientShellPopupOverlayClose(_))
    ));
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.method, crate::api::schema::Method::WorkspaceFocus(_))
    )));
}
