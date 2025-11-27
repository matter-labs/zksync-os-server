# Prover API

```
        .route("/prover-jobs/v1/status", get(status))
        .route("/prover-jobs/v1/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/v1/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/v1/SNARK/pick", post(pick_snark_job))
        .route("/prover-jobs/v1/SNARK/submit", post(submit_snark_proof))
```
