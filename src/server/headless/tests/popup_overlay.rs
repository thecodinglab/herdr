use super::*;

const OVERLAY_CLIENT: u64 = 52;
const LOCAL_VIEWER: u64 = 7;

fn local_popup_binding(command: &str) -> crate::config::CustomCommandKeybind {
    crate::config::CustomCommandKeybind {
        bindings: crate::config::ActionKeybinds::prefix("u"),
        label: "prefix+u".into(),
        command: command.into(),
        action: crate::config::CustomCommandAction::Popup,
        description: None,
        width: None,
        height: None,
        run_on: crate::config::CommandRunOn::Local,
    }
}

fn install_local_popup(server: &mut HeadlessServer, binding: crate::config::CustomCommandKeybind) {
    server.app.install_test_custom_commands(&[binding]);
}

fn local_popup_command_id(server: &HeadlessServer) -> String {
    server.app.client_shell_command_manifest()[0]
        .command_id
        .clone()
}

/// A client shell connected to this server while it presents another machine.
fn connect_presenting_elsewhere(
    server: &mut HeadlessServer,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        surface_reuse: false,
        surface_delta: false,
        surface_scroll: false,
        client_id: OVERLAY_CLIENT,
        surface_cols: 100,
        surface_rows: 30,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        direct_graphics: false,
        endpoint_keybindings: false,
        mouse_capture: false,
        surface_active: false,
        writer,
    });
    while control.try_recv().is_ok() {}
    (control, render)
}

fn view() -> api::schema::ClientShellPopupOverlayView {
    api::schema::ClientShellPopupOverlayView {
        endpoint_id: "0123456789abcdef0123456789abcdef".into(),
        endpoint_label: "macmini".into(),
        workspace_id: Some("w9".into()),
        tab_id: Some("w9:t1".into()),
        pane_id: Some("w9:p1".into()),
        pane_cwd: Some("/remote/path".into()),
    }
}

fn overlay_request(
    server: &mut HeadlessServer,
    client_id: u64,
    method: api::schema::Method,
) -> (bool, bool) {
    let boot_id = server.client_shell_boot_id.clone();
    let changed = server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
        client_id,
        boot_id,
        request: Box::new(api::schema::Request {
            id: "overlay".into(),
            method,
        }),
    });
    (changed, true)
}

fn open(server: &mut HeadlessServer, client_id: u64) -> bool {
    let command_id = local_popup_command_id(server);
    overlay_request(
        server,
        client_id,
        api::schema::Method::ClientShellPopupOverlayOpen(
            api::schema::ClientShellPopupOverlayOpenParams {
                command_id,
                view: view(),
            },
        ),
    )
    .0
}

fn response(control: &std::sync::mpsc::Receiver<Vec<u8>>) -> serde_json::Value {
    loop {
        match read_server_message(control.recv().expect("endpoint response")) {
            ServerMessage::ClientShellEndpointResponseChunk { data, .. } => {
                return serde_json::from_slice(&data).unwrap();
            }
            _ => continue,
        }
    }
}

/// Install a popup that the overlay client owns, with a capturable input channel.
fn install_overlay_popup(
    server: &mut HeadlessServer,
) -> (
    crate::terminal::TerminalId,
    tokio::sync::mpsc::Receiver<Bytes>,
) {
    let (runtime, input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            40,
            12,
            0,
            b"LOCAL_POPUP",
            4,
        );
    let (_, terminal_id) = server.app.install_test_popup_runtime(runtime);
    server.popup_overlay = Some(super::super::popup_overlay::PopupOverlayOwner::test_new(
        OVERLAY_CLIENT,
        terminal_id.clone(),
    ));
    (terminal_id, input_rx)
}

fn drain_surfaces(render: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<protocol::PaneSurfaceFrame> {
    let mut surfaces = Vec::new();
    while let Ok(bytes) = render.try_recv() {
        if let ServerMessage::PaneSurface(surface) = read_server_message(bytes) {
            surfaces.push(surface);
        }
    }
    surfaces
}

#[tokio::test]
async fn presenting_elsewhere_client_opens_a_local_overlay_with_view_env() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    install_local_popup(
        &mut server,
        local_popup_binding(
            "printf 'VIEW=%s|%s|%s|%s\\n' \"$HERDR_VIEW_ENDPOINT\" \"$HERDR_VIEW_ENDPOINT_LABEL\" \"$HERDR_VIEW_WORKSPACE_ID\" \"$HERDR_VIEW_PANE_CWD\"; sleep 30",
        ),
    );
    let (control, render) = connect_presenting_elsewhere(&mut server);

    assert!(open(&mut server, OVERLAY_CLIENT));

    assert!(response(&control).get("result").is_some());
    assert!(server.app.state.popup_pane.is_some());
    assert!(server.is_popup_overlay_client(OVERLAY_CLIENT));
    assert_eq!(server.popup_owner_tab_id, None);
    let terminal_id = server
        .app
        .state
        .popup_pane
        .as_ref()
        .unwrap()
        .terminal_id
        .clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .recent_unwrapped_text_snapshot(usize::MAX)
            .text;
        if text.contains("VIEW=0123456789abcdef0123456789abcdef|macmini|w9|/remote/path") {
            break;
        }
        assert!(Instant::now() < deadline, "popup env missing: {text:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.render_and_stream();
    let surfaces = drain_surfaces(&render);
    let [surface] = surfaces.as_slice() else {
        panic!("expected one overlay surface, got {}", surfaces.len());
    };
    assert!(surface.panes.is_empty());
    assert_eq!(surface.frame.width, 0);
    assert!(surface
        .popup
        .as_ref()
        .is_some_and(|popup| popup.terminal_id == terminal_id.to_string()));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn overlay_surfaces_are_popup_only_and_other_viewers_do_not_see_the_popup() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (_viewer_control, viewer_render) = connect_matching_test_shell(&mut server, LOCAL_VIEWER);
    let (_control, render) = connect_presenting_elsewhere(&mut server);
    install_overlay_popup(&mut server);

    server.render_and_stream();

    let surfaces = drain_surfaces(&render);
    let [surface] = surfaces.as_slice() else {
        panic!("expected one overlay surface, got {}", surfaces.len());
    };
    assert!(surface.panes.is_empty());
    assert!(surface.splits.is_empty());
    assert!(surface.frame.cells.is_empty());
    let popup = surface.popup.as_ref().expect("overlay popup");
    assert!(frame_text(&popup.frame).contains("LOCAL_POPUP"));
    let viewer = recv_pane_surface(&viewer_render, "local viewer");
    assert!(viewer.popup.is_none());
    assert!(frame_text(&viewer.frame).contains("BASE"));

    // An unchanged popup is not streamed again.
    server.render_and_stream();
    assert!(drain_surfaces(&render).is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn overlay_client_drives_popup_input_while_other_viewers_cannot() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (_viewer_control, _viewer_render) = connect_matching_test_shell(&mut server, LOCAL_VIEWER);
    let (_control, _render) = connect_presenting_elsewhere(&mut server);
    let (terminal_id, mut input_rx) = install_overlay_popup(&mut server);

    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: LOCAL_VIEWER,
        terminal_id: terminal_id.to_string(),
        events: vec![protocol::ClientPaneInputEvent::Paste("viewer".into())],
    });
    assert!(input_rx.try_recv().is_err(), "other viewer drove the popup");

    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: OVERLAY_CLIENT,
        terminal_id: terminal_id.to_string(),
        events: vec![protocol::ClientPaneInputEvent::Paste("overlay".into())],
    });
    let mut received = Vec::new();
    while let Ok(bytes) = input_rx.try_recv() {
        received.extend_from_slice(&bytes);
    }
    assert!(
        String::from_utf8_lossy(&received).contains("overlay"),
        "{received:?}"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn ended_popup_sends_a_final_popup_less_surface() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (_control, render) = connect_presenting_elsewhere(&mut server);
    install_overlay_popup(&mut server);
    server.render_and_stream();
    assert_eq!(drain_surfaces(&render).len(), 1);

    assert!(server.app.close_popup_pane());
    server.render_and_stream();

    let surfaces = drain_surfaces(&render);
    let [surface] = surfaces.as_slice() else {
        panic!("expected the closing surface, got {}", surfaces.len());
    };
    assert!(surface.popup.is_none());
    assert!(server.popup_overlay.is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn overlay_close_and_disconnect_end_the_popup() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (control, _render) = connect_presenting_elsewhere(&mut server);
    install_overlay_popup(&mut server);

    overlay_request(
        &mut server,
        LOCAL_VIEWER,
        api::schema::Method::ClientShellPopupOverlayClose(api::schema::EmptyParams::default()),
    );
    assert!(
        server.app.state.popup_pane.is_some(),
        "only the owner may close it"
    );

    overlay_request(
        &mut server,
        OVERLAY_CLIENT,
        api::schema::Method::ClientShellPopupOverlayClose(api::schema::EmptyParams::default()),
    );
    assert!(response(&control).get("result").is_some());
    assert!(server.app.state.popup_pane.is_none());
    assert!(server.popup_overlay.is_none());

    install_overlay_popup(&mut server);
    server.remove_client_and_resize_if_needed(OVERLAY_CLIENT);
    assert!(server.app.state.popup_pane.is_none());
    assert!(server.popup_overlay.is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn overlay_open_is_refused_while_another_popup_runs_or_for_endpoint_commands() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (control, _render) = connect_presenting_elsewhere(&mut server);
    let mut endpoint_popup = local_popup_binding("sleep 30");
    endpoint_popup.run_on = crate::config::CommandRunOn::Endpoint;
    install_local_popup(&mut server, endpoint_popup);

    open(&mut server, OVERLAY_CLIENT);
    assert_eq!(response(&control)["error"]["code"], "command_failed");
    assert!(server.app.state.popup_pane.is_none());

    install_local_popup(&mut server, local_popup_binding("sleep 30"));
    server.app.install_test_popup_runtime(
        crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"OTHER"),
    );
    open(&mut server, OVERLAY_CLIENT);
    assert_eq!(response(&control)["error"]["code"], "popup_open");
    assert!(server.popup_overlay.is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn overlay_client_resize_reflows_the_popup() {
    let mut server = test_headless_server();
    install_shared_view_test_runtime(&mut server);
    let (_control, _render) = connect_presenting_elsewhere(&mut server);
    install_overlay_popup(&mut server);

    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: OVERLAY_CLIENT,
        surface_cols: 120,
        surface_rows: 40,
        cell_width_px: 8,
        cell_height_px: 16,
        pixel_mouse: false,
    }));
    assert_eq!(server.clients[&OVERLAY_CLIENT].terminal_size, (120, 40));
    shutdown_test_runtimes(&mut server);
}
