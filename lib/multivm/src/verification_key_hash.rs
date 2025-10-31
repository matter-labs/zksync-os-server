use std::fmt::{self, Display};

#[derive(Debug, PartialEq, Copy, Clone)]
pub struct VerificationKeyHash(&'static str);

impl Display for VerificationKeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// verification key hash generated from zksync-os v0.0.26 and zksync-airbender v0.5.0
pub const V3_VERIFICATION_KEY: VerificationKeyHash =
    VerificationKeyHash("0x6a4509801ec284b8921c63dc6aaba668a0d71382d87ae4095ffc2235154e9fa3");

impl TryFrom<String> for VerificationKeyHash {
    type Error = anyhow::Error;

    fn try_from(value: String) -> anyhow::Result<Self> {
        match value.as_str() {
            s if s == V3_VERIFICATION_KEY.0 => Ok(V3_VERIFICATION_KEY),
            _ => Err(anyhow::anyhow!("unknown verification key hash")),
        }
    }
}
