use super::*;

fn connect_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    client_focus: bool,
    surface_active: bool,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        client_id,
        surface_cols: 80,
        surface_rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        direct_graphics: false,
        endpoint_keybindings: false,
        mouse_capture: false,
        surface_active,
        surface_reuse: false,
        surface_delta: false,
        surface_scroll: false,
        client_focus,
        writer,
    });
    control_rx
}

fn client_focus(
    server: &mut HeadlessServer,
    workspace_id: &str,
    endpoint: Option<&str>,
    all_clients: bool,
) -> serde_json::Value {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "focus".into(),
            method: api::schema::Method::ClientFocus(api::schema::ClientFocusParams {
                workspace_id: workspace_id.into(),
                endpoint: endpoint.map(str::to_owned),
                all_clients,
            }),
        },
        respond_to,
        response_write_complete: None,
    });
    serde_json::from_str(&response_rx.recv().unwrap()).unwrap()
}

/// Returns every forwarded focus request queued for one client, dropping other controls.
fn received_focus(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Vec<protocol::endpoint::EndpointClientFocus> {
    receiver
        .try_iter()
        .filter_map(|bytes| match read_server_message(bytes) {
            ServerMessage::EndpointControl { kind, data }
                if kind == protocol::endpoint::CLIENT_FOCUS_KIND =>
            {
                Some(serde_json::from_str(&data).unwrap())
            }
            _ => None,
        })
        .collect()
}

fn focus_test_server() -> HeadlessServer {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("focus")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server
}

#[tokio::test]
async fn client_focus_reaches_only_the_most_recently_active_capable_shell() {
    let mut server = focus_test_server();
    let first = connect_shell(&mut server, 1, true, true);
    let second = connect_shell(&mut server, 2, true, true);
    // Newest, but an older client that never advertised the control.
    let legacy = connect_shell(&mut server, 3, false, true);
    let workspace_id = server.app.public_workspace_id(0);
    for receiver in [&first, &second, &legacy] {
        received_focus(receiver);
    }

    let response = client_focus(&mut server, &workspace_id, None, false);
    assert_eq!(response["result"]["type"], "client_focus");
    assert_eq!(response["result"]["delivered"], 1);
    assert!(received_focus(&first).is_empty());
    assert_eq!(
        received_focus(&second),
        vec![protocol::endpoint::EndpointClientFocus {
            workspace_id: workspace_id.clone(),
            endpoint: None,
        }]
    );
    assert!(received_focus(&legacy).is_empty());

    // Interaction makes the first shell the most recently active one.
    server.promote_client_to_foreground(1);
    let response = client_focus(&mut server, &workspace_id, Some("macmini"), false);
    assert_eq!(response["result"]["delivered"], 1);
    assert_eq!(
        received_focus(&first),
        vec![protocol::endpoint::EndpointClientFocus {
            workspace_id: workspace_id.clone(),
            endpoint: Some("macmini".into()),
        }]
    );
    assert!(received_focus(&second).is_empty());

    let response = client_focus(&mut server, &workspace_id, None, true);
    assert_eq!(response["result"]["delivered"], 2);
    assert_eq!(received_focus(&first).len(), 1);
    assert_eq!(received_focus(&second).len(), 1);
    assert!(received_focus(&legacy).is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_focus_reaches_a_shell_that_is_viewing_another_machine() {
    let mut server = focus_test_server();
    let background = connect_shell(&mut server, 1, true, false);
    received_focus(&background);
    let workspace_id = server.app.public_workspace_id(0);

    let response = client_focus(&mut server, &workspace_id, None, false);
    assert_eq!(response["result"]["delivered"], 1);
    assert_eq!(received_focus(&background).len(), 1);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_focus_reports_zero_deliveries_and_rejects_bad_targets() {
    let mut server = focus_test_server();
    let legacy = connect_shell(&mut server, 1, false, true);
    received_focus(&legacy);
    let workspace_id = server.app.public_workspace_id(0);

    let response = client_focus(&mut server, &workspace_id, None, true);
    assert_eq!(response["result"]["delivered"], 0);

    let response = client_focus(&mut server, "w_missing", None, false);
    assert_eq!(response["error"]["code"], "workspace_not_found");
    // Another machine's workspace ids are not validated against this server.
    let response = client_focus(&mut server, "w_missing", Some("macmini"), false);
    assert_eq!(response["result"]["delivered"], 0);
    let response = client_focus(&mut server, "", None, false);
    assert_eq!(response["error"]["code"], "invalid_params");
    let response = client_focus(&mut server, &workspace_id, Some(" "), false);
    assert_eq!(response["error"]["code"], "invalid_params");
    assert!(received_focus(&legacy).is_empty());
    shutdown_test_runtimes(&mut server);
}
