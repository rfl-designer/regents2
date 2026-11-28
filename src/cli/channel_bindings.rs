//! `regents channel` commands (Regents channels, ADR-0004). In the herdr
//! binary, `channel` stays the update channel command.

use std::path::Path;

use clap::Command;

use crate::api::schema::channels::{
    ChannelCreateProjection, ChannelOpenProjection, ChannelPostProjection, ChannelReadProjection,
    ChannelRenameProjection,
};
use crate::api::schema::{Method, Request};

use super::projection::{self, Input, Leaf, ParseError};

pub(crate) fn channel_name() -> Input<String> {
    Input::positional("channel", 0, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name("CHANNEL")
}

pub(crate) fn new_channel_name() -> Input<String> {
    Input::positional("new_name", 1, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name("NEW_NAME")
}

/// The message text: every word after the channel name, joined by spaces.
pub(crate) fn message_body() -> Input<String> {
    Input::rest("body", "MESSAGE", 1, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .verbatim()
}

pub(crate) fn reply_to() -> Input<Option<u64>> {
    Input::value("reply-to", "MESSAGE_ID", |slot, value| {
        *slot = Some(super::parse_u64_flag("--reply-to", value)?);
        Ok(())
    })
    .help("Reply to a message of the channel")
}

pub(crate) fn before() -> Input<Option<u64>> {
    Input::value("before", "MESSAGE_ID", |slot, value| {
        *slot = Some(super::parse_u64_flag("--before", value)?);
        Ok(())
    })
    .help("Show only messages older than this message")
}

pub(crate) fn limit() -> Input<Option<u32>> {
    Input::value("limit", "N", |slot, value| {
        *slot = Some(super::parse_u32_flag("--limit", value)?);
        Ok(())
    })
    .help("Show at most N of the most recent messages")
}

const LEAVES: [Leaf; 5] = [
    Leaf {
        name: "create",
        about: "Create a channel",
        id: "cli:channel:create",
        arguments: ChannelCreateProjection::arguments,
        method: |matches| ChannelCreateProjection::from_matches(matches).map(Method::ChannelCreate),
    },
    Leaf {
        name: "rename",
        about: "Rename a channel; its history stays",
        id: "cli:channel:rename",
        arguments: ChannelRenameProjection::arguments,
        method: |matches| ChannelRenameProjection::from_matches(matches).map(Method::ChannelRename),
    },
    Leaf {
        name: "post",
        about: "Post a message to a channel",
        id: "cli:channel:post",
        arguments: ChannelPostProjection::arguments,
        method: |matches| ChannelPostProjection::from_matches(matches).map(Method::ChannelPost),
    },
    Leaf {
        name: "read",
        about: "Show the messages of a channel",
        id: "cli:channel:read",
        arguments: ChannelReadProjection::arguments,
        method: |matches| ChannelReadProjection::from_matches(matches).map(Method::ChannelRead),
    },
    Leaf {
        name: "open",
        about: "Open a channel as a chat pane, or focus its chat pane",
        id: "cli:channel:open",
        arguments: ChannelOpenProjection::arguments,
        method: |matches| ChannelOpenProjection::from_matches(matches).map(Method::ChannelOpen),
    },
];

/// `channel view`: the chat view that a chat pane runs. It is a terminal
/// program, not one API request, so `cli.rs` runs it before the leaves.
pub(crate) use crate::regents::chat_panes::VIEW_COMMAND;

fn view_command() -> Command {
    Command::new(VIEW_COMMAND)
        .about("Show a channel as a chat in this terminal")
        .arg(
            clap::Arg::new("channel")
                .value_name("CHANNEL")
                .required(true),
        )
        .arg(
            clap::Arg::new("workspace")
                .long("workspace")
                .value_name("WORKSPACE_ID")
                .help("Workspace of the channel: its public ID or its stable ID"),
        )
}

/// Runs `channel view <CHANNEL> [--workspace ID]`; `args` follow `view`.
pub(crate) fn run_view(args: &[String]) -> std::io::Result<i32> {
    let matches = match view_command()
        .bin_name(format!(
            "{} channel {VIEW_COMMAND}",
            crate::build_info::product_name()
        ))
        .no_binary_name(true)
        .try_get_matches_from(args)
    {
        Ok(matches) => matches,
        Err(error) => return super::print_clap_error(&error),
    };
    let (channel, workspace_id) = view_arguments(&matches);
    crate::regents::chat::run(channel, workspace_id)
}

fn view_arguments(matches: &clap::ArgMatches) -> (String, Option<String>) {
    (
        matches
            .get_one::<String>("channel")
            .cloned()
            .unwrap_or_default(),
        matches.get_one::<String>("workspace").cloned(),
    )
}

pub(crate) fn command() -> Command {
    Command::new("channel")
        .about("Create, rename, post to and read channels over the socket API")
        .subcommand_required(true)
        .subcommands(LEAVES.iter().map(Leaf::command))
        .subcommand(view_command())
}

pub(crate) fn request(args: &[String]) -> Result<Request, ParseError> {
    let cwd = std::env::current_dir().map_err(ParseError::Io)?;
    let mut request = request_in(args, &cwd)?;
    if let Method::ChannelPost(params) = &mut request.method {
        params.origin_thread_id = env_value(crate::regents::THREAD_ID_ENV_VAR);
        params.thread_token = env_value(crate::regents::THREAD_TOKEN_ENV_VAR);
    }
    Ok(request)
}

/// The thread of a command that a thread CLI runs: the server sets
/// `REGENTS_THREAD_ID` and `REGENTS_THREAD_TOKEN` in the environment of each
/// thread.
pub(super) fn env_value(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// Builds the request of a command run in `cwd`. Without `--workspace`, the
/// workspace is the folder that holds `cwd`, so a command that an agent runs
/// in its workspace posts there (ADR-0003: the folder is the identity). The
/// server uses the active workspace when `cwd` is in no workspace folder.
fn request_in(args: &[String], cwd: &Path) -> Result<Request, ParseError> {
    let mut request = projection::group_request(command(), &LEAVES, args)?;
    let workspace_id = match &mut request.method {
        Method::ChannelPost(params) => &mut params.workspace_id,
        Method::ChannelRead(params) => &mut params.workspace_id,
        Method::ChannelOpen(params) => &mut params.workspace_id,
        Method::ChannelCreate(params) => &mut params.workspace_id,
        Method::ChannelRename(params) => &mut params.workspace_id,
        _ => return Ok(request),
    };
    if workspace_id.is_none() {
        *workspace_id = crate::regents::folder::folder_workspace_id(cwd);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// A directory outside any workspace folder.
    fn outside() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    fn parse(args: &[&str]) -> serde_json::Value {
        let request =
            request_in(&owned(args), &outside()).unwrap_or_else(|err| panic!("{args:?}: {err:?}"));
        serde_json::to_value(request).unwrap()
    }

    fn fails(args: &[&str]) -> bool {
        request_in(&owned(args), &outside()).is_err()
    }

    #[test]
    fn without_workspace_the_folder_of_the_working_directory_is_used() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("regents-cwd-{}-{stamp}", std::process::id()));
        let folder = base.join("projeto");
        let id = crate::regents::folder::open_or_init(&folder)
            .unwrap()
            .manifest
            .id;
        let nested = folder.join("src").join("modulo");
        std::fs::create_dir_all(&nested).unwrap();

        for args in [&["post", "geral", "oi"][..], &["read", "geral"]] {
            let request = serde_json::to_value(request_in(&owned(args), &nested).unwrap()).unwrap();
            assert_eq!(request["params"]["workspace_id"], id.as_str(), "{args:?}");
        }
        let explicit =
            request_in(&owned(&["read", "--workspace", "w9", "geral"]), &nested).unwrap();
        assert_eq!(
            serde_json::to_value(explicit).unwrap()["params"]["workspace_id"],
            "w9"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn post_sends_the_channel_and_the_words_of_the_message() {
        assert_eq!(
            parse(&["post", "geral", "oi"]),
            serde_json::json!({
                "id": "cli:channel:post", "method": "channel.post",
                "params": {"channel": "geral", "body": "oi"}
            })
        );
        assert_eq!(
            parse(&["post", "geral", "oi", "--tudo", "bem?"])["params"]["body"],
            "oi --tudo bem?"
        );
        assert_eq!(
            parse(&[
                "post",
                "--workspace",
                "w2",
                "--reply-to",
                "3",
                "geral",
                "oi"
            ])["params"],
            serde_json::json!({"workspace_id": "w2", "channel": "geral", "body": "oi", "reply_to": 3})
        );
    }

    #[test]
    fn read_sends_the_channel_and_the_optional_range() {
        assert_eq!(
            parse(&["read", "geral"]),
            serde_json::json!({
                "id": "cli:channel:read", "method": "channel.read",
                "params": {"channel": "geral"}
            })
        );
        assert_eq!(
            parse(&[
                "read",
                "--limit",
                "5",
                "--before",
                "9",
                "--workspace",
                "w1",
                "geral"
            ])["params"],
            serde_json::json!({"workspace_id": "w1", "channel": "geral", "limit": 5, "before": 9})
        );
    }

    #[test]
    fn create_and_rename_send_the_channel_names() {
        assert_eq!(
            parse(&["create", "deploy"]),
            serde_json::json!({
                "id": "cli:channel:create", "method": "channel.create",
                "params": {"channel": "deploy"}
            })
        );
        assert_eq!(
            parse(&["rename", "--workspace", "w1", "deploy", "release"]),
            serde_json::json!({
                "id": "cli:channel:rename", "method": "channel.rename",
                "params": {"workspace_id": "w1", "channel": "deploy", "new_name": "release"}
            })
        );
    }

    #[test]
    fn usage_and_errors_name_the_running_binary() {
        crate::build_info::as_regents_for_test(|| {
            let commands = projection::group_commands(&command());
            assert!(
                commands.starts_with("regents channel commands:"),
                "{commands}"
            );
            assert!(
                commands.contains("regents channel post <CHANNEL> <MESSAGE>..."),
                "{commands}"
            );
            let Err(ParseError::Clap(error)) = request_in(&owned(&["post"]), &outside()) else {
                panic!("a post without a channel must fail in clap");
            };
            let rendered = error.render().to_string();
            assert!(
                rendered.contains("Usage: regents channel post"),
                "{rendered}"
            );

            let mut help = Vec::new();
            assert!(super::super::spec::write_requested_help_for_test(
                &["regents", "channel", "read", "--help"],
                &mut help
            ));
            let help = String::from_utf8(help).unwrap();
            assert!(help.contains("Usage: regents channel read"), "{help}");
        });
    }

    #[test]
    fn invalid_grammar_fails_before_transport() {
        for args in [
            &[][..],
            &["unknown"],
            &["post"],
            &["post", "geral"],
            &["post", "--reply-to", "x", "geral", "oi"],
            &["read"],
            &["read", "--limit", "-1", "geral"],
            &["read", "geral", "extra"],
            &["create"],
            &["create", "a", "b"],
            &["rename", "deploy"],
            &["rename", "a", "b", "c"],
        ] {
            assert!(fails(args), "{args:?}");
        }
    }

    #[test]
    fn view_takes_a_channel_and_an_optional_workspace() {
        let parse = |args: &[&str]| {
            view_command()
                .no_binary_name(true)
                .try_get_matches_from(owned(args))
                .map(|matches| view_arguments(&matches))
        };

        assert_eq!(parse(&["geral"]).unwrap(), ("geral".into(), None));
        assert_eq!(
            parse(&["geral", "--workspace", "abc"]).unwrap(),
            ("geral".into(), Some("abc".into()))
        );
        assert!(parse(&[]).is_err());
        assert!(parse(&["geral", "--workspace"]).is_err());
        assert!(parse(&["geral", "outro"]).is_err());
    }

    #[test]
    fn view_parses_the_command_line_of_a_chat_pane() {
        let view = crate::regents::channel::ChannelView {
            folder_id: "f1".into(),
            channel: "geral".into(),
        };
        let argv = crate::regents::chat_panes::view_argv(&view).unwrap();

        assert_eq!(argv[1..3], ["channel", VIEW_COMMAND]);
        let matches = view_command()
            .no_binary_name(true)
            .try_get_matches_from(&argv[3..])
            .unwrap();
        assert_eq!(
            view_arguments(&matches),
            ("geral".into(), Some("f1".into()))
        );
    }
}
