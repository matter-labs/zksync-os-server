# Prover API

Endpoint reference. For how proving works — the two stages, the two proof
systems, modes, configuration and failure handling — see [Proving](provers.md).

All routes are served under `/prover-jobs/v1`.

```
GET  /status/                       all lanes

POST /FRI/pick                      Airbender per-batch FRI
POST /FRI/submit
POST /FRI/{id}/failed               report a failed proof
GET  /FRI/{id}/peek

POST /SNARK/pick                    Airbender range SNARK
POST /SNARK/submit
GET  /SNARK/{from}/{to}/peek

POST /ZiSK/pick                     ZiSK per-batch vadcop_final
POST /ZiSK/submit
GET  /ZiSK/status
GET  /ZiSK/{batch_number}/peek

POST /ZiSK-AGG/pick                 ZiSK range aggregation
POST /ZiSK-AGG/submit
```

The `ZiSK*` routes are registered only when the second proof system is enabled.
The API has no authentication — keep the port internal.
