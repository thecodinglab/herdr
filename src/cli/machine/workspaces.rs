//! `herdr machine workspaces`: one listing of Local and every saved SSH machine.
//!
//! Every machine is queried on its own thread with a single read-only
//! `session.snapshot` request, so one slow or broken machine only changes its
//! own status. The snapshot carries the server protocol, which replaces the
//! separate ping that `ensure_server_protocol_compatible` would cost over SSH.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget};
use crate::api::schema::{EmptyParams, Method, Request};
use crate::client::endpoint::{EndpointCatalog, SavedSshEndpoint};

pub(super) const USAGE: &str = "usage: herdr machine workspaces [--json] [--timeout-ms <N>]";
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);
/// Cancelled workers get this long to stop their SSH children before exit.
const CANCEL_GRACE: Duration = Duration::from_secs(1);
const REQUEST_ID: &str = "cli:machine:workspaces";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    json: bool,
    timeout: Duration,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let args = super::super::expand_equals_args(args, &["--timeout-ms"]);
    let mut json = false;
    let mut timeout = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" if !json => json = true,
            "--timeout-ms" if timeout.is_none() => {
                let value = args
                    .get(index + 1)
                    .ok_or("missing value for --timeout-ms")?;
                let millis = value
                    .parse::<u64>()
                    .ok()
                    .filter(|millis| *millis > 0)
                    .ok_or_else(|| {
                        format!("--timeout-ms must be a positive number of milliseconds: {value}")
                    })?;
                timeout = Some(Duration::from_millis(millis));
                index += 1;
            }
            _ => return Err(USAGE.to_owned()),
        }
        index += 1;
    }
    Ok(Args {
        json,
        timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
    })
}

pub(super) fn run(args: &[String]) -> io::Result<i32> {
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let profiles = EndpointCatalog::load_profiles().map_err(io::Error::other)?;
    let mut machines = vec![Machine::Local];
    machines.extend(profiles.into_iter().map(Machine::Ssh));
    let report = Report {
        machines: collect(machines, args.timeout, Arc::new(query_machine)),
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(io::Error::other)?
        );
    } else {
        print!("{}", render_text(&report));
    }
    Ok(0)
}

#[derive(Debug, Clone)]
enum Machine {
    Local,
    Ssh(SavedSshEndpoint),
}

impl Machine {
    fn id(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Ssh(profile) => profile.id.to_string(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Ssh(profile) => profile.label.clone(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh(_) => "ssh",
        }
    }

    fn enabled(&self) -> bool {
        match self {
            Self::Local => true,
            Self::Ssh(profile) => profile.enabled,
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    machines: Vec<MachineReport>,
}

#[derive(Debug, Serialize)]
struct MachineReport {
    id: String,
    label: String,
    kind: &'static str,
    status: Status,
    error: Option<String>,
    elapsed_ms: u64,
    workspaces: Vec<WorkspaceRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Offline,
    Disabled,
    AuthRequired,
    Timeout,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Offline => "offline",
            Self::Disabled => "disabled",
            Self::AuthRequired => "auth_required",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct WorkspaceRow {
    workspace_id: String,
    number: usize,
    label: String,
    focused: bool,
    agent_status: Option<String>,
    agent_title: Option<String>,
    tab_count: usize,
    cwd: Option<String>,
    branch: Option<String>,
    worktree: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    status: Status,
    message: Option<String>,
}

impl Failure {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: Some(message.into()),
        }
    }

    fn timeout(timeout: Duration) -> Self {
        Self::new(
            Status::Timeout,
            format!("no response within {} ms", timeout.as_millis()),
        )
    }
}

type Outcome = Result<Vec<WorkspaceRow>, Failure>;
type Query = dyn Fn(&Machine, Duration, &Cancel) -> Outcome + Send + Sync;

/// Cancels the current connection attempt of one machine. Each bridge attempt
/// gets a fresh flag because dropping a bridge also raises its flag.
#[derive(Default)]
struct Cancel {
    state: Mutex<(bool, Option<Arc<AtomicBool>>)>,
}

impl Cancel {
    fn attempt(&self) -> Option<Arc<AtomicBool>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.0 {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        state.1 = Some(Arc::clone(&flag));
        Some(flag)
    }

    fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        if let Some(flag) = &state.1 {
            flag.store(true, Ordering::Release);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .0
    }
}

fn collect(machines: Vec<Machine>, timeout: Duration, query: Arc<Query>) -> Vec<MachineReport> {
    let started = Instant::now();
    let deadline = started + timeout;
    let (sender, receiver) = mpsc::channel::<(usize, Outcome, Duration)>();
    let mut outcomes: Vec<Option<(Outcome, Duration)>> = vec![None; machines.len()];
    let mut cancels = Vec::with_capacity(machines.len());
    let mut pending = 0;
    for (index, machine) in machines.iter().enumerate() {
        let cancel = Arc::new(Cancel::default());
        cancels.push(Arc::clone(&cancel));
        if !machine.enabled() {
            outcomes[index] = Some((
                Err(Failure {
                    status: Status::Disabled,
                    message: None,
                }),
                Duration::ZERO,
            ));
            continue;
        }
        let sender = sender.clone();
        let query = Arc::clone(&query);
        let machine = machine.clone();
        let spawned = std::thread::Builder::new()
            .name("herdr-machine-workspaces".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    query(&machine, timeout, &cancel)
                }))
                .unwrap_or_else(|_| Err(Failure::new(Status::Error, "machine query panicked")));
                let _ = sender.send((index, outcome, started.elapsed()));
            });
        match spawned {
            Ok(_) => pending += 1,
            Err(error) => {
                outcomes[index] = Some((
                    Err(Failure::new(
                        Status::Error,
                        format!("could not start machine query: {error}"),
                    )),
                    Duration::ZERO,
                ));
            }
        }
    }
    drop(sender);

    while pending > 0 {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match receiver.recv_timeout(remaining) {
            Ok((index, outcome, elapsed)) => {
                outcomes[index] = Some((outcome, elapsed));
                pending -= 1;
            }
            Err(_) => break,
        }
    }
    if pending > 0 {
        for (index, outcome) in outcomes.iter().enumerate() {
            if outcome.is_none() {
                cancels[index].cancel();
            }
        }
        // Late results are discarded; waiting only lets cancelled workers kill
        // their SSH children and remove bridge sockets before the process exits.
        let grace = Instant::now() + CANCEL_GRACE;
        while pending > 0 {
            let Some(remaining) = grace.checked_duration_since(Instant::now()) else {
                break;
            };
            if receiver.recv_timeout(remaining).is_err() {
                break;
            }
            pending -= 1;
        }
    }

    machines
        .iter()
        .zip(outcomes)
        .map(|(machine, outcome)| {
            let (outcome, elapsed) =
                outcome.unwrap_or_else(|| (Err(Failure::timeout(timeout)), timeout));
            let (status, error, workspaces) = match outcome {
                Ok(workspaces) => (Status::Ok, None, workspaces),
                // Socket timeouts surface as raw OS errors; report the budget instead.
                Err(failure) if failure.status == Status::Timeout => (
                    Status::Timeout,
                    Failure::timeout(timeout).message,
                    Vec::new(),
                ),
                Err(failure) => (failure.status, failure.message, Vec::new()),
            };
            MachineReport {
                id: machine.id(),
                label: machine.label(),
                kind: machine.kind(),
                status,
                error,
                elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                workspaces,
            }
        })
        .collect()
}

fn query_machine(machine: &Machine, timeout: Duration, cancel: &Cancel) -> Outcome {
    let deadline = Instant::now() + timeout;
    match machine {
        Machine::Local => query_local(deadline),
        Machine::Ssh(profile) => query_ssh(profile, timeout, deadline, cancel),
    }
}

fn snapshot_request() -> Request {
    Request {
        id: REQUEST_ID.into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    }
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

fn query_local(deadline: Instant) -> Outcome {
    let client = ApiClient::local();
    let Some(timeout) = remaining(deadline) else {
        return Err(Failure::new(Status::Timeout, "no time left to query"));
    };
    match client.request_value_with_timeout(&snapshot_request(), timeout) {
        Ok(value) => snapshot_rows(
            &value,
            &crate::session::active_restart_after_update_guidance(),
        ),
        Err(ApiClientError::Io(error)) if super::super::server_not_running_error(&error) => Err(
            Failure::new(Status::Offline, "local Herdr server is not running"),
        ),
        Err(error) => Err(classify_api_error(error)),
    }
}

fn query_ssh(
    profile: &SavedSshEndpoint,
    timeout: Duration,
    deadline: Instant,
    cancel: &Cancel,
) -> Outcome {
    let guidance = format!(
        "Update Herdr and restart the server on machine '{}' (session {}). Stopping the server exits its pane processes.",
        profile.label, profile.session
    );
    let mut use_cached_metadata = true;
    let mut first_attempt = true;
    loop {
        let flag = cancel.attempt().ok_or_else(|| Failure::timeout(timeout))?;
        let bridge = crate::remote::SavedSshApiBridge::start_cancellable(
            profile.id.as_str(),
            &profile.target,
            &profile.session,
            use_cached_metadata,
            first_attempt,
            flag,
        )
        .map_err(|error| {
            if cancel.is_cancelled() {
                Failure::timeout(timeout)
            } else {
                classify_io_error(&error)
            }
        })?;
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(
            bridge.socket_path().to_owned(),
        ));
        let request_timeout = remaining(deadline).ok_or_else(|| Failure::timeout(timeout))?;
        let error = match client.request_value_with_timeout(&snapshot_request(), request_timeout) {
            Ok(value) => return snapshot_rows(&value, &guidance),
            Err(error) => error,
        };
        if cancel.is_cancelled() || remaining(deadline).is_none() {
            return Err(Failure::timeout(timeout));
        }
        // Mirrors `target::server_status`: a read-only snapshot may be retried
        // once after stale cached metadata or a retired shared SSH transport.
        let failure = bridge.reported_failure();
        let stale_metadata = bridge.used_cached_metadata
            && failure
                .as_ref()
                .is_some_and(crate::remote::SavedSshApiBridge::stale_metadata_failure);
        if !first_attempt || !bridge.can_retry || (!stale_metadata && !bridge.recover_transport()) {
            return Err(match failure {
                Some(failure) => classify_io_error(&failure),
                None => classify_api_error(error),
            });
        }
        if stale_metadata {
            bridge.invalidate_metadata();
        }
        drop(bridge);
        use_cached_metadata = !stale_metadata;
        first_attempt = false;
    }
}

fn classify_io_error(error: &io::Error) -> Failure {
    let message = error.to_string();
    let status = if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        Status::Timeout
    } else if crate::remote::ssh_error_requires_authentication(&message) {
        Status::AuthRequired
    } else {
        Status::Error
    };
    Failure::new(status, message)
}

fn classify_api_error(error: ApiClientError) -> Failure {
    match error {
        ApiClientError::Io(error) => classify_io_error(&error),
        ApiClientError::EmptyResponse => Failure::new(
            Status::Error,
            "the server closed the connection without a response",
        ),
        error => Failure::new(Status::Error, error.to_string()),
    }
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    workspaces: Vec<SnapshotWorkspace>,
    #[serde(default)]
    panes: Vec<SnapshotPane>,
    #[serde(default)]
    layouts: Vec<SnapshotLayout>,
    #[serde(default)]
    agents: Vec<SnapshotAgent>,
}

#[derive(Deserialize)]
struct SnapshotWorkspace {
    workspace_id: String,
    number: usize,
    label: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    tab_count: usize,
    #[serde(default)]
    active_tab_id: Option<String>,
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    worktree: Option<serde_json::Value>,
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Deserialize)]
struct SnapshotPane {
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
}

#[derive(Deserialize)]
struct SnapshotLayout {
    workspace_id: String,
    tab_id: String,
    #[serde(default)]
    focused_pane_id: Option<String>,
}

#[derive(Deserialize)]
struct SnapshotAgent {
    workspace_id: String,
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    terminal_title: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    focused: bool,
}

/// Turns one `session.snapshot` response into rows, rejecting other protocols.
fn snapshot_rows(response: &serde_json::Value, restart_guidance: &str) -> Outcome {
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("server rejected the session snapshot request");
        return Err(Failure::new(Status::Error, message));
    }
    let snapshot = response
        .pointer("/result/snapshot")
        .ok_or_else(|| Failure::new(Status::Error, "server did not return a session snapshot"))?;
    let protocol = snapshot
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .and_then(|protocol| u32::try_from(protocol).ok())
        .ok_or_else(|| {
            Failure::new(
                Status::Error,
                "server snapshot did not include a protocol version",
            )
        })?;
    if let Some(mismatch) =
        super::super::protocol_guard::mismatch_response(REQUEST_ID, protocol, restart_guidance)
    {
        return Err(Failure::new(Status::Error, mismatch.error.message));
    }
    let snapshot = Snapshot::deserialize(snapshot).map_err(|error| {
        Failure::new(
            Status::Error,
            format!("invalid session snapshot from server: {error}"),
        )
    })?;
    Ok(workspace_rows(&snapshot))
}

fn status_priority(status: Option<&str>) -> u8 {
    match status {
        Some("blocked") => 4,
        Some("done") => 3,
        Some("working") => 2,
        Some("idle") => 1,
        _ => 0,
    }
}

fn non_empty(value: Option<&String>) -> Option<&String> {
    value.filter(|value| !value.trim().is_empty())
}

fn workspace_rows(snapshot: &Snapshot) -> Vec<WorkspaceRow> {
    snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let id = workspace.workspace_id.as_str();
            let active_tab = workspace.active_tab_id.as_deref();
            let focused_pane = active_tab
                .and_then(|tab| {
                    snapshot
                        .layouts
                        .iter()
                        .find(|layout| layout.workspace_id == id && layout.tab_id == tab)
                })
                .and_then(|layout| layout.focused_pane_id.as_deref())
                .and_then(|pane_id| snapshot.panes.iter().find(|pane| pane.pane_id == pane_id));
            let fallback_pane_cwd = || {
                snapshot
                    .panes
                    .iter()
                    .filter(|pane| pane.workspace_id == id)
                    .filter(|pane| active_tab.is_none_or(|tab| pane.tab_id == tab))
                    .find_map(|pane| {
                        non_empty(pane.foreground_cwd.as_ref()).or(non_empty(pane.cwd.as_ref()))
                    })
            };
            let cwd = focused_pane
                .and_then(|pane| {
                    non_empty(pane.foreground_cwd.as_ref()).or(non_empty(pane.cwd.as_ref()))
                })
                .cloned()
                .or_else(|| {
                    workspace
                        .worktree
                        .as_ref()
                        .and_then(|worktree| worktree.get("checkout_path"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .or_else(|| fallback_pane_cwd().cloned());
            // The first agent wins ties, so a focused agent beats another of
            // the same status only when the server lists it first.
            let agent_title = snapshot
                .agents
                .iter()
                .filter(|agent| agent.workspace_id == id)
                .fold(None::<&SnapshotAgent>, |best, agent| match best {
                    Some(best)
                        if (status_priority(best.agent_status.as_deref()), best.focused)
                            >= (
                                status_priority(agent.agent_status.as_deref()),
                                agent.focused,
                            ) =>
                    {
                        Some(best)
                    }
                    _ => Some(agent),
                })
                .and_then(|agent| {
                    non_empty(agent.terminal_title_stripped.as_ref())
                        .or(non_empty(agent.terminal_title.as_ref()))
                        .cloned()
                });
            WorkspaceRow {
                workspace_id: workspace.workspace_id.clone(),
                number: workspace.number,
                label: workspace.label.clone(),
                focused: workspace.focused,
                agent_status: workspace.agent_status.clone(),
                agent_title,
                tab_count: workspace.tab_count,
                cwd,
                branch: non_empty(workspace.branch.as_ref()).cloned(),
                worktree: workspace.worktree.clone().filter(|value| !value.is_null()),
            }
        })
        .collect()
}

fn render_text(report: &Report) -> String {
    let rows = report
        .machines
        .iter()
        .flat_map(|machine| {
            machine.workspaces.iter().map(|workspace| {
                [
                    machine.label.clone(),
                    workspace.number.to_string(),
                    format!(
                        "{} {}",
                        if workspace.focused { "*" } else { " " },
                        workspace.label
                    ),
                    workspace.agent_status.clone().unwrap_or_else(|| "-".into()),
                    workspace.branch.clone().unwrap_or_else(|| "-".into()),
                    workspace.cwd.clone().unwrap_or_else(|| "-".into()),
                    workspace.agent_title.clone().unwrap_or_default(),
                ]
            })
        })
        .collect::<Vec<_>>();
    let mut widths = [0; 7];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(display_width(cell));
        }
    }
    for machine in &report.machines {
        widths[0] = widths[0].max(display_width(&machine.label));
    }

    let mut output = String::new();
    let mut rows = rows.into_iter();
    for machine in &report.machines {
        if machine.status != Status::Ok {
            let mut line = format!(
                "{}  {}",
                pad(&machine.label, widths[0]),
                machine.status.as_str()
            );
            if let Some(error) = &machine.error {
                line.push_str(": ");
                line.push_str(&error.escape_debug().to_string());
            }
            output.push_str(&line);
            output.push('\n');
            continue;
        }
        if machine.workspaces.is_empty() {
            output.push_str(&format!(
                "{}  no workspaces\n",
                pad(&machine.label, widths[0])
            ));
            continue;
        }
        for row in rows.by_ref().take(machine.workspaces.len()) {
            let last = row.len() - 1;
            let line = row
                .iter()
                .enumerate()
                .map(|(index, cell)| {
                    if index == last {
                        cell.clone()
                    } else if index == 1 {
                        format!("{cell:>width$}", width = widths[index])
                    } else {
                        pad(cell, widths[index])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ");
            output.push_str(line.trim_end());
            output.push('\n');
        }
    }
    if report
        .machines
        .iter()
        .all(|machine| machine.workspaces.is_empty())
        && report
            .machines
            .iter()
            .all(|machine| machine.status == Status::Ok)
    {
        output.push_str("No workspaces.\n");
    }
    output
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn pad(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn snapshot(protocol: u32) -> serde_json::Value {
        json!({"id": REQUEST_ID, "result": {"type": "session_snapshot", "snapshot": {
            "version": "test",
            "protocol": protocol,
            "focused_workspace_id": "wA",
            "workspaces": [
                {"workspace_id": "wA", "number": 1, "label": "devtools/main", "focused": true,
                 "pane_count": 2, "tab_count": 2, "active_tab_id": "wA:2", "agent_status": "working",
                 "branch": "main"},
                {"workspace_id": "wB", "number": 2, "label": "herdr", "focused": false,
                 "pane_count": 1, "tab_count": 1, "active_tab_id": "wB:1", "agent_status": "idle",
                 "worktree": {"repo_key": "/r/.git", "repo_name": "herdr", "repo_root": "/r",
                              "checkout_path": "/r/wt", "is_linked_worktree": true}}
            ],
            "tabs": [],
            "panes": [
                {"pane_id": "wA-1", "terminal_id": "t1", "workspace_id": "wA", "tab_id": "wA:1",
                 "focused": false, "cwd": "/home/u/first", "agent_status": "idle", "revision": 0},
                {"pane_id": "wA-2", "terminal_id": "t2", "workspace_id": "wA", "tab_id": "wA:2",
                 "focused": true, "cwd": "/home/u/dev", "foreground_cwd": "/home/u/dev/devtools/main",
                 "agent_status": "working", "revision": 0},
                {"pane_id": "wB-1", "terminal_id": "t3", "workspace_id": "wB", "tab_id": "wB:1",
                 "focused": false, "agent_status": "idle", "revision": 0}
            ],
            "layouts": [
                {"workspace_id": "wA", "tab_id": "wA:2", "zoomed": false,
                 "area": {"x": 0, "y": 0, "width": 80, "height": 24}, "focused_pane_id": "wA-2",
                 "panes": [], "splits": []}
            ],
            "agents": [
                {"terminal_id": "t1", "workspace_id": "wA", "tab_id": "wA:1", "pane_id": "wA-1",
                 "focused": false, "agent_status": "idle", "terminal_title": "idle title"},
                {"terminal_id": "t2", "workspace_id": "wA", "tab_id": "wA:2", "pane_id": "wA-2",
                 "focused": true, "agent_status": "working", "terminal_title": "⠋ Rework the switcher",
                 "terminal_title_stripped": "Rework the switcher"}
            ]
        }}})
    }

    #[test]
    fn args_accept_json_and_timeout_in_both_forms() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            Args {
                json: false,
                timeout: DEFAULT_TIMEOUT
            }
        );
        for input in [
            args(&["--json", "--timeout-ms", "250"]),
            args(&["--timeout-ms=250", "--json"]),
        ] {
            assert_eq!(
                parse_args(&input).unwrap(),
                Args {
                    json: true,
                    timeout: Duration::from_millis(250)
                },
                "{input:?}"
            );
        }
    }

    #[test]
    fn args_reject_bad_values_duplicates_and_extras() {
        for input in [
            args(&["--timeout-ms"]),
            args(&["--timeout-ms", "0"]),
            args(&["--timeout-ms", "-5"]),
            args(&["--timeout-ms", "soon"]),
            args(&["--json", "--json"]),
            args(&["--timeout-ms", "1", "--timeout-ms", "2"]),
            args(&["macmini"]),
            args(&["--all"]),
        ] {
            assert!(parse_args(&input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn snapshot_rows_shape_workspace_fields() {
        let rows = snapshot_rows(&snapshot(crate::protocol::PROTOCOL_VERSION), "restart").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            serde_json::to_value(&rows[0]).unwrap(),
            json!({
                "workspace_id": "wA", "number": 1, "label": "devtools/main", "focused": true,
                "agent_status": "working", "agent_title": "Rework the switcher", "tab_count": 2,
                "cwd": "/home/u/dev/devtools/main", "branch": "main", "worktree": null
            })
        );
        // No layout and no pane cwd: the worktree checkout is the workspace cwd.
        assert_eq!(rows[1].cwd.as_deref(), Some("/r/wt"));
        assert_eq!(rows[1].agent_title, None);
        assert_eq!(rows[1].branch, None);
        assert_eq!(rows[1].worktree.as_ref().unwrap()["checkout_path"], "/r/wt");
    }

    #[test]
    fn most_urgent_agent_titles_the_workspace() {
        let mut value = snapshot(crate::protocol::PROTOCOL_VERSION);
        value["result"]["snapshot"]["agents"][0]["agent_status"] = json!("blocked");
        let rows = snapshot_rows(&value, "restart").unwrap();
        assert_eq!(rows[0].agent_title.as_deref(), Some("idle title"));

        value["result"]["snapshot"]["agents"][0]["agent_status"] = json!("working");
        let rows = snapshot_rows(&value, "restart").unwrap();
        assert_eq!(
            rows[0].agent_title.as_deref(),
            Some("Rework the switcher"),
            "equal status prefers the focused agent"
        );
    }

    #[test]
    fn cwd_falls_back_to_pane_cwd_without_a_foreground_cwd() {
        let mut value = snapshot(crate::protocol::PROTOCOL_VERSION);
        value["result"]["snapshot"]["panes"][1]
            .as_object_mut()
            .unwrap()
            .remove("foreground_cwd");
        let rows = snapshot_rows(&value, "restart").unwrap();
        assert_eq!(rows[0].cwd.as_deref(), Some("/home/u/dev"));
    }

    #[test]
    fn protocol_mismatch_and_server_errors_become_error_status() {
        let failure =
            snapshot_rows(&snapshot(crate::protocol::PROTOCOL_VERSION + 1), "restart").unwrap_err();
        assert_eq!(failure.status, Status::Error);
        assert!(
            failure.message.as_deref().unwrap().contains("protocol"),
            "{failure:?}"
        );

        let failure = snapshot_rows(
            &json!({"id": REQUEST_ID, "error": {"code": "invalid_request", "message": "nope"}}),
            "restart",
        )
        .unwrap_err();
        assert_eq!(failure, Failure::new(Status::Error, "nope"));

        let failure = snapshot_rows(
            &json!({"id": REQUEST_ID, "result": {"type": "session_snapshot", "snapshot": {}}}),
            "restart",
        )
        .unwrap_err();
        assert_eq!(failure.status, Status::Error);
    }

    #[test]
    fn io_errors_map_to_statuses() {
        assert_eq!(
            classify_io_error(&io::Error::new(io::ErrorKind::TimedOut, "slow")).status,
            Status::Timeout
        );
        assert_eq!(
            classify_io_error(&io::Error::other("dev@mac: Permission denied (publickey).")).status,
            Status::AuthRequired
        );
        assert_eq!(
            classify_io_error(&io::Error::other("remote SSH connection failed")).status,
            Status::Error
        );
    }

    fn profile(label: &str, enabled: bool) -> Machine {
        let mut profile = SavedSshEndpoint::new(label, format!("{label}-ssh"), "default").unwrap();
        profile.enabled = enabled;
        Machine::Ssh(profile)
    }

    #[test]
    fn collect_keeps_catalog_order_and_bounds_hung_machines() {
        let machines = vec![
            Machine::Local,
            profile("fast", true),
            profile("off", false),
            profile("hung", true),
        ];
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancelled);
        let query: Arc<Query> = Arc::new(move |machine, _timeout, cancel| match machine {
            Machine::Local => Err(Failure::new(Status::Offline, "not running")),
            Machine::Ssh(profile) if profile.label == "hung" => {
                let flag = cancel.attempt().unwrap();
                while !flag.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                observed.store(true, Ordering::Release);
                Ok(Vec::new())
            }
            Machine::Ssh(_) => Ok(vec![WorkspaceRow {
                workspace_id: "w1".into(),
                number: 1,
                label: "one".into(),
                focused: true,
                agent_status: None,
                agent_title: None,
                tab_count: 1,
                cwd: None,
                branch: None,
                worktree: None,
            }]),
        });
        let started = Instant::now();
        let reports = collect(machines, Duration::from_millis(100), query);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            cancelled.load(Ordering::Acquire),
            "hung query was cancelled"
        );
        let summary = reports
            .iter()
            .map(|report| (report.label.as_str(), report.kind, report.status))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("local", "local", Status::Offline),
                ("fast", "ssh", Status::Ok),
                ("off", "ssh", Status::Disabled),
                ("hung", "ssh", Status::Timeout),
            ]
        );
        assert_eq!(reports[0].id, "local");
        assert_eq!(reports[1].workspaces.len(), 1);
        assert_eq!(reports[2].error, None);
        assert_eq!(reports[3].elapsed_ms, 100);
        assert!(reports[3].error.as_deref().unwrap().contains("100 ms"));
    }

    #[test]
    fn report_json_keeps_null_fields() {
        let report = Report {
            machines: vec![MachineReport {
                id: "local".into(),
                label: "local".into(),
                kind: "local",
                status: Status::AuthRequired,
                error: None,
                elapsed_ms: 3,
                workspaces: Vec::new(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({"machines": [{"id": "local", "label": "local", "kind": "local",
                "status": "auth_required", "error": null, "elapsed_ms": 3, "workspaces": []}]})
        );
    }

    #[test]
    fn text_output_aligns_rows_and_reports_unavailable_machines() {
        let rows = snapshot_rows(&snapshot(crate::protocol::PROTOCOL_VERSION), "restart").unwrap();
        let report = Report {
            machines: vec![
                MachineReport {
                    id: "local".into(),
                    label: "local".into(),
                    kind: "local",
                    status: Status::Ok,
                    error: None,
                    elapsed_ms: 1,
                    workspaces: rows,
                },
                MachineReport {
                    id: "0123456789abcdef0123456789abcdef".into(),
                    label: "macmini".into(),
                    kind: "ssh",
                    status: Status::Timeout,
                    error: Some("no response within 5000 ms".into()),
                    elapsed_ms: 5000,
                    workspaces: Vec::new(),
                },
            ],
        };
        assert_eq!(
            render_text(&report),
            "local    1  * devtools/main  working  main  /home/u/dev/devtools/main  Rework the switcher\n\
             local    2    herdr          idle     -     /r/wt\n\
             macmini  timeout: no response within 5000 ms\n"
        );
    }
}
