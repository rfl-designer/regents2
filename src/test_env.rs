//! RAII guards that set or remove process environment variables in tests and
//! restore the previous values on drop. Hold the env lock of the module under
//! test while a guard is alive.

use std::ffi::OsString;

fn restore_env_var(key: &str, value: Option<OsString>) {
    if let Some(value) = value {
        std::env::set_var(key, value);
    } else {
        std::env::remove_var(key);
    }
}

pub(crate) struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        restore_env_var(self.key, self.previous.clone());
    }
}

pub(crate) struct EnvVarsRemovedGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvVarsRemovedGuard {
    pub(crate) fn new(keys: &[&'static str]) -> Self {
        let previous: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            std::env::remove_var(key);
        }
        Self { previous }
    }
}

impl Drop for EnvVarsRemovedGuard {
    fn drop(&mut self) {
        for (key, value) in self.previous.clone() {
            restore_env_var(key, value);
        }
    }
}
