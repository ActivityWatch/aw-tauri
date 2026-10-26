/// The enclosing ActivityWatch release, not this host crate's 0.1.0 version.
/// Cargo tracks option_env! inputs and recompiles when AW_VERSION changes.
pub fn bundle_version() -> Option<&'static str> {
    option_env!("AW_VERSION").filter(|version| !version.is_empty())
}
