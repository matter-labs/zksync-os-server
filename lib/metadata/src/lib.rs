//! Metadata information about the node.

use std::sync::LazyLock;

/// Current node version as a string.
pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current client version: `zksync-os/v{major}.{minor}.{patch}`. Prerelease identifier and build
/// metadata are purposefully ignored.
pub const NODE_CLIENT_VERSION: &str = concat!(
    "zksync-os/v",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    ".",
    env!("CARGO_PKG_VERSION_PATCH")
);

/// Current node version as a SemVer version.
pub static NODE_SEMVER_VERSION: LazyLock<semver::Version> = LazyLock::new(|| {
    NODE_VERSION
        .parse()
        .expect("node has invalid semver version")
});
