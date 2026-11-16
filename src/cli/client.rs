use clap::{ArgMatches, Command};

use crate::api::schema::common::ClientFocusProjection;
use crate::api::schema::{Method, Request};

use super::projection::{self, ParseError};

const NOT_DELIVERED: &str = "no attached herdr client accepted the focus request";

pub(crate) fn command() -> Command {
    Command::new("client")
        .about("Control attached Herdr clients")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(focus_command())
}

fn focus_command() -> Command {
    let mut arguments = ClientFocusProjection::arguments();
    arguments.push(json_flag());
    projection::command("focus", arguments)
        .about("Focus a workspace in the most recently active attached client")
}

/// `--json` only selects the output format, so it never reaches the request.
fn json_flag() -> clap::Arg {
    clap::Arg::new("json")
        .long("json")
        .action(clap::ArgAction::SetTrue)
        .help("Print the JSON response")
}

struct FocusCommand {
    request: Request,
    json: bool,
}

fn request(matches: &ArgMatches) -> Result<FocusCommand, ParseError> {
    match matches.subcommand() {
        Some(("focus", matches)) => Ok(FocusCommand {
            request: Request {
                id: "cli:client:focus".into(),
                method: Method::ClientFocus(ClientFocusProjection::from_matches(matches)?),
            },
            json: matches.get_flag("json"),
        }),
        _ => Err(ParseError::Usage("expected client focus".into())),
    }
}

pub(super) fn run_client_command(args: &[String]) -> std::io::Result<i32> {
    match projection::parse(command(), args, request) {
        Ok(command) => {
            let response = super::send_request(&command.request)?;
            Ok(print_focus_response(&response, command.json))
        }
        Err(ParseError::Clap(error)) => super::print_clap_error(&error),
        Err(ParseError::Usage(message)) => {
            eprintln!("{message}");
            Ok(2)
        }
        Err(ParseError::Io(error)) => Err(error),
    }
}

fn print_focus_response(response: &serde_json::Value, json: bool) -> i32 {
    if response.get("error").is_some() {
        eprintln!("{response}");
        return 1;
    }
    if json {
        println!("{response}");
    }
    let delivered = focus_delivered(response);
    if delivered == 0 {
        eprintln!("{NOT_DELIVERED}");
        return 1;
    }
    0
}

fn focus_delivered(response: &serde_json::Value) -> u64 {
    response["result"]["delivered"].as_u64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<FocusCommand, ParseError> {
        let args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
        projection::parse(command(), &args, request)
    }

    #[test]
    fn focus_binding_builds_client_focus_requests() {
        for (args, params, json) in [
            (
                vec!["focus", "w1"],
                serde_json::json!({"workspace_id": "w1", "endpoint": null, "all_clients": false}),
                false,
            ),
            (
                vec![
                    "focus",
                    "w2",
                    "--endpoint",
                    "0123456789abcdef0123456789abcdef",
                    "--all-clients",
                    "--json",
                ],
                serde_json::json!({
                    "workspace_id": "w2",
                    "endpoint": "0123456789abcdef0123456789abcdef",
                    "all_clients": true,
                }),
                true,
            ),
            (
                vec!["focus", "--endpoint", "local", "w3"],
                serde_json::json!({"workspace_id": "w3", "endpoint": "local", "all_clients": false}),
                false,
            ),
        ] {
            let command = parse(&args).expect("valid client focus command");
            assert_eq!(command.json, json);
            assert_eq!(
                serde_json::to_value(command.request).unwrap(),
                serde_json::json!({
                    "id": "cli:client:focus",
                    "method": "client.focus",
                    "params": params,
                })
            );
        }
    }

    #[test]
    fn focus_binding_rejects_bad_arity_before_transport() {
        command().debug_assert();
        for args in [
            vec!["focus"],
            vec!["focus", "w1", "w2"],
            vec!["focus", "w1", "--endpoint"],
            vec!["focus", "w1", "--endpoint", "a", "--endpoint", "b"],
        ] {
            assert!(
                matches!(parse(&args), Err(ParseError::Clap(error)) if error.exit_code() == 2),
                "{args:?}"
            );
        }
        assert!(matches!(
            parse(&["focus", "--help"]),
            Err(ParseError::Clap(error)) if error.exit_code() == 0
        ));
    }

    #[test]
    fn focus_exit_code_requires_a_delivery() {
        let delivered = |count: u64| {
            serde_json::json!({
                "id": "cli:client:focus",
                "result": {"type": "client_focus", "delivered": count},
            })
        };
        assert_eq!(print_focus_response(&delivered(1), false), 0);
        assert_eq!(print_focus_response(&delivered(2), true), 0);
        assert_eq!(print_focus_response(&delivered(0), false), 1);
        assert_eq!(
            print_focus_response(
                &serde_json::json!({
                    "id": "cli:client:focus",
                    "error": {"code": "workspace_not_found", "message": "workspace w9 not found"},
                }),
                false,
            ),
            1
        );
    }
}
