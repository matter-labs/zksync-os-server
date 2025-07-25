use reqwest::{Client, StatusCode};

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
}
