//! Metadata information about the node.

use std::sync::LazyLock;

pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const NODE_ZKSYNC_OS_VERSION: &str = concat!(
    "zksync-os/v",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    ".",
    env!("CARGO_PKG_VERSION_PATCH")
);

pub static NODE_SEMVER_VERSION: LazyLock<semver::Version> = LazyLock::new(|| {
    NODE_VERSION
        .parse()
        .expect("node has invalid semver version")
});
