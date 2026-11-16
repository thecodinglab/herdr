//! `run_on = "local"` popups: a popup that runs on the local server while the client presents
//! another machine. The local server streams popup-only surfaces for this client connection; the
//! client draws them over the presented remote surface and routes popup input back to Local.

use super::*;

pub(crate) const POPUP_OVERLAY_OPEN_METHOD: &str = "client_shell.popup_overlay.open";

/// Client-owned presentation state for one local popup overlay.
#[derive(Debug)]
pub(crate) struct ClientLocalPopupOverlay {
    /// Local connection generation that owns the overlay. `None` is reserved for tests.
    pub(super) generation: Option<u64>,
    pub(super) boot_id: String,
    /// Open request that has not been answered yet.
    pub(super) request_id: Option<String>,
    /// Latest popup surface. `None` until the local server's first overlay frame.
    pub(super) popup: Option<Box<crate::protocol::ClientShellPopupSurface>>,
}

/// One local overlay endpoint request, routed to the Local connection whatever is presented.
#[derive(Debug)]
pub(crate) struct ClientLocalPopupOverlayRequest {
    pub(crate) generation: Option<u64>,
    pub(crate) boot_id: String,
    pub(crate) request: Box<crate::api::schema::Request>,
}

impl ClientShellState {
    /// Local overlay commands are bound only while another machine is presented. Presenting
    /// Local runs the same commands as ordinary Local popups.
    pub(super) fn wants_local_popup_overlays(&self) -> bool {
        !self.active_endpoint_id.is_local()
    }

    /// Whether Local surfaces must currently be accepted for the overlay of `generation`.
    pub(crate) fn accepts_local_popup_overlay_surface(&self, generation: u64) -> bool {
        self.local_popup_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.generation.is_none_or(|owner| owner == generation))
    }

    pub(crate) fn local_popup_overlay_active(&self) -> bool {
        self.local_popup_overlay.is_some()
    }

    /// The local popup terminal that receives input while the overlay is visible.
    pub(crate) fn local_popup_overlay_terminal_id(&self) -> Option<&str> {
        self.local_popup_overlay
            .as_ref()
            .and_then(|overlay| overlay.popup.as_deref())
            .map(|popup| popup.terminal_id.as_str())
    }

    pub(crate) fn owns_local_popup_overlay_request(&self, request_id: &str) -> bool {
        self.local_popup_overlay
            .as_ref()
            .and_then(|overlay| overlay.request_id.as_deref())
            == Some(request_id)
    }

    fn local_popup_overlay_view(&self) -> crate::api::schema::ClientShellPopupOverlayView {
        let (endpoint_id, endpoint_label) = match &self.active_endpoint_id {
            ClientEndpointId::Local => ("local".to_owned(), "local".to_owned()),
            ClientEndpointId::Ssh(profile_id) => (
                profile_id.to_string(),
                self.endpoint_label(&self.active_endpoint_id).to_owned(),
            ),
        };
        let snapshot = self.snapshot.as_deref();
        let pane_cwd = snapshot.and_then(|snapshot| {
            let pane_id = snapshot.focused_pane_id.as_deref()?;
            let pane = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id)?;
            pane.foreground_cwd.clone().or_else(|| pane.cwd.clone())
        });
        crate::api::schema::ClientShellPopupOverlayView {
            endpoint_id,
            endpoint_label,
            workspace_id: snapshot.and_then(|snapshot| snapshot.focused_workspace_id.clone()),
            tab_id: snapshot.and_then(|snapshot| snapshot.focused_tab_id.clone()),
            pane_id: snapshot.and_then(|snapshot| snapshot.focused_pane_id.clone()),
            pane_cwd,
        }
    }

    /// Ask the local server to open `command` as an overlay over the presented machine.
    pub(super) fn open_local_popup_overlay(
        &mut self,
        command: &crate::config::CustomCommandKeybind,
        outcome: &mut ClientShellInput,
    ) {
        if self.local_popup_overlay.is_some() || self.popup_pending {
            return;
        }
        let local = ClientEndpointId::Local;
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == local)
            .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
            .filter(|endpoint| endpoint.snapshot.is_some())
        else {
            let label = self.endpoint_label(&local).to_owned();
            outcome.repaint |= self.receive_endpoint_unavailable(format!("{label} is not ready"));
            return;
        };
        if !endpoint
            .methods
            .as_ref()
            .is_some_and(|methods| methods.contains(POPUP_OVERLAY_OPEN_METHOD))
        {
            outcome.repaint |= self.push_endpoint_notice(
                ClientEndpointNoticeKind::Unsupported,
                POPUP_OVERLAY_OPEN_METHOD,
                "Action unavailable",
                "The local server does not support local popups yet. Update and restart it to enable this action.",
            );
            return;
        }
        let snapshot = endpoint
            .snapshot
            .as_deref()
            .expect("checked local snapshot");
        let labels = command.bindings.labels();
        let mut candidates = snapshot.commands.iter().filter(|candidate| {
            candidate.action == crate::protocol::ClientShellCommandAction::Popup
                && !labels.is_empty()
                && labels
                    .iter()
                    .all(|label| candidate.binding_labels.contains(label))
        });
        let command_id = match (candidates.next(), candidates.next()) {
            (Some(candidate), None) => candidate.command_id.clone(),
            _ => {
                self.set_endpoint_error(
                    "local popup command is not available on the local server; reload configuration",
                );
                outcome.repaint = true;
                return;
            }
        };
        let boot_id = snapshot.boot_id.clone();
        let generation = endpoint.snapshot_generation;
        let view = self.local_popup_overlay_view();
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let request_id = format!("client-shell-popup-overlay:{request_id}");
        self.local_popup_overlay = Some(ClientLocalPopupOverlay {
            generation,
            boot_id: boot_id.clone(),
            request_id: Some(request_id.clone()),
            popup: None,
        });
        self.popup_pending = true;
        self.popup_pending_deadline = None;
        outcome
            .actions
            .push(ClientShellAction::LocalPopupOverlay(Box::new(
                ClientLocalPopupOverlayRequest {
                    generation,
                    boot_id,
                    request: Box::new(crate::api::schema::Request {
                        id: request_id,
                        method: crate::api::schema::Method::ClientShellPopupOverlayOpen(
                            crate::api::schema::ClientShellPopupOverlayOpenParams {
                                command_id,
                                view,
                            },
                        ),
                    }),
                },
            )));
    }

    /// Complete the overlay open request. Returns whether the shell must repaint.
    pub(crate) fn handle_local_popup_overlay_result(
        &mut self,
        request_id: &str,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        if !self.owns_local_popup_overlay_request(request_id) {
            return false;
        }
        match result {
            Ok(_) => {
                if let Some(overlay) = self.local_popup_overlay.as_mut() {
                    overlay.request_id = None;
                }
                if self.local_popup_overlay_terminal_id().is_none() {
                    self.popup_pending_deadline =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                }
                false
            }
            Err(error) => {
                let code = error.code.as_deref().unwrap_or("invalid_response");
                let message = error.message.clone();
                self.clear_local_popup_overlay();
                self.push_endpoint_notice(
                    ClientEndpointNoticeKind::Rejected,
                    format!("{POPUP_OVERLAY_OPEN_METHOD}:{code}"),
                    "Local popup unavailable",
                    message,
                );
                true
            }
        }
    }

    /// Apply one popup-only Local surface for the overlay. Returns whether the shell must repaint.
    pub(crate) fn receive_local_popup_overlay_surface(
        &mut self,
        generation: u64,
        surface: PaneSurfaceFrame,
    ) -> bool {
        if !self.accepts_local_popup_overlay_surface(generation) {
            return false;
        }
        let Some(overlay) = self.local_popup_overlay.as_mut() else {
            return false;
        };
        if surface.boot_id != overlay.boot_id {
            return false;
        }
        match surface.popup {
            Some(popup) => {
                overlay.popup = Some(popup);
                self.reconcile_popup_terminal();
            }
            None => {
                // A popup-less frame after the open request ends the overlay. Before the
                // response, the local server has not started it yet.
                if overlay.request_id.is_some() && overlay.popup.is_none() {
                    return false;
                }
                self.clear_local_popup_overlay();
            }
        }
        true
    }

    /// Forget the overlay without asking the local server, e.g. when Local disconnected.
    pub(crate) fn clear_local_popup_overlay(&mut self) -> bool {
        if self.local_popup_overlay.take().is_none() {
            return false;
        }
        self.popup_pending = false;
        self.popup_pending_deadline = None;
        self.reconcile_popup_terminal();
        true
    }

    /// Close an open overlay before the client changes what it presents. Returns the close
    /// request for the local server, which ends the popup process.
    pub(crate) fn close_local_popup_overlay(&mut self) -> Option<ClientLocalPopupOverlayRequest> {
        let overlay = self.local_popup_overlay.as_ref()?;
        let generation = overlay.generation;
        let boot_id = overlay.boot_id.clone();
        self.clear_local_popup_overlay();
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        Some(ClientLocalPopupOverlayRequest {
            generation,
            boot_id,
            request: Box::new(crate::api::schema::Request {
                id: format!("client-shell-popup-overlay:{request_id}"),
                method: crate::api::schema::Method::ClientShellPopupOverlayClose(
                    crate::api::schema::EmptyParams::default(),
                ),
            }),
        })
    }

    /// The popup currently receiving input: the local overlay first, then the presented
    /// machine's own popup.
    pub(super) fn reconcile_popup_terminal(&mut self) {
        let next_popup = self
            .local_popup_overlay_terminal_id()
            .map(str::to_owned)
            .or_else(|| {
                self.pane_surface
                    .as_ref()
                    .and_then(|surface| surface.popup.as_deref())
                    .map(|popup| popup.terminal_id.clone())
            });
        self.transition_popup_terminal(next_popup);
    }
}

#[cfg(test)]
impl ClientShellState {
    pub(crate) fn test_set_local_popup_overlay(&mut self, terminal_id: &str) {
        let popup = crate::protocol::ClientShellPopupSurface {
            terminal_id: terminal_id.to_owned(),
            title: "local".into(),
            width: None,
            height: None,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.local_popup_overlay = Some(ClientLocalPopupOverlay {
            generation: None,
            boot_id: "boot".into(),
            request_id: None,
            popup: Some(Box::new(popup)),
        });
        self.reconcile_popup_terminal();
    }
}
