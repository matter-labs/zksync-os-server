# Changelog

## [0.1.1](https://github.com/matter-labs/zksync-os-server/compare/v0.1.0...v0.1.1) (2025-08-19)


### Features

* add mini merkle tree crate ([#169](https://github.com/matter-labs/zksync-os-server/issues/169)) ([3c068ea](https://github.com/matter-labs/zksync-os-server/commit/3c068ead7d98dc7fd8441f7e5ad41b9619c3e44a))
* allow replaying blocks from zero ([#197](https://github.com/matter-labs/zksync-os-server/issues/197)) ([b0da499](https://github.com/matter-labs/zksync-os-server/commit/b0da499e09a978b55aa3c5bf0e278ac2dd20ad54))
* **api:** implement `ots_` namespace; add support for local Otterscan ([#168](https://github.com/matter-labs/zksync-os-server/issues/168)) ([dae4794](https://github.com/matter-labs/zksync-os-server/commit/dae47942cfadc885910b5ab0f158a2ef16612dd3))
* **api:** implement `zks_getL2ToL1LogProof` ([#203](https://github.com/matter-labs/zksync-os-server/issues/203)) ([c83e1c8](https://github.com/matter-labs/zksync-os-server/commit/c83e1c8e078f7346f4f3ded10d90d35c6f9b108c))
* **api:** limit req/resp body size ([#204](https://github.com/matter-labs/zksync-os-server/issues/204)) ([db19257](https://github.com/matter-labs/zksync-os-server/commit/db19257919f8cacb37cafa079d42f8fa0b4af548))
* component state observability ([#187](https://github.com/matter-labs/zksync-os-server/issues/187)) ([d961485](https://github.com/matter-labs/zksync-os-server/commit/d961485a5d3204a92eb2a2e6ab0bfb4d60c31190))
* dump block input on `run_block` error ([#165](https://github.com/matter-labs/zksync-os-server/issues/165)) ([75f76ac](https://github.com/matter-labs/zksync-os-server/commit/75f76acda4bd167c22b88c3b9567a71a54fac7bc))
* Instructions on how to run 2 chains, and prometheus config ([#195](https://github.com/matter-labs/zksync-os-server/issues/195)) ([3b890fb](https://github.com/matter-labs/zksync-os-server/commit/3b890fb8c2c8ead4c1dbf6e343d8f735ed5230d5))
* **l1-sender:** basic http support ([#175](https://github.com/matter-labs/zksync-os-server/issues/175)) ([92a90fa](https://github.com/matter-labs/zksync-os-server/commit/92a90fa8d65b04d8368e074afc40f1992d684b72))
* **l1-sender:** implement L1 batch execution ([#157](https://github.com/matter-labs/zksync-os-server/issues/157)) ([5d27812](https://github.com/matter-labs/zksync-os-server/commit/5d278121f4c0abe37e416b82c663ae8b9b4f04f7))
* **l1-watcher:** implement basic `L1CommitWatcher` ([#189](https://github.com/matter-labs/zksync-os-server/issues/189)) ([326ac6b](https://github.com/matter-labs/zksync-os-server/commit/326ac6b33c069a46e6648388e396f86d2a1b49bf))
* **l1-watcher:** track last committed block ([#194](https://github.com/matter-labs/zksync-os-server/issues/194)) ([dda3a18](https://github.com/matter-labs/zksync-os-server/commit/dda3a1884b33501eb287c14f01a67406e0981dbc))
* **l1-watcher:** track last executed block ([#199](https://github.com/matter-labs/zksync-os-server/issues/199)) ([c34194d](https://github.com/matter-labs/zksync-os-server/commit/c34194d81d90cab4e654ffc7b0638c8420f6ff20))
* limit number of blocks per batch ([#192](https://github.com/matter-labs/zksync-os-server/issues/192)) ([195ce8f](https://github.com/matter-labs/zksync-os-server/commit/195ce8ffb737b96ee11ad79e83c72a7fd809c472))
* proper batching ([#167](https://github.com/matter-labs/zksync-os-server/issues/167)) ([e3b5ebc](https://github.com/matter-labs/zksync-os-server/commit/e3b5ebc9fc46d74594a9cc897f0d7efc5f367a41))
* report earliest block number ([#216](https://github.com/matter-labs/zksync-os-server/issues/216)) ([af9263f](https://github.com/matter-labs/zksync-os-server/commit/af9263f078146c9370460ebd748cd07f33780f9b))
* save `node_version` and `block_output_hash` in `ReplayRecord` ([#162](https://github.com/matter-labs/zksync-os-server/issues/162)) ([50eb1af](https://github.com/matter-labs/zksync-os-server/commit/50eb1afb70649b2ec23d82191e946cc3beec03a6))
* save proper block 0 ([#198](https://github.com/matter-labs/zksync-os-server/issues/198)) ([ca8d46b](https://github.com/matter-labs/zksync-os-server/commit/ca8d46b585b88652a064a53cc709ab05d20a554d))
* setup release please ([#156](https://github.com/matter-labs/zksync-os-server/issues/156)) ([0a0f170](https://github.com/matter-labs/zksync-os-server/commit/0a0f170d2f22ffc3580a30d0f16db21eb01766d9))
* **storage:** implement batch storage ([#200](https://github.com/matter-labs/zksync-os-server/issues/200)) ([0c06f14](https://github.com/matter-labs/zksync-os-server/commit/0c06f14fa3cda7f4768464da1d3e8130b39a9c5a))
* support real SNARK provers ([#164](https://github.com/matter-labs/zksync-os-server/issues/164)) ([5ced71c](https://github.com/matter-labs/zksync-os-server/commit/5ced71c9bb147bf2cc8ec1eaabcc29dad0ef8c61))
* unify batcher subsystem latency tracking ([#170](https://github.com/matter-labs/zksync-os-server/issues/170)) ([25e0301](https://github.com/matter-labs/zksync-os-server/commit/25e030194c58665b35f5af6c4e38662473302d1f))
* upgrade zksync-os to 0.0.10 ([#215](https://github.com/matter-labs/zksync-os-server/issues/215)) ([53a4e82](https://github.com/matter-labs/zksync-os-server/commit/53a4e824990da42e37b55e406f1308d5d92ead25))


### Bug Fixes

* adopt some channel capacity to accomodate all rescheduled jobs ([2bd5878](https://github.com/matter-labs/zksync-os-server/commit/2bd5878eb7fac663b00782b3d8394d89195f1f5c))
* **api:** disable Prague in mempool ([9c00b42](https://github.com/matter-labs/zksync-os-server/commit/9c00b427ee78266327406cf2fd60b37d3ab968c3))
* **l1-watch:** support new deployments ([#166](https://github.com/matter-labs/zksync-os-server/issues/166)) ([8215db9](https://github.com/matter-labs/zksync-os-server/commit/8215db9de3bf614e2e527a1aad9467bcc9d101a5))
* skip already processed l1 transactions in watcher on restart ([#172](https://github.com/matter-labs/zksync-os-server/issues/172)) ([b290405](https://github.com/matter-labs/zksync-os-server/commit/b290405529160de4840ac12f1b90cc8161026a15))
* state recovery - read persisted repository block - not memory ([#191](https://github.com/matter-labs/zksync-os-server/issues/191)) ([146cb19](https://github.com/matter-labs/zksync-os-server/commit/146cb19f3798fc064fcb9b771b200dcefa266f43))
* **storage:** report proper lazy latest block ([#193](https://github.com/matter-labs/zksync-os-server/issues/193)) ([a570006](https://github.com/matter-labs/zksync-os-server/commit/a570006b3a215f48fa117b4ab47870b707b770da))
* update release version suffix for crates in CI ([#159](https://github.com/matter-labs/zksync-os-server/issues/159)) ([8c661fe](https://github.com/matter-labs/zksync-os-server/commit/8c661fea8e30d2d3396161b3f9013085c4de467a))
* use spawn instead of select! to start everything ([#185](https://github.com/matter-labs/zksync-os-server/issues/185)) ([09a71af](https://github.com/matter-labs/zksync-os-server/commit/09a71afef222835282c3a1952ef4f04793603c26))
