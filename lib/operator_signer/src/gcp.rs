use alloy::signers::gcp::{
    GcpKeyRingRef, GcpSigner, KeySpecifier,
    gcloud_sdk::{
        GoogleApi,
        google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient,
    },
};

/// Creates a GCP KMS signer from a resource name.
pub(crate) async fn create_gcp_signer(resource_name: &str) -> anyhow::Result<GcpSigner> {
    let (keyring, key_id, version) = parse_kms_resource_name(resource_name)?;
    let specifier = KeySpecifier::new(keyring, &key_id, version);

    let client = GoogleApi::from_function(
        KeyManagementServiceClient::new,
        "https://cloudkms.googleapis.com",
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to create GCP KMS client: {e}"))?;

    GcpSigner::new(client, specifier, None)
        .await
        .map_err(|e| anyhow::anyhow!("failed to initialize GCP KMS signer: {e}"))
}

/// Parses a KMS resource name into its components.
///
/// Expected format:
/// `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`
pub(crate) fn parse_kms_resource_name(
    resource_name: &str,
) -> anyhow::Result<(GcpKeyRingRef, String, u64)> {
    let parts: Vec<&str> = resource_name.split('/').collect();
    if parts.len() != 10
        || parts[0] != "projects"
        || parts[2] != "locations"
        || parts[4] != "keyRings"
        || parts[6] != "cryptoKeys"
        || parts[8] != "cryptoKeyVersions"
    {
        anyhow::bail!(
            "invalid KMS resource name format: expected \
             'projects/{{project}}/locations/{{location}}/keyRings/{{ring}}/cryptoKeys/{{key}}/cryptoKeyVersions/{{version}}', \
             got '{resource_name}'"
        );
    }

    let project_id = parts[1];
    let location = parts[3];
    let keyring_name = parts[5];
    let key_id = parts[7].to_string();
    let version: u64 = parts[9]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid key version number: '{}'", parts[9]))?;

    let keyring = GcpKeyRingRef::new(project_id, location, keyring_name);
    Ok((keyring, key_id, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kms_resource_name_valid() {
        let resource = "projects/my-project/locations/us-central1/keyRings/my-ring/cryptoKeys/my-key/cryptoKeyVersions/1";
        let (keyring, key_id, version) = parse_kms_resource_name(resource).unwrap();
        assert_eq!(keyring.google_project_id, "my-project");
        assert_eq!(keyring.location, "us-central1");
        assert_eq!(keyring.name, "my-ring");
        assert_eq!(key_id, "my-key");
        assert_eq!(version, 1);
    }

    #[test]
    fn test_parse_kms_resource_name_invalid() {
        assert!(parse_kms_resource_name("invalid/resource/name").is_err());
        assert!(parse_kms_resource_name("").is_err());
        assert!(
            parse_kms_resource_name(
                "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/notanumber"
            )
            .is_err()
        );
    }
}
