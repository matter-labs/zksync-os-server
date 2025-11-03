use axum::{
    Router,
    routing::{get, post},
};

use crate::prover_api::prover_server::{
    AppState,
    legacy::handlers::{
        get_failed_fri_proof, peek_batch_data, peek_fri_proofs, pick_fri_job, pick_snark_job,
        status, submit_fri_proof, submit_snark_proof,
    },
};

pub(in crate::prover_api::prover_server) fn legacy_routes() -> Router<AppState> {
    Router::new()
        // server <-> prover routes
        .route("/FRI/pick", post(pick_fri_job))
        .route("/FRI/submit", post(submit_fri_proof))
        .route("/SNARK/pick", post(pick_snark_job))
        .route("/SNARK/submit", post(submit_snark_proof))
        // debugging routes
        .route("/FRI/{id}/peek", get(peek_batch_data))
        .route("/FRI/{id}/failed", get(get_failed_fri_proof))
        .route("/SNARK/{from}/{to}/peek", get(peek_fri_proofs))
        .route("/status/", get(status))
}
