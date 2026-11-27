//! Process identity rules for the `regents` binary (Regents fork, ADR-0002).
//!
//! The `regents` binary compiles from the herdr sources. It keeps herdr's
//! internal names, including the `HERDR_*` environment contract that panes and
//! integration hooks use. Thus a `regents` process started inside a pane of a
//! herdr server inherits environment that binds it to that herdr server. This
//! module removes that binding at startup, so herdr and Regents never see each
//! other's servers.

pub(crate) mod channel;
pub(crate) mod folder;
pub(crate) mod registry;

use std::ffi::OsStr;
use std::path::Path;

/// Set by every server for its panes (`integration::apply_pane_base_env`):
/// the executable of the server that launched the pane.
const SERVER_BIN_PATH_ENV_VAR: &str = "HERDR_BIN_PATH";

/// Environment that a server sets for its panes and that binds a child process
/// to that server.
const SERVER_PANE_ENV_VARS: &[&str] = &[
    crate::HERDR_ENV_VAR,
    crate::api::SOCKET_PATH_ENV_VAR,
    crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
    crate::session::SESSION_ENV_VAR,
    crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
    crate::integration::HERDR_TAB_ID_ENV_VAR,
    crate::integration::HERDR_PANE_ID_ENV_VAR,
    SERVER_BIN_PATH_ENV_VAR,
];

/// Registry of workspace folders for the running product. Only Regents has
/// folder workspaces.
pub(crate) fn workspace_registry_path() -> Option<std::path::PathBuf> {
    crate::build_info::is_regents().then(registry::default_path)
}

/// Removes an inherited binding to a server of another product. Call it at the
/// start of `main`, before other threads start.
pub(crate) fn detach_from_other_product_server() {
    if !crate::build_info::is_regents() {
        return;
    }
    let product = crate::build_info::product_name();
    if std::env::var_os(crate::HERDR_ENV_VAR).as_deref() != Some(OsStr::new(crate::HERDR_ENV_VALUE))
    {
        return;
    }
    let server_bin_path = std::env::var_os(SERVER_BIN_PATH_ENV_VAR);
    let server_product = server_bin_path
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_stem)
        .and_then(OsStr::to_str);
    if server_product == Some(product) {
        return;
    }
    for var in SERVER_PANE_ENV_VARS {
        std::env::remove_var(var);
    }
}
