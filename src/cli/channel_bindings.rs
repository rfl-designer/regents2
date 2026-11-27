//! `regents channel` commands (Regents channels, ADR-0004). In the herdr
//! binary, `channel` stays the update channel command.

use std::path::Path;

use clap::Command;

use crate::api::schema::channels::{ChannelPostProjection, ChannelReadProjection};
use crate::api::schema::{Method, Request};

use super::projection::{self, Input, Leaf, ParseError};

pub(crate) fn channel_name() -> Input<String> {
    Input::positional("channel", 0, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name("CHANNEL")
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

const LEAVES: [Leaf; 2] = [
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
];

pub(crate) fn command() -> Command {
    Command::new("channel")
        .about("Post and read channel messages over the socket API")
        .subcommand_required(true)
        .subcommands(LEAVES.iter().map(Leaf::command))
}

pub(crate) fn request(args: &[String]) -> Result<Request, ParseError> {
    let cwd = std::env::current_dir().map_err(ParseError::Io)?;
    request_in(args, &cwd)
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
        _ => return Ok(request),
    };
    if workspace_id.is_none() {
        *workspace_id = folder_workspace_id(cwd);
    }
    Ok(request)
}

/// Stable ID of the workspace folder that holds `dir`: the nearest ancestor
/// with `.regents/workspace.toml`. `None` when there is none or it cannot be
/// read.
fn folder_workspace_id(dir: &Path) -> Option<String> {
    let root = dir.ancestors().find(|ancestor| {
        ancestor
            .join(crate::regents::folder::STATE_DIR)
            .join(crate::regents::folder::MANIFEST_FILE)
            .is_file()
    })?;
    crate::regents::folder::read_manifest(root)
        .ok()
        .map(|manifest| manifest.id)
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
        ] {
            assert!(fails(args), "{args:?}");
        }
    }
}
