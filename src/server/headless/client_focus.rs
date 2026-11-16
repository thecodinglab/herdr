use super::*;

impl HeadlessServer {
    /// Forwards `client.focus` to attached client shells that advertised support.
    ///
    /// Only the most recently active shell receives it unless `all_clients` is set.
    /// The receiving shell resolves the endpoint against its own catalog, so this
    /// server only validates the workspace when the request targets itself.
    pub(super) fn handle_client_focus_api(
        &mut self,
        id: String,
        params: api::schema::ClientFocusParams,
    ) -> String {
        let error = |code: &str, message: String| {
            serde_json::to_string(&api::schema::ErrorResponse {
                id: id.clone(),
                error: api::schema::ErrorBody {
                    code: code.into(),
                    message,
                },
            })
            .unwrap_or_else(|_| "{}".to_string())
        };
        if params.workspace_id.trim().is_empty() {
            return error("invalid_params", "workspace_id is empty".into());
        }
        if params
            .endpoint
            .as_deref()
            .is_some_and(|endpoint| endpoint.trim().is_empty())
        {
            return error("invalid_params", "endpoint is empty".into());
        }
        if params.endpoint.is_none() && self.app.parse_workspace_id(&params.workspace_id).is_none()
        {
            return error(
                "workspace_not_found",
                format!("workspace {} not found", params.workspace_id),
            );
        }
        let message = match crate::protocol::endpoint::client_focus_message(
            &crate::protocol::endpoint::EndpointClientFocus {
                workspace_id: params.workspace_id,
                endpoint: params.endpoint,
            },
        ) {
            Ok(message) => message,
            Err(err) => return error("internal_error", err.to_string()),
        };
        let mut delivered = 0_u64;
        for client_id in
            crate::server::clients::client_focus_targets(&self.clients, params.all_clients)
        {
            if self.send_to_client(client_id, message.clone()) {
                delivered += 1;
            }
        }
        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: api::schema::ResponseResult::ClientFocus { delivered },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }
}
