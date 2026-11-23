//! `ashi` is a double-entry financial transactions database.
//!
//! This crate is currently a scaffold: it exposes only build metadata while
//! the engine is being designed.

/// Returns the version of the `ashi` crate, as declared in its `Cargo.toml`.
///
/// # Examples
///
/// ```
/// assert_ne!(ashi::version(), "");
/// ```
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_matches_cargo_manifest() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
        assert_ne!(version(), "");
    }
}
