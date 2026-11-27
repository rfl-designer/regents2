use std::collections::HashMap;

use clap::{Arg, ArgMatches, Command};

use crate::api::schema::workspaces::{
    WorkspaceAddProjection, WorkspaceCloseProjection, WorkspaceCreateProjection,
    WorkspaceLinkProjection, WorkspaceMetadataProjection, WorkspaceRelinkProjection,
    WorkspaceRemoveProjection, WorkspaceRenameProjection,
};
use crate::api::schema::{EmptyParams, Method, Request, WorkspaceTarget};

use super::projection::{cli_projection, Field, Input, ParseError};

pub(crate) use super::projection::{boolean, optional_text};

pub(crate) fn workspace_id() -> Input<String> {
    Input::positional("workspace_id", 0, |slot, value| {
        *slot = super::normalize_workspace_id(value);
        Ok(())
    })
}

/// Stable workspace ID from `workspace.toml` (Regents), as the first
/// positional argument.
pub(crate) fn stable_workspace_id() -> Input<String> {
    Input::positional("id", 0, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name("ID")
}

/// Folder path argument. A local relative path becomes absolute here,
/// because the server resolves paths from its own cwd.
pub(crate) fn folder_path() -> Input<String> {
    folder_path_at(0)
}

/// Folder path argument at positional `index`.
pub(crate) fn folder_path_at(index: usize) -> Input<String> {
    Input::positional("path", index, |slot, value| {
        *slot = super::worktree::normalize_path_arg(value)?;
        Ok(())
    })
    .path()
}

pub(crate) fn label() -> Input<String> {
    Input::rest("label", "LABEL", 1, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .verbatim()
}

pub(crate) fn environment() -> Input<HashMap<String, String>> {
    Input::<HashMap<String, String>>::value("env", "KEY=VALUE", |slot, value| {
        let (key, value) = super::parse_env_assignment(value)?;
        slot.insert(key, value);
        Ok(())
    })
    .help("Set an environment variable for the launched process")
}

pub(crate) fn source() -> Input<String> {
    Input::value("source", "ID", |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .required()
}

pub(crate) fn validate_source(source: &str) -> Result<(), ParseError> {
    if source.trim().is_empty() {
        return Err(ParseError::Usage("missing required --source".into()));
    }
    Ok(())
}

pub(crate) fn token() -> Input<HashMap<String, Option<String>>> {
    Input::<HashMap<String, Option<String>>>::value("token", "NAME=VALUE", |slot, value| {
        let (key, value) = super::parse_token_assignment(value)?;
        slot.insert(key, value);
        Ok(())
    })
}

pub(crate) fn clear_token() -> Input<HashMap<String, Option<String>>> {
    Input::<HashMap<String, Option<String>>>::value("clear-token", "NAME", |slot, value| {
        slot.insert(value.to_owned(), None);
        Ok(())
    })
}

pub(crate) fn validate_tokens(tokens: &HashMap<String, Option<String>>) -> Result<(), ParseError> {
    if tokens.is_empty() {
        return Err(ParseError::Usage("missing token to set or clear".into()));
    }
    Ok(())
}

pub(crate) fn seq() -> Input<Option<u64>> {
    Input::value("seq", "N", |slot, value| {
        *slot = Some(super::parse_u64_flag("--seq", value)?);
        Ok(())
    })
}

pub(crate) fn ttl_ms() -> Input<Option<u64>> {
    Input::value("ttl-ms", "N", |slot, value| {
        *slot = Some(super::parse_u64_flag("--ttl-ms", value)?);
        Ok(())
    })
}

cli_projection! {
    pub(crate) WorkspaceTargetProjection for WorkspaceTarget {
        workspace_id => Field::new(String::new()).input(workspace_id()),
    }
}

cli_projection! {
    pub(crate) WorkspaceListProjection for EmptyParams {}
}

pub(crate) struct Leaf {
    pub(crate) name: &'static str,
    about: &'static str,
    id: &'static str,
    pub(crate) quiet: bool,
    arguments: fn() -> Vec<Arg>,
    method: fn(&ArgMatches) -> Result<Method, ParseError>,
}

impl Leaf {
    pub(crate) fn request(&self, args: &[String]) -> Result<Request, ParseError> {
        let method = super::projection::parse(
            self.command().bin_name(format!(
                "{} workspace {}",
                crate::build_info::product_name(),
                self.name
            )),
            args,
            self.method,
        )?;
        Ok(Request {
            id: self.id.into(),
            method,
        })
    }

    fn command(&self) -> Command {
        super::projection::command(self.name, (self.arguments)()).about(self.about)
    }
}

pub(crate) const LEAVES: [Leaf; 7] = [
    Leaf {
        name: "list",
        about: "List workspaces",
        id: "cli:workspace:list",
        quiet: false,
        arguments: WorkspaceListProjection::arguments,
        method: |matches| WorkspaceListProjection::from_matches(matches).map(Method::WorkspaceList),
    },
    Leaf {
        name: "create",
        about: "Create a workspace",
        id: "cli:workspace:create",
        quiet: false,
        arguments: WorkspaceCreateProjection::arguments,
        method: |matches| {
            WorkspaceCreateProjection::from_matches(matches).map(Method::WorkspaceCreate)
        },
    },
    Leaf {
        name: "get",
        about: "Show a workspace",
        id: "cli:workspace:get",
        quiet: false,
        arguments: WorkspaceTargetProjection::arguments,
        method: |matches| {
            WorkspaceTargetProjection::from_matches(matches).map(Method::WorkspaceGet)
        },
    },
    Leaf {
        name: "focus",
        about: "Focus a workspace",
        id: "cli:workspace:focus",
        quiet: false,
        arguments: WorkspaceTargetProjection::arguments,
        method: |matches| {
            WorkspaceTargetProjection::from_matches(matches).map(Method::WorkspaceFocus)
        },
    },
    Leaf {
        name: "rename",
        about: "Rename a workspace",
        id: "cli:workspace:rename",
        quiet: false,
        arguments: WorkspaceRenameProjection::arguments,
        method: |matches| {
            WorkspaceRenameProjection::from_matches(matches).map(Method::WorkspaceRename)
        },
    },
    Leaf {
        name: "report-metadata",
        about: "Report display-only workspace metadata",
        id: "cli:request",
        quiet: true,
        arguments: WorkspaceMetadataProjection::arguments,
        method: |matches| {
            WorkspaceMetadataProjection::from_matches(matches).map(Method::WorkspaceReportMetadata)
        },
    },
    Leaf {
        name: "close",
        about: "Close a workspace",
        id: "cli:workspace:close",
        quiet: false,
        arguments: WorkspaceCloseProjection::arguments,
        method: |matches| {
            WorkspaceCloseProjection::from_matches(matches).map(Method::WorkspaceClose)
        },
    },
];

/// Regents commands for folder workspaces. `list` shows the registered
/// folders instead of the open layout containers.
const REGENTS_LEAVES: [Leaf; 5] = [
    Leaf {
        name: "add",
        about: "Register a folder as a workspace and open it",
        id: "cli:workspace:add",
        quiet: false,
        arguments: WorkspaceAddProjection::arguments,
        method: |matches| WorkspaceAddProjection::from_matches(matches).map(Method::WorkspaceAdd),
    },
    Leaf {
        name: "list",
        about: "List registered workspaces",
        id: "cli:workspace:list",
        quiet: false,
        arguments: WorkspaceListProjection::arguments,
        method: |matches| {
            WorkspaceListProjection::from_matches(matches).map(Method::WorkspaceRegistryList)
        },
    },
    Leaf {
        name: "link",
        about: "Bind a legacy workspace to a folder",
        id: "cli:workspace:link",
        quiet: false,
        arguments: WorkspaceLinkProjection::arguments,
        method: |matches| WorkspaceLinkProjection::from_matches(matches).map(Method::WorkspaceLink),
    },
    Leaf {
        name: "relink",
        about: "Bind a disconnected workspace to its new folder (same ID)",
        id: "cli:workspace:relink",
        quiet: false,
        arguments: WorkspaceRelinkProjection::arguments,
        method: |matches| {
            WorkspaceRelinkProjection::from_matches(matches).map(Method::WorkspaceRelink)
        },
    },
    Leaf {
        name: "remove",
        about: "Remove a workspace from the registry; the folder stays on disk",
        id: "cli:workspace:remove",
        quiet: false,
        arguments: WorkspaceRemoveProjection::arguments,
        method: |matches| {
            WorkspaceRemoveProjection::from_matches(matches).map(Method::WorkspaceRemove)
        },
    },
];

/// Workspace commands of the product. Regents commands replace herdr commands
/// with the same name; the herdr binary keeps its commands unchanged.
fn leaves_for(regents: bool) -> Vec<&'static Leaf> {
    if !regents {
        return LEAVES.iter().collect();
    }
    REGENTS_LEAVES
        .iter()
        .chain(LEAVES.iter().filter(|leaf| {
            REGENTS_LEAVES
                .iter()
                .all(|regents| regents.name != leaf.name)
        }))
        .collect()
}

pub(crate) fn leaves() -> Vec<&'static Leaf> {
    leaves_for(crate::build_info::is_regents())
}

pub(crate) fn command() -> Command {
    Command::new("workspace")
        .about("Manage workspaces over the socket API")
        .subcommands(leaves().into_iter().map(Leaf::command))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: &str, args: &[&str]) -> Result<Request, ParseError> {
        LEAVES
            .iter()
            .find(|leaf| leaf.name == command)
            .unwrap()
            .request(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn workspace_projection_preserves_defaults_and_ordered_maps() {
        let request = request(
            "create",
            &[
                "--cwd",
                "first",
                "--cwd",
                "--literal",
                "--focus",
                "--no-focus",
                "--env",
                "K=first",
                "--env",
                "K=",
                "--env",
                "V=a=b",
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:create", "method": "workspace.create",
                "params": { "cwd": "--literal", "focus": false, "env": {"K": "", "V": "a=b"} }
            })
        );
        assert_eq!(
            serde_json::to_value(super::tests::request("create", &[]).unwrap()).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:create", "method": "workspace.create", "params": {"focus": false}
            })
        );
    }

    #[test]
    fn workspace_projection_preserves_joined_labels_and_hidden_group() {
        assert_eq!(
            serde_json::to_value(request("rename", &["--id", "a", "--flag", "b"]).unwrap())
                .unwrap(),
            serde_json::json!({
                "id": "cli:workspace:rename", "method": "workspace.rename",
                "params": {"workspace_id": "--id", "label": "a --flag b"}
            })
        );
        assert_eq!(
            serde_json::to_value(request("close", &["id", "--group"]).unwrap()).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:close", "method": "workspace.close",
                "params": {"workspace_id": "id", "close_group": true}
            })
        );
        let close = command().find_subcommand("close").unwrap().clone();
        assert!(close
            .get_arguments()
            .any(|arg| arg.get_long() == Some("group") && arg.is_hide_set()));
        command().debug_assert();
    }

    #[test]
    fn workspace_projection_preserves_metadata_patches_and_error_channels() {
        assert_eq!(
            serde_json::to_value(
                request(
                    "report-metadata",
                    &[
                        "id",
                        "--source",
                        "  user:jj  ",
                        "--token",
                        "K=a=b",
                        "--clear-token",
                        "K",
                        "--token",
                        "E=",
                        "--seq",
                        "0",
                        "--ttl-ms",
                        "0",
                    ]
                )
                .unwrap()
            )
            .unwrap(),
            serde_json::json!({
                "id": "cli:request", "method": "workspace.report_metadata",
                "params": {"workspace_id": "id", "source": "  user:jj  ", "tokens": {"K": null, "E": ""}, "seq": 0, "ttl_ms": 0}
            })
        );
        assert!(
            matches!(request("report-metadata", &["id", "--source", "valid", "--seq", "bad"]), Err(ParseError::Io(error)) if error.to_string() == "invalid value for --seq: bad")
        );
        assert!(
            matches!(request("report-metadata", &["id", "--source", " "]), Err(ParseError::Usage(message)) if message == "missing required --source")
        );
        assert!(
            matches!(request("report-metadata", &["id", "--source", "valid"]), Err(ParseError::Usage(message)) if message == "missing token to set or clear")
        );
    }

    fn product_request(
        regents: bool,
        command: &str,
        args: &[&str],
    ) -> Option<Result<Request, ParseError>> {
        leaves_for(regents)
            .into_iter()
            .find(|leaf| leaf.name == command)
            .map(|leaf| leaf.request(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>()))
    }

    #[test]
    fn regents_workspace_add_sends_an_absolute_folder_path() {
        let expected = std::env::current_dir()
            .unwrap()
            .join("projeto")
            .display()
            .to_string();

        let request = product_request(true, "add", &["projeto", "--focus"])
            .unwrap()
            .unwrap();

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:add", "method": "workspace.add",
                "params": {"path": expected, "focus": true}
            })
        );
        assert!(matches!(
            product_request(true, "add", &[]),
            Some(Err(ParseError::Clap(_)))
        ));
    }

    #[test]
    fn regents_workspace_list_shows_registered_workspaces_and_herdr_keeps_its_list() {
        let regents = product_request(true, "list", &[]).unwrap().unwrap();
        let herdr = product_request(false, "list", &[]).unwrap().unwrap();

        assert_eq!(
            serde_json::to_value(regents).unwrap()["method"],
            "workspace.registry.list"
        );
        assert_eq!(
            serde_json::to_value(herdr).unwrap()["method"],
            "workspace.list"
        );
        assert!(product_request(false, "add", &["x"]).is_none());
        assert!(product_request(false, "link", &["w1", "x"]).is_none());
        let herdr_names: Vec<_> = leaves_for(false).iter().map(|leaf| leaf.name).collect();
        let regents_names: Vec<_> = leaves_for(true).iter().map(|leaf| leaf.name).collect();
        assert_eq!(
            herdr_names,
            LEAVES.iter().map(|leaf| leaf.name).collect::<Vec<_>>()
        );
        assert_eq!(regents_names.len(), LEAVES.len() + 4);
    }

    #[test]
    fn regents_workspace_relink_sends_the_id_and_an_absolute_folder_path() {
        let expected = std::env::current_dir()
            .unwrap()
            .join("novo")
            .display()
            .to_string();

        let request = product_request(true, "relink", &["abc123", "novo"])
            .unwrap()
            .unwrap();

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:relink", "method": "workspace.relink",
                "params": {"id": "abc123", "path": expected}
            })
        );
        assert!(matches!(
            product_request(true, "relink", &["abc123"]),
            Some(Err(ParseError::Clap(_)))
        ));
        assert!(product_request(false, "relink", &["abc123", "novo"]).is_none());
    }

    #[test]
    fn regents_workspace_remove_sends_the_id() {
        let request = product_request(true, "remove", &["abc123"])
            .unwrap()
            .unwrap();

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:remove", "method": "workspace.remove",
                "params": {"id": "abc123"}
            })
        );
        assert!(product_request(false, "remove", &["abc123"]).is_none());
    }

    #[test]
    fn regents_workspace_link_sends_the_workspace_and_an_absolute_folder_path() {
        let expected = std::env::current_dir()
            .unwrap()
            .join("projeto")
            .display()
            .to_string();

        let request = product_request(true, "link", &["w2", "projeto"])
            .unwrap()
            .unwrap();

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:link", "method": "workspace.link",
                "params": {"workspace_id": "w2", "path": expected}
            })
        );
        assert!(matches!(
            product_request(true, "link", &["w2"]),
            Some(Err(ParseError::Clap(_)))
        ));
    }

    #[test]
    fn workspace_projection_uses_authoritative_clap_grammar() {
        assert_eq!(
            serde_json::to_value(request("create", &["--cwd=x"]).unwrap()).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:create", "method": "workspace.create", "params": {"cwd": "x", "focus": false}
            })
        );
        assert_eq!(
            serde_json::to_value(request("close", &["--group", "id"]).unwrap()).unwrap(),
            serde_json::json!({
                "id": "cli:workspace:close", "method": "workspace.close", "params": {"workspace_id": "id", "close_group": true}
            })
        );
        assert!(
            matches!(request("get", &[]), Err(ParseError::Clap(error)) if error.kind() == clap::error::ErrorKind::MissingRequiredArgument)
        );
        assert!(
            matches!(request("get", &["id", "extra"]), Err(ParseError::Clap(error)) if error.kind() == clap::error::ErrorKind::UnknownArgument)
        );
        assert!(
            matches!(request("create", &["--cwd"]), Err(ParseError::Clap(error)) if error.exit_code() == 2)
        );
        assert!(
            matches!(request("get", &["id", "--help"]), Err(ParseError::Clap(error)) if error.kind() == clap::error::ErrorKind::DisplayHelp && error.exit_code() == 0)
        );
        assert!(
            matches!(request("create", &["--env", "=value"]), Err(ParseError::Usage(message)) if message == "env key must not be empty")
        );
    }
}
