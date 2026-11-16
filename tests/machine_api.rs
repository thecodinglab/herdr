#![cfg(all(unix, not(target_os = "macos")))]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use base64::Engine as _;

const PROFILE_ID: &str = "0123456789abcdef0123456789abcdef";
const SSH: &str = r#"#!/bin/sh
printf 'ssh\n' >> "$TEST_ROOT/ssh-calls"
control=; previous=; operation=; configuration=
for arg do
    last=$arg
    printf '%s\n' "$arg" >> "$TEST_ROOT/ssh-args"
    if [ "$previous" = -S ]; then control=$(printf '%s' "$arg" | sed 's/%C/fixture/'); fi
    if [ "$previous" = -O ]; then operation=$arg; fi
    if [ "$arg" = -G ]; then configuration=yes; fi
    previous=$arg
done
if [ -n "$configuration" ]; then
    printf 'controlpath %s\n' "$control"
    exit 0
fi
if [ "$operation" = stop ]; then
    test -n "$TEST_CONTROL" && test -S "$TEST_CONTROL" || exit 255
    rm -- "$TEST_CONTROL"
    exit 0
fi
if [ "$last" = exit ]; then
    if [ "$control" = none ]; then exit 0; fi
    echo 'test reused transport failed' >&2
    exit 255
fi
printf '%s' "$control" > "$TEST_ROOT/control-path"
if [ -n "$TEST_CONTROL" ] && [ -S "$TEST_CONTROL" ]; then
    if [ "$TEST_MODE" = frozen ] && [ "$last" != '/bin/sh -s' ]; then
        exec sleep 30
    else
        echo 'test reused transport failed' >&2
        exit 255
    fi
fi
if [ "$TEST_MODE" = cold_then_broken ] && [ "$last" != '/bin/sh -s' ]; then
    case "$last" in *base64*) echo 'test new transport failed' >&2; exit 255;; esac
fi
case "$last" in
    '/bin/sh -c '*)
        case "$last" in
            *base64*) : ;;
            *) echo "script must use stdin, not a command argument: $last" >&2; exit 2 ;;
        esac
        if [ "$TEST_MODE" = offline ]; then echo 'test remote connection failed' >&2; exit 255; fi
        printf 'login banner\n'
        PATH="$TEST_ROOT/remote bin:/usr/bin:/bin" exec /bin/sh -c "$last" ;;
    '/bin/sh -s')
        script=$(cat)
        printf 'login banner\n'
        case "$script" in
            *'uname -s'*) printf 'herdr-remote-output-ready:1\n'; uname -s; uname -m ;;
            *) PATH="$TEST_ROOT/remote bin:/usr/bin:/bin" exec /bin/sh -c "$script" ;;
        esac ;;
    *) echo "unexpected command: $last" >&2; exit 2 ;;
esac
"#;

struct Harness {
    root: PathBuf,
    state: PathBuf,
    remote: UnixListener,
    local: UnixListener,
    protocol: u64,
}

impl Harness {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(format!(
            "/var/tmp/hma-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let app = if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        };
        let state = root.join("state").join(app).join("client");
        let session = root.join("config").join(app).join("sessions/fleet");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&session).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/ssh"), SSH).unwrap();
        fs::set_permissions(root.join("bin/ssh"), fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_herdr"), root.join("remote herdr")).unwrap();
        fs::create_dir_all(root.join("remote bin")).unwrap();
        let remote_wrapper = root.join("remote bin/herdr");
        fs::write(
            &remote_wrapper,
            r#"#!/bin/sh
if [ "$TEST_MODE" = old ]; then printf 'herdr-api-bridge-v1\n'; exit 2; fi
case "$*" in
    *'--check')
        if IFS= read -r request; then
            echo 'capability check consumed API stdin' >&2
            exit 2
        fi ;;
esac
exec "$TEST_REMOTE_HERDR" "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&remote_wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(state.join("endpoints.json"), serde_json::to_vec(&json!({
            "version": 1,
            "ssh": [{"id": PROFILE_ID, "label": "mac", "target": "fake-mac", "session": "fleet", "enabled": true}]
        })).unwrap()).unwrap();
        let remote = UnixListener::bind(session.join("herdr.sock")).unwrap();
        remote.set_nonblocking(true).unwrap();
        let local = UnixListener::bind(root.join("local.sock")).unwrap();
        local.set_nonblocking(true).unwrap();
        let status = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .args(["status", "client", "--json"])
            .output()
            .unwrap();
        let status: Value = serde_json::from_slice(&status.stdout).unwrap();
        Self {
            root,
            state,
            remote,
            local,
            protocol: status["protocol"].as_u64().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .args(args)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("TEST_ROOT", &self.root)
            .env("TEST_REMOTE_HERDR", self.root.join("remote herdr"))
            .env("HERDR_SOCKET_PATH", self.root.join("local.sock"))
            .env(
                "HERDR_CLIENT_SOCKET_PATH",
                self.root.join("never-client.sock"),
            )
            .env("HERDR_SESSION", "wrong-inherited-session")
            .env("HERDR_PANE_ID", "wrong-local-pane")
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_REMOTE_BINARY");
        command
    }

    fn serve(&self, result: Value, protocol: u64) -> std::thread::JoinHandle<Value> {
        let listener = self.remote.try_clone().unwrap();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "remote API request did not arrive"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let ping = request["method"] == "ping";
                if !ping && result.is_null() {
                    return request;
                }
                let response = if ping {
                    json!({"id": request["id"], "result": {"type": "pong", "version": "test", "protocol": protocol}})
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                    let mut response = result.clone();
                    response["id"] = request["id"].clone();
                    response
                };
                writeln!(stream, "{response}").unwrap();
                if !ping || protocol == 0 {
                    return request;
                }
            }
        })
    }

    fn warm_metadata(&self) {
        let server = self.serve(json!({}), 0);
        success(
            self.command(&["--machine", "mac", "status", "server", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(server.join().unwrap()["method"], "ping");
    }

    fn ssh_calls(&self) -> usize {
        fs::read_to_string(self.root.join("ssh-calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn managed_control(&self) -> (PathBuf, UnixListener) {
        let path = PathBuf::from(fs::read_to_string(self.root.join("control-path")).unwrap());
        let listener = UnixListener::bind(&path).unwrap();
        (path, listener)
    }

    fn assert_local_untouched(&self) {
        assert_eq!(
            self.local.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!self.root.join("never-client.sock").exists());
        assert!(!fs::read_dir(&self.root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("herdr-api-")));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn machine_api_routes_structured_payload_and_remote_errors_without_local_fallback() {
    let harness = Harness::new();
    let server = harness.serve(
        json!({"error":{"code":"test_remote_error","message":"remote rejected prompt"}}),
        harness.protocol,
    );
    let prompt = "quotes ' \" ; $(touch should-not-exist)\n--machine other";
    let output = harness
        .command(&["--machine", "mac", "agent", "prompt", "w4:p1", prompt])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(error["error"]["code"], "test_remote_error");
    let request = server.join().unwrap();
    assert_eq!(request["method"], "agent.prompt");
    assert_eq!(request["params"]["text"], prompt);
    let ssh_args = fs::read_to_string(harness.root.join("ssh-args")).unwrap();
    assert!(ssh_args.contains("StrictHostKeyChecking=yes"));
    assert!(ssh_args.contains("BatchMode=yes"));
    assert!(!ssh_args.contains("should-not-exist"));
    // The POSIX bridge script is base64-encoded so Windows OpenSSH cannot expand
    // it; decode the forwarding command it carries.
    let bridge = ssh_args
        .lines()
        .find(|line| line.contains("base64"))
        .expect("base64 bridge command");
    let encoded = bridge
        .split_whitespace()
        .nth(5)
        .expect("base64-encoded bridge script");
    let script = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64 bridge script"),
    )
    .expect("UTF-8 bridge script");
    assert!(script.contains("remote-api-bridge"), "{script}");
    assert!(script.contains("fleet"), "{script}");
    harness.assert_local_untouched();
}

#[test]
fn machine_api_profile_id_routes_large_list_responses() {
    let harness = Harness::new();
    let data = "remote data ".repeat(20_000);
    let server = harness.serve(
        json!({"result":{"type":"agent_list","agents":[],"test_data":data}}),
        harness.protocol,
    );
    let response = success(
        harness
            .command(&["--machine", PROFILE_ID, "agent", "list"])
            .output()
            .unwrap(),
    );
    assert_eq!(response["result"]["test_data"], data);
    assert_eq!(server.join().unwrap()["method"], "agent.list");
    assert_eq!(
        fs::read_to_string(harness.root.join("ssh-calls"))
            .unwrap()
            .lines()
            .count(),
        4,
        "cold command: platform, discovery, protocol ping, request"
    );
    harness.assert_local_untouched();
}

#[test]
fn machine_api_bootstrap_falls_back_from_an_old_path_binary() {
    let harness = Harness::new();
    fs::create_dir_all(harness.root.join(".local/bin")).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_herdr"),
        harness.root.join(".local/bin/herdr"),
    )
    .unwrap();
    let server = harness.serve(
        json!({"result":{"type":"agent_list","agents":[]}}),
        harness.protocol,
    );
    success(
        harness
            .command(&["--machine", "mac", "agent", "list"])
            .env("TEST_MODE", "old")
            .output()
            .unwrap(),
    );
    assert_eq!(server.join().unwrap()["method"], "agent.list");
    harness.assert_local_untouched();
}

#[test]
fn machine_api_reuses_discovery_across_commands_without_rewriting_profiles() {
    let harness = Harness::new();
    let catalog_before = fs::read(harness.state.join("endpoints.json")).unwrap();
    for expected_calls in [None, Some(2)] {
        let before = harness.ssh_calls();
        let server = harness.serve(
            json!({"result":{"type":"agent_list","agents":[]}}),
            harness.protocol,
        );
        success(
            harness
                .command(&["--machine", "mac", "agent", "list"])
                .output()
                .unwrap(),
        );
        assert_eq!(server.join().unwrap()["method"], "agent.list");
        if let Some(expected) = expected_calls {
            assert_eq!(
                harness.ssh_calls() - before,
                expected,
                "warm commands must skip discovery"
            );
        }
    }
    let before = harness.ssh_calls();
    let server = harness.serve(json!({}), 0);
    success(
        harness
            .command(&["--machine", "mac", "status", "server", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(server.join().unwrap()["method"], "ping");
    assert_eq!(
        harness.ssh_calls() - before,
        1,
        "warm status needs only one SSH"
    );
    assert_eq!(
        fs::read(harness.state.join("endpoints.json")).unwrap(),
        catalog_before
    );
    harness.assert_local_untouched();
}

#[test]
fn machine_api_recovers_a_stale_path_before_sending_a_mutation() {
    let harness = Harness::new();
    harness.warm_metadata();
    fs::remove_file(harness.root.join("remote bin/herdr")).unwrap();
    fs::create_dir_all(harness.root.join(".local/bin")).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_herdr"),
        harness.root.join(".local/bin/herdr"),
    )
    .unwrap();
    let before = harness.ssh_calls();
    let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
    success(
        harness
            .command(&["--machine", "mac", "pane", "close", "w4:p1"])
            .output()
            .unwrap(),
    );
    assert_eq!(server.join().unwrap()["method"], "pane.close");
    assert_eq!(
        harness.ssh_calls() - before,
        5,
        "one failed ping followed by fresh discovery and one command"
    );
    let before = harness.ssh_calls();
    harness.warm_metadata();
    assert_eq!(
        harness.ssh_calls() - before,
        1,
        "recovered path must be saved"
    );
    harness.assert_local_untouched();
}

#[test]
fn machine_api_never_replays_a_mutation_when_its_response_is_lost() {
    let harness = Harness::new();
    harness.warm_metadata();
    let before = harness.ssh_calls();
    let server = harness.serve(Value::Null, harness.protocol);
    let output = harness
        .command(&["--machine", "mac", "pane", "close", "w4:p1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(server.join().unwrap()["method"], "pane.close");
    assert_eq!(
        harness.ssh_calls() - before,
        2,
        "mutation must not trigger rediscovery or replay"
    );
    harness.assert_local_untouched();
}

#[test]
fn machine_api_transient_connection_failure_keeps_working_metadata() {
    let harness = Harness::new();
    harness.warm_metadata();
    let before = harness.ssh_calls();
    let output = harness
        .command(&["--machine", "mac", "agent", "list"])
        .env("TEST_MODE", "offline")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        harness.ssh_calls() - before,
        3,
        "a disappeared managed socket permits only one read-only retry"
    );
    let before = harness.ssh_calls();
    harness.warm_metadata();
    assert_eq!(
        harness.ssh_calls() - before,
        1,
        "transient failure must not discard metadata"
    );
}

#[test]
fn machine_api_recovers_a_ping_timeout_without_child_exit_before_one_mutation() {
    let harness = Harness::new();
    harness.warm_metadata();
    let metadata = harness
        .state
        .join("ssh-metadata")
        .join(format!("{PROFILE_ID}.json"));
    let before = fs::read(&metadata).unwrap();
    let (control, listener) = harness.managed_control();
    // The first bridge stays alive without forwarding Ping. Recovery may only
    // recreate that read-only connection; the mutation is sent afterwards.
    let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
    success(
        harness
            .command(&["--machine", "mac", "pane", "close", "w4:p1"])
            .env("TEST_MODE", "frozen")
            .env("TEST_CONTROL", &control)
            .output()
            .unwrap(),
    );
    assert_eq!(server.join().unwrap()["method"], "pane.close");
    assert!(!control.exists());
    assert_eq!(fs::read(metadata).unwrap(), before);
    drop(listener);
    harness.assert_local_untouched();
}

#[test]
fn machine_api_cold_discovery_recovery_consumes_the_command_retry() {
    for mode in ["cold", "cold_then_broken"] {
        let harness = Harness::new();
        harness.warm_metadata();
        let (control, listener) = harness.managed_control();
        let metadata = harness
            .state
            .join("ssh-metadata")
            .join(format!("{PROFILE_ID}.json"));
        fs::remove_file(metadata).unwrap();
        let before = harness.ssh_calls();
        let server = (mode == "cold")
            .then(|| harness.serve(json!({"result":{"type":"ok"}}), harness.protocol));
        let output = harness
            .command(&["--machine", "mac", "pane", "close", "w4:p1"])
            .env("TEST_MODE", mode)
            .env("TEST_CONTROL", &control)
            .output()
            .unwrap();
        if let Some(server) = server {
            success(output);
            assert_eq!(server.join().unwrap()["method"], "pane.close");
        } else {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("test new transport failed"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                harness.ssh_calls() - before,
                8,
                "cold recovery must leave no second Ping recovery"
            );
        }
        assert!(!control.exists());
        drop(listener);
        harness.assert_local_untouched();
    }
}

#[test]
fn machine_remove_deletes_only_that_profiles_metadata() {
    let harness = Harness::new();
    harness.warm_metadata();
    let directory = harness.state.join("ssh-metadata");
    let other = directory.join("fedcba9876543210fedcba9876543210.json");
    fs::write(&other, "other machine metadata").unwrap();
    let before = harness.ssh_calls();
    let output = harness
        .command(&["machine", "remove", PROFILE_ID])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!directory.join(format!("{PROFILE_ID}.json")).exists());
    assert_eq!(fs::read_to_string(other).unwrap(), "other machine metadata");
    assert_eq!(harness.ssh_calls(), before);
    harness.assert_local_untouched();
}

#[test]
fn machine_api_bad_or_unwritable_metadata_does_not_block_commands() {
    let harness = Harness::new();
    harness.warm_metadata();
    let path = harness
        .state
        .join("ssh-metadata")
        .join(format!("{PROFILE_ID}.json"));
    fs::write(&path, "broken metadata").unwrap();
    let before = harness.ssh_calls();
    harness.warm_metadata();
    assert_eq!(harness.ssh_calls() - before, 3);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    for _ in 0..2 {
        let before = harness.ssh_calls();
        harness.warm_metadata();
        assert_eq!(harness.ssh_calls() - before, 3);
    }
    harness.assert_local_untouched();
}

#[test]
fn machine_api_never_inherits_the_callers_pane() {
    let harness = Harness::new();
    let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
    success(
        harness
            .command(&["--machine=mac", "pane", "current"])
            .output()
            .unwrap(),
    );
    let request = server.join().unwrap();
    assert_eq!(request["method"], "pane.current");
    assert!(request["params"]["caller_pane_id"].is_null());
    harness.assert_local_untouched();
}

#[test]
fn machine_api_remote_paths_and_wait_parameters_reach_the_server() {
    for (args, method, expected) in [
        (
            vec![
                "worktree",
                "create",
                "--cwd",
                "~/Projects/herdr",
                "--branch",
                "review",
                "--path",
                "/srv/review",
            ],
            "worktree.create",
            json!({"cwd":"~/Projects/herdr", "branch":"review", "path":"/srv/review"}),
        ),
        (
            vec![
                "agent",
                "wait",
                "w4:p1",
                "--until",
                "idle",
                "--timeout",
                "2000",
            ],
            "agent.wait",
            json!({"target":"w4:p1", "until":["idle"], "timeout_ms":2000}),
        ),
    ] {
        let harness = Harness::new();
        let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
        let mut command = harness.command(&["--machine", "mac"]);
        success(command.args(args).output().unwrap());
        let request = server.join().unwrap();
        assert_eq!(request["method"], method);
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&request["params"][key], value);
        }
        harness.assert_local_untouched();
    }
}

#[test]
fn machine_api_status_reports_remote_identity_not_local_installation_state() {
    let harness = Harness::new();
    let server = harness.serve(json!({}), 0);
    let status = success(
        harness
            .command(&["--machine", "mac", "status", "server", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(status["session"], "fleet");
    assert_eq!(status["socket"], format!("machine:{PROFILE_ID}/fleet"));
    assert!(status["server_binary_stale"].is_null());
    assert_eq!(server.join().unwrap()["method"], "ping");
    assert_eq!(
        fs::read_to_string(harness.root.join("ssh-calls"))
            .unwrap()
            .lines()
            .count(),
        3,
        "cold status: platform, discovery, status"
    );
    harness.assert_local_untouched();
}

#[test]
fn machine_api_usage_errors_do_not_connect() {
    let harness = Harness::new();
    for args in [
        vec!["--machine", "missing", "agent", "list"],
        vec!["--machine", "mac", "config", "reset-keys"],
        vec![
            "--machine",
            "mac",
            "agent",
            "explain",
            "--file",
            "/any/file",
            "--agent",
            "pi",
        ],
        vec![
            "--machine",
            "mac",
            "pane",
            "split",
            "--current",
            "--direction",
            "right",
        ],
        vec!["--machine", "mac", "--session", "local", "agent", "list"],
    ] {
        assert_eq!(
            harness.command(&args).output().unwrap().status.code(),
            Some(2)
        );
    }
    assert!(!harness.root.join("ssh-args").exists());
    harness.assert_local_untouched();
}

#[test]
fn machine_api_rejects_old_bridges_and_disconnected_machines() {
    for (mode, message) in [
        ("old", "update Herdr"),
        ("offline", "test remote connection failed"),
    ] {
        let harness = Harness::new();
        let output = harness
            .command(&["--machine", "mac", "agent", "list"])
            .env("TEST_MODE", mode)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        harness.assert_local_untouched();
    }
}

#[test]
fn machine_api_server_stop_is_sent_only_to_the_selected_machine() {
    let harness = Harness::new();
    let server = harness.serve(json!({"result":{"type":"ok"}}), harness.protocol);
    let output = harness
        .command(&["--machine", "mac", "server", "stop"])
        .env("HERDR_SOCKET_PATH", harness.root.join("missing-local.sock"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        harness.root.join("ssh-args").exists(),
        "server stop bypassed machine routing"
    );
    assert_eq!(server.join().unwrap()["method"], "server.stop");
    harness.assert_local_untouched();
}

#[test]
fn machine_api_protocol_mismatch_never_sends_the_mutation() {
    let harness = Harness::new();
    harness.warm_metadata();
    let server = harness.serve(json!({}), 0);
    let output = harness
        .command(&["--machine", "mac", "pane", "close", "w4:p1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(server.join().unwrap()["method"], "ping");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("protocol_mismatch"), "{error}");
    assert!(error.contains("machine 'mac'"), "{error}");
    assert!(!error.contains("HERDR_SOCKET_PATH="), "{error}");
    harness.assert_local_untouched();
}

fn workspace_snapshot(protocol: u64, workspace_id: &str, label: &str) -> Value {
    json!({"result": {"type": "session_snapshot", "snapshot": {
        "version": "test",
        "protocol": protocol,
        "focused_workspace_id": workspace_id,
        "workspaces": [{
            "workspace_id": workspace_id, "number": 1, "label": label, "focused": true,
            "pane_count": 1, "tab_count": 1, "active_tab_id": format!("{workspace_id}:1"),
            "agent_status": "working", "branch": "main"
        }],
        "tabs": [],
        "panes": [{
            "pane_id": format!("{workspace_id}-1"), "terminal_id": "t1",
            "workspace_id": workspace_id, "tab_id": format!("{workspace_id}:1"), "focused": true,
            "cwd": "/srv", "foreground_cwd": format!("/srv/{label}"),
            "agent_status": "working", "revision": 0
        }],
        "layouts": [{
            "workspace_id": workspace_id, "tab_id": format!("{workspace_id}:1"), "zoomed": false,
            "area": {"x": 0, "y": 0, "width": 80, "height": 24},
            "focused_pane_id": format!("{workspace_id}-1"), "panes": [], "splits": []
        }],
        "agents": [{
            "terminal_id": "t1", "workspace_id": workspace_id, "tab_id": format!("{workspace_id}:1"),
            "pane_id": format!("{workspace_id}-1"), "focused": true, "agent_status": "working",
            "terminal_title": format!("* {label} task"), "terminal_title_stripped": format!("{label} task")
        }]
    }}})
}

impl Harness {
    fn serve_local(&self, mut response: Value) -> std::thread::JoinHandle<Value> {
        let listener = self.local.try_clone().unwrap();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "local request did not arrive");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            response["id"] = request["id"].clone();
            writeln!(stream, "{response}").unwrap();
            request
        })
    }

    fn add_disabled_machine(&self) {
        fs::write(self.state.join("endpoints.json"), serde_json::to_vec(&json!({
            "version": 1,
            "ssh": [
                {"id": PROFILE_ID, "label": "mac", "target": "fake-mac", "session": "fleet", "enabled": true},
                {"id": "fedcba9876543210fedcba9876543210", "label": "parked", "target": "fake-parked", "session": "default", "enabled": false}
            ]
        })).unwrap()).unwrap();
    }

    fn assert_no_bridge_sockets(&self) {
        assert!(!fs::read_dir(&self.root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("herdr-api-")));
    }
}

#[test]
fn machine_workspaces_lists_local_then_saved_machines_in_catalog_order() {
    let harness = Harness::new();
    harness.add_disabled_machine();
    let local = harness.serve_local(workspace_snapshot(harness.protocol, "wL", "local-work"));
    let remote = harness.serve(
        workspace_snapshot(harness.protocol, "wR", "remote-work"),
        harness.protocol,
    );
    let report = success(
        harness
            .command(&["machine", "workspaces", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(local.join().unwrap()["method"], "session.snapshot");
    assert_eq!(remote.join().unwrap()["method"], "session.snapshot");
    let machines = report["machines"].as_array().unwrap();
    let summary = machines
        .iter()
        .map(|machine| {
            (
                machine["id"].as_str().unwrap(),
                machine["label"].as_str().unwrap(),
                machine["kind"].as_str().unwrap(),
                machine["status"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            ("local", "local", "local", "ok"),
            (PROFILE_ID, "mac", "ssh", "ok"),
            (
                "fedcba9876543210fedcba9876543210",
                "parked",
                "ssh",
                "disabled"
            ),
        ]
    );
    assert!(machines[0]["elapsed_ms"].is_u64());
    assert!(machines[1]["error"].is_null());
    assert_eq!(machines[2]["workspaces"], json!([]));
    assert_eq!(
        machines[1]["workspaces"],
        json!([{
            "workspace_id": "wR", "number": 1, "label": "remote-work", "focused": true,
            "agent_status": "working", "agent_title": "remote-work task", "tab_count": 1,
            "cwd": "/srv/remote-work", "branch": "main", "worktree": null
        }])
    );
    assert_eq!(machines[0]["workspaces"][0]["label"], "local-work");
    assert!(
        !fs::read_to_string(harness.root.join("ssh-args"))
            .unwrap_or_default()
            .contains("fake-parked"),
        "disabled machines are never contacted"
    );
    harness.assert_no_bridge_sockets();
}

#[test]
fn machine_workspaces_reports_unavailable_machines_without_failing() {
    let harness = Harness::new();
    let output = harness
        .command(&["machine", "workspaces", "--json"])
        .env("HERDR_SOCKET_PATH", harness.root.join("missing-local.sock"))
        .env("TEST_MODE", "offline")
        .output()
        .unwrap();
    let report = success(output);
    assert_eq!(report["machines"][0]["status"], "offline");
    assert_eq!(report["machines"][1]["status"], "error");
    let error = report["machines"][1]["error"].as_str().unwrap();
    assert!(error.contains("test remote connection failed"), "{error}");

    let output = harness
        .command(&["machine", "workspaces"])
        .env("HERDR_SOCKET_PATH", harness.root.join("missing-local.sock"))
        .env("TEST_MODE", "offline")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("local  offline"), "{text}");
    assert!(text.contains("\nmac    error: "), "{text}");
    harness.assert_local_untouched();
}

#[test]
fn machine_workspaces_maps_a_remote_protocol_mismatch_to_error() {
    let harness = Harness::new();
    let remote = harness.serve(
        workspace_snapshot(harness.protocol + 1, "wR", "remote-work"),
        harness.protocol,
    );
    let report = success(
        harness
            .command(&["machine", "workspaces", "--json"])
            .env("HERDR_SOCKET_PATH", harness.root.join("missing-local.sock"))
            .output()
            .unwrap(),
    );
    assert_eq!(remote.join().unwrap()["method"], "session.snapshot");
    let machine = &report["machines"][1];
    assert_eq!(machine["status"], "error");
    assert_eq!(machine["workspaces"], json!([]));
    let error = machine["error"].as_str().unwrap();
    assert!(error.contains("older than server protocol"), "{error}");
}

#[test]
fn machine_workspaces_bounds_a_hung_machine_and_cleans_up_its_bridge() {
    let harness = Harness::new();
    harness.warm_metadata();
    let listener = harness.remote.try_clone().unwrap();
    let hung = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match listener.accept() {
                // Hold the connection open without answering.
                Ok((stream, _)) => {
                    std::thread::sleep(Duration::from_secs(4));
                    drop(stream);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "remote request did not arrive");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let started = Instant::now();
    let report = success(
        harness
            .command(&["machine", "workspaces", "--json", "--timeout-ms=700"])
            .env("HERDR_SOCKET_PATH", harness.root.join("missing-local.sock"))
            .output()
            .unwrap(),
    );
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(report["machines"][0]["status"], "offline");
    assert_eq!(report["machines"][1]["status"], "timeout");
    assert_eq!(report["machines"][1]["elapsed_ms"], 700);
    harness.assert_no_bridge_sockets();
    hung.join().unwrap();
}

#[test]
fn machine_workspaces_usage_errors_do_not_connect() {
    let harness = Harness::new();
    for args in [
        vec!["machine", "workspaces", "mac"],
        vec!["machine", "workspaces", "--timeout-ms"],
        vec!["machine", "workspaces", "--timeout-ms", "0"],
    ] {
        assert_eq!(
            harness.command(&args).output().unwrap().status.code(),
            Some(2),
            "{args:?}"
        );
    }
    assert!(!harness.root.join("ssh-args").exists());
    harness.assert_local_untouched();
}
