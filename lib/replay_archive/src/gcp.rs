//! Shared GCP credential loading for the GCS and KMS backends.

use anyhow::Context as _;
use google_cloud_auth::credentials::{
    Builder as AdcBuilder, Credentials, anonymous, external_account, impersonated, service_account,
    user_account,
};
use std::path::Path;

/// Builds [Credentials] from Application Default Credentials.
pub(crate) fn ambient_credentials() -> anyhow::Result<Credentials> {
    AdcBuilder::default()
        .build()
        .context(crate::GCP_CREDENTIALS_HINT)
}

/// Builds [Credentials] from a credentials JSON file at `path`.
///
/// Mirrors the credential-type dispatch that the auth library performs for
/// Application Default Credentials, which it only exposes for the well-known
/// ADC locations, not for arbitrary paths.
pub(crate) fn credentials_from_file(path: &Path) -> anyhow::Result<Credentials> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read GCP credentials file {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("GCP credentials file {} is not valid JSON", path.display()))?;
    let credential_type = json
        .get("type")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "GCP credentials file {} has no `type` field",
                path.display()
            )
        })?;
    let credentials = match credential_type.as_str() {
        "service_account" => service_account::Builder::new(json).build(),
        "external_account" => external_account::Builder::new(json).build(),
        "impersonated_service_account" => impersonated::Builder::new(json).build(),
        "authorized_user" => user_account::Builder::new(json).build(),
        other => anyhow::bail!(
            "unsupported credential type `{other}` in GCP credentials file {}",
            path.display()
        ),
    };
    credentials.with_context(|| {
        format!(
            "failed to build GCP credentials from file {}",
            path.display()
        )
    })
}

pub(crate) fn anonymous_credentials() -> Credentials {
    anonymous::Builder::new().build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_aws_external_account_credentials() {
        let credentials_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            credentials_file.path(),
            r#"{
                "type": "external_account",
                "audience": "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/provider",
                "subject_token_type": "urn:ietf:params:aws:token-type:aws4_request",
                "token_url": "https://sts.googleapis.com/v1/token",
                "credential_source": {
                    "environment_id": "aws1",
                    "region_url": "http://127.0.0.1:1/region",
                    "url": "http://127.0.0.1:1/credentials",
                    "regional_cred_verification_url": "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
                    "imdsv2_session_token_url": "http://127.0.0.1:1/token"
                }
            }"#,
        )
        .unwrap();

        credentials_from_file(credentials_file.path()).unwrap();
    }
}
