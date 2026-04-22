use axum::http::StatusCode;

pub(crate) async fn live() -> StatusCode {
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_always_returns_200() {
        assert_eq!(live().await, StatusCode::OK);
    }
}
