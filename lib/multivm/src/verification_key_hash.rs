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

macro_rules! define_verification_keys {
    ( $( ($doc:expr, $name:ident, $hash:expr) ),* $(,)? ) => {
        // Define the public constants for each key
        $(
            #[doc = $doc]
            pub const $name: VerificationKeyHash = VerificationKeyHash($hash);
        )*

        // Implement FromStr to parse from a string
        impl FromStr for VerificationKeyHash {
            type Err = anyhow::Error;

            fn from_str(vk_hash: &str) -> anyhow::Result<Self> {
                match vk_hash {
                    $(
                        $hash => Ok($name),
                    )*
                    val => Err(anyhow::anyhow!("unknown verification key hash: {val}")),
                }
            }
        }
    };
}

// Central place to define all verification keys.
// To add a new one, you only need to add a line here.
define_verification_keys! {
    (
        // comment for V3
        "verification key hash generated from zksync-os v0.0.26, zksync-airbender v0.5.0 and zkos-wrapper v0.5.0",
        // constant name to be used in the code
        V3_VERIFICATION_KEY,
        // actual verification key hash
        "0x6a4509801ec284b8921c63dc6aaba668a0d71382d87ae4095ffc2235154e9fa3"
    ),
}
