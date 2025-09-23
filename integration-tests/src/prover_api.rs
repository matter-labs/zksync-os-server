use backon::{ConstantBuilder, Retryable};
use reqwest::{Client, StatusCode};
use std::time::Duration;

pub struct ProverApi {
    client: Client,
    base_url: String,
}

impl ProverApi {
    /// Create a new client targeting the given base URL
    pub fn new<U: Into<String>>(base_url: U) -> Self {
        ProverApi {
            client: Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Checks batch status.
    /// Returns `true` if batch has been proven, `false` otherwise.
    pub async fn check_batch_status(&self, batch_number: u64) -> anyhow::Result<bool> {
        let url = format!("{}/prover-jobs/FRI/{batch_number}", self.base_url);
        let resp = self.client.get(&url).send().await?;

        match resp.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NO_CONTENT => Ok(false),
            s => Err(anyhow::anyhow!(
                "Unexpected status {} when fetching next block",
                s
            )),
        }
    }

    /// Resolves when the requested batch gets reported as proven by prover API.
    pub async fn wait_for_batch_proven(&self, batch_number: u64) -> anyhow::Result<()> {
        (|| async {
            let status = self.check_batch_status(batch_number).await?;
            if status {
                Ok(())
            } else {
                Err(anyhow::anyhow!("batch is not ready yet"))
            }
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_secs(1))
                .with_max_times(10),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "proof not ready yet, retrying");
        })
        .await
    }
}
