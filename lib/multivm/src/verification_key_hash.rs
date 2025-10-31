use std::fmt::{self, Display};

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

impl TryFrom<&str> for VerificationKeyHash {
    type Error = anyhow::Error;

    fn try_from(vk_hash: &str) -> anyhow::Result<Self> {
        match vk_hash {
            V3_VK_HASH => Ok(V3_VERIFICATION_KEY),
            val => Err(anyhow::anyhow!("unknown verification key hash: {val}")),
        }
    }
}

impl TryFrom<String> for VerificationKeyHash {
    type Error = anyhow::Error;

    fn try_from(value: String) -> anyhow::Result<Self> {
        // Just forwarding to the &str implementation
        VerificationKeyHash::try_from(value.as_str())
    }
}
