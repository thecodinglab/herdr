//! Local popup overlays: a `run_on = "local"` popup opened by a client shell that presents
//! another machine. The popup stays this server's one popup, but only the requesting client sees
//! and drives it. While that client's surface is inactive here, it receives popup-only surfaces
//! and never this server's panes.

use super::*;

pub(super) struct PopupOverlayOwner {
    pub(super) client_id: u64,
    terminal_id: crate::terminal::TerminalId,
    /// Popup last streamed to the inactive client, used to skip identical frames.
    last_sent: Option<Box<protocol::ClientShellPopupSurface>>,
    sent_any: bool,
    surface_revision: u64,
}

impl HeadlessServer {
    /// Whether `client_id` presents and drives this server's popup.
    pub(super) fn client_sees_popup(&self, client_id: u64) -> bool {
        match &self.popup_overlay {
            Some(owner) => owner.client_id == client_id,
            None => self.popup_owner_tab_id == self.shell_tab_id_for_client(client_id),
        }
    }

    pub(super) fn is_popup_overlay_client(&self, client_id: u64) -> bool {
        self.popup_overlay
            .as_ref()
            .is_some_and(|owner| owner.client_id == client_id)
    }

    /// Answer `client_shell.popup_overlay.*` on the endpoint lane. These run before the
    /// active-surface gate because the requesting client presents another machine.
    pub(super) fn handle_popup_overlay_request(
        &mut self,
        client_id: u64,
        boot_id: String,
        request_id: String,
        method: &api::schema::Method,
    ) -> bool {
        let result = match method {
            api::schema::Method::ClientShellPopupOverlayOpen(params) => {
                self.open_popup_overlay(client_id, params)
            }
            api::schema::Method::ClientShellPopupOverlayClose(_) => {
                Ok(self.close_popup_overlay_for(client_id))
            }
            _ => return false,
        };
        let (message, changed) = match result {
            Ok(changed) => (
                crate::server::client_commands::success_message_with_result(
                    boot_id,
                    request_id,
                    api::schema::ResponseResult::Ok {},
                ),
                changed,
            ),
            Err((code, message)) => (
                crate::server::client_commands::error_message(boot_id, request_id, code, message),
                false,
            ),
        };
        self.send_to_client(client_id, message);
        changed
    }

    fn open_popup_overlay(
        &mut self,
        client_id: u64,
        params: &api::schema::ClientShellPopupOverlayOpenParams,
    ) -> Result<bool, (&'static str, String)> {
        if self.handoff_in_progress {
            return Err(("server_unavailable", "server is handing off".to_owned()));
        }
        if self.app.state.popup_pane.is_some() {
            return Err((
                "popup_open",
                "another popup is already open on this machine".to_owned(),
            ));
        }
        let Some(binding) = self.app.resolve_client_shell_command(&params.command_id) else {
            return Err((
                "command_not_found",
                "custom command manifest is stale; reload configuration".to_owned(),
            ));
        };
        let Some((cols, rows)) = self
            .clients
            .get(&client_id)
            .map(|client| client.terminal_size)
        else {
            return Ok(false);
        };
        self.app
            .spawn_overlay_popup_command(&binding, &params.view, Rect::new(0, 0, cols, rows))
            .map_err(|err| ("command_failed", err.to_string()))?;
        let Some(terminal_id) = self
            .app
            .state
            .popup_pane
            .as_ref()
            .map(|popup| popup.terminal_id.clone())
        else {
            return Err(("command_failed", "popup did not start".to_owned()));
        };
        self.popup_owner_tab_id = None;
        self.popup_overlay = Some(PopupOverlayOwner {
            client_id,
            terminal_id,
            last_sent: None,
            sent_any: false,
            surface_revision: 0,
        });
        self.app.render_dirty.request_generic();
        Ok(true)
    }

    /// The overlay client stopped presenting the popup; end its process.
    fn close_popup_overlay_for(&mut self, client_id: u64) -> bool {
        if !self.is_popup_overlay_client(client_id) {
            return false;
        }
        let owner = self.popup_overlay.take().expect("checked overlay owner");
        self.close_overlay_popup_pane(&owner)
    }

    fn close_overlay_popup_pane(&mut self, owner: &PopupOverlayOwner) -> bool {
        if self
            .app
            .state
            .popup_pane
            .as_ref()
            .is_some_and(|popup| popup.terminal_id == owner.terminal_id)
        {
            self.app.close_popup_pane()
        } else {
            false
        }
    }

    /// A disconnected overlay client can no longer close its popup.
    pub(super) fn retire_popup_overlay_client(&mut self, client_id: u64) {
        if self.is_popup_overlay_client(client_id) {
            let owner = self.popup_overlay.take().expect("checked overlay owner");
            self.close_overlay_popup_pane(&owner);
        }
    }

    /// Stream the overlay popup to its client while that client presents another machine.
    /// This renders only the popup terminal, so its cost is independent of this server's panes.
    pub(super) fn stream_popup_overlay(&mut self) {
        let Some(owner) = self.popup_overlay.as_ref() else {
            return;
        };
        let client_id = owner.client_id;
        let popup_alive = self
            .app
            .state
            .popup_pane
            .as_ref()
            .is_some_and(|popup| popup.terminal_id == owner.terminal_id);
        let Some(client) = self.clients.get(&client_id) else {
            self.popup_overlay = None;
            return;
        };
        if client.shell_surface_active {
            // The client presents this server again: its ordinary surface carries the popup.
            if !popup_alive {
                self.popup_overlay = None;
            }
            return;
        }
        let area = Rect::new(0, 0, client.terminal_size.0, client.terminal_size.1);
        let cell_size = if client.cell_size.is_known() {
            client.cell_size
        } else {
            crate::kitty_graphics::HostCellSize::default()
        };
        let projection_revision = client.shell_projection_revision;
        let popup = if popup_alive {
            if self
                .app
                .state
                .popup_pane
                .as_ref()
                .and_then(|popup| self.app.terminal_runtimes.get(&popup.terminal_id))
                .is_some_and(|runtime| runtime.synchronized_output_state().0)
            {
                // The end of synchronized output schedules the next render.
                return;
            }
            crate::server::client_shell::render_popup_surface(&self.app, area, true, cell_size)
        } else {
            None
        };
        if popup_alive && popup.is_none() {
            // Too small to draw: keep the overlay until the client grows again.
            return;
        }
        let surface_revision = {
            let owner = self.popup_overlay.as_mut().expect("checked overlay owner");
            if popup_alive && owner.sent_any && owner.last_sent == popup {
                return;
            }
            owner.surface_revision.saturating_add(1)
        };
        let surface = protocol::PaneSurfaceFrame {
            boot_id: self.client_shell_boot_id.clone(),
            projection_revision,
            surface_revision,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
            popup: popup.clone(),
            graphics: Default::default(),
        };
        let framed = match Self::frame_server_message(&ServerMessage::PaneSurface(surface)) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(client_id, err = %err, "failed to frame popup overlay surface");
                return;
            }
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let Some(writer) = client.writer.as_ref() else {
            return;
        };
        match writer.render.try_send(framed) {
            Ok(()) => {
                // A plain surface replaces the client's decoder baseline; the next ordinary
                // surface for this connection must be complete.
                client.request_repaint();
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                client.defer_full_render();
                return;
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
        }
        if popup_alive {
            let owner = self.popup_overlay.as_mut().expect("checked overlay owner");
            owner.surface_revision = surface_revision;
            owner.last_sent = popup;
            owner.sent_any = true;
        } else {
            self.popup_overlay = None;
        }
    }
}

#[cfg(test)]
impl PopupOverlayOwner {
    pub(super) fn test_new(client_id: u64, terminal_id: crate::terminal::TerminalId) -> Self {
        Self {
            client_id,
            terminal_id,
            last_sent: None,
            sent_any: false,
            surface_revision: 0,
        }
    }
}
