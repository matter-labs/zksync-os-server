//! Generates one validator's key set for BFT consensus and prints it in the formats the
//! node configuration expects:
//!
//! - `consensus.network_key` / `consensus.bls_key`: the two secrets, hex.
//! - the public committee entry (everything before `@`): what this validator
//!   contributes to every node's `consensus.validators` list — append `@<host:port>`.
//!
//! Secrets are printed to stdout and nowhere else; capture and store them like any
//! other operator key material.

use commonware_codec::{DecodeExt, Encode};
use commonware_cryptography::Signer;
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519;
use rand08::rngs::OsRng;

fn main() {
    let mut rng = OsRng;

    // An ed25519 private key is an arbitrary 32-byte seed.
    let mut seed = [0u8; 32];
    rand08::RngCore::fill_bytes(&mut rng, &mut seed);
    let network_key =
        ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes are a valid key");
    let network_public = network_key.public_key();

    let (bls_key, bls_public) = ops::keypair::<_, MinPk>(&mut rng);

    println!("# secrets — this validator's node configuration");
    println!(
        "consensus.network_key = \"{}\"",
        alloy::hex::encode(network_key.encode())
    );
    println!(
        "consensus.bls_key = \"{}\"",
        alloy::hex::encode(bls_key.encode())
    );
    println!();
    println!("# public — this validator's entry for every node's `consensus.validators`");
    println!(
        "{}:{}@<host:port>",
        alloy::hex::encode(network_public.encode()),
        alloy::hex::encode(bls_public.encode())
    );
}
