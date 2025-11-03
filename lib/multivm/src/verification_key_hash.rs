use std::{
    fmt::{self, Display},
    str::FromStr,
};

#[derive(Debug, PartialEq, Copy, Clone)]
pub struct VerificationKeyHash(&'static str);

impl Display for VerificationKeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// verification key hash generated from zksync-os v0.0.26, zksync-airbender v0.5.0 and zkos-wrapper v0.5.0
const V3_VK_HASH: &str = "0x6a4509801ec284b8921c63dc6aaba668a0d71382d87ae4095ffc2235154e9fa3";
pub const V3_VERIFICATION_KEY: VerificationKeyHash = VerificationKeyHash(V3_VK_HASH);

impl FromStr for VerificationKeyHash {
    type Err = anyhow::Error;

    fn from_str(vk_hash: &str) -> anyhow::Result<Self> {
        match vk_hash {
            V3_VK_HASH => Ok(V3_VERIFICATION_KEY),
            val => Err(anyhow::anyhow!("unknown verification key hash: {val}")),
        }
    }
}
