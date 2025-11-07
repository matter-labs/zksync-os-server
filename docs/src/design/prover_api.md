# Prover API

```
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/SNARK/pick", post(pick_snark_job))
        .route("/prover-jobs/SNARK/submit", post(submit_snark_proof))
```
