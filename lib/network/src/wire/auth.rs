use alloy::primitives::{Address, B256, Bytes, Signature, SignatureError, keccak256};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// Request to treat the current session as a verifier session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct VerifierRoleRequest {}

/// Main-node challenge used to authenticate a verifier session.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct VerifierChallenge {
    pub nonce: B256,
}

/// External-node authentication response proving control of the verifier signing key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct VerifierAuth {
    pub signature: Bytes,
}

/// Domain separator for verifier-session authentication signatures.
///
/// This keeps verifier-auth signatures distinct from signatures produced for other purposes.
const VERIFIER_AUTH_DOMAIN: &[u8] = b"zksync-os:verifier-auth:v1";

/// Returns the prehash that external nodes sign to prove control of a verifier key for `nonce`.
pub(crate) fn verifier_auth_prehash(nonce: B256) -> B256 {
    keccak256([VERIFIER_AUTH_DOMAIN, nonce.as_slice()].concat())
}

/// Recovers the verifier signer from the response signature for `nonce`.
pub(crate) fn recover_verifier_signer(
    nonce: B256,
    signature: &[u8],
) -> Result<Address, SignatureError> {
    let signature = Signature::from_raw(signature)?;
    signature.recover_address_from_prehash(&verifier_auth_prehash(nonce))
}

#[cfg(test)]
mod tests {
    use super::{recover_verifier_signer, verifier_auth_prehash};
    use alloy::primitives::B256;
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;
    use std::str::FromStr;

    #[test]
    fn verifier_auth_round_trip_recovers_signer() {
        let signer = PrivateKeySigner::from_str(
            "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
        )
        .unwrap();
        let nonce = B256::repeat_byte(0xAB);
        let signature = signer
            .sign_hash_sync(&verifier_auth_prehash(nonce))
            .unwrap();

        let recovered = recover_verifier_signer(nonce, &signature.as_bytes()).unwrap();
        assert_eq!(recovered, signer.address());
    }

    #[test]
    fn malformed_signature_is_rejected() {
        let err = recover_verifier_signer(B256::repeat_byte(0xAB), &[7u8; 64]).unwrap_err();
        assert!(matches!(
            err,
            alloy::primitives::SignatureError::FromBytes(_)
        ));
    }
}
