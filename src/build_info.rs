//! Build identity helpers.
//!
//! User-facing messages name the running binary, never a fixed `herdr`. Use
//! `product_name()` for command names and the lowercase product noun ("no
//! herdr server"), `product_command()` for a command hint with arguments, and
//! `product_display_name()` for capitalized prose ("restart Herdr").

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Product name of the running binary: `regents` for the Regents fork binary,
/// `herdr` otherwise. Both binaries compile from the same sources.
pub fn product_name() -> &'static str {
    #[cfg(test)]
    if let Some(name) = PRODUCT_NAME_OVERRIDE.with(std::cell::Cell::get) {
        return name;
    }
    match option_env!("CARGO_BIN_NAME") {
        Some("regents") => "regents",
        _ => "herdr",
    }
}

/// Product name for prose in user-facing messages: `Regents` or `Herdr`.
pub fn product_display_name() -> &'static str {
    if is_regents() {
        "Regents"
    } else {
        "Herdr"
    }
}

/// Command line that runs the running binary with `args`, for command hints in
/// user-facing messages: `herdr server stop` or `regents server stop`.
pub fn product_command(args: &str) -> String {
    format!("{} {args}", product_name())
}

#[cfg(test)]
thread_local! {
    static PRODUCT_NAME_OVERRIDE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Runs `f` as if the running binary were `regents`. The unit tests compile
/// only into the `herdr` binary, so this is how they cover Regents output.
#[cfg(test)]
pub(crate) fn as_regents_for_test<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(Option<&'static str>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PRODUCT_NAME_OVERRIDE.with(|name| name.set(self.0));
        }
    }
    let _restore = Restore(PRODUCT_NAME_OVERRIDE.with(|name| name.replace(Some("regents"))));
    f()
}

/// True for the `regents` binary of the Regents fork.
pub fn is_regents() -> bool {
    product_name() == "regents"
}

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }
}
