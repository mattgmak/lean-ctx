/// Sets one env var for a test and restores it on drop. Holds the test env
/// lock for its whole life: `serial_test` groups alone only serialize tests
/// that opt into the same group, while any other test reading the variable
/// (or mutating the env under the lock) would still race it.
#[cfg(test)]
pub(crate) struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
    _env: crate::core::data_dir::TestEnvGuard,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let env = crate::core::data_dir::test_env_lock();
        let previous = std::env::var_os(key);
        crate::test_env::set_var(key, value);
        Self {
            key,
            previous,
            _env: env,
        }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    // Runs before the fields drop, so the env is restored while the lock is
    // still held.
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            crate::test_env::set_var(self.key, previous);
        } else {
            crate::test_env::remove_var(self.key);
        }
    }
}
