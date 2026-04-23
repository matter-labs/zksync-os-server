# Changelog

## [0.19.1](https://github.com/matter-labs/zksync-os-server/compare/v0.19.0...v0.19.1) (2026-04-23)


### Features

* allow l1 poll interval to be configured ([#1175](https://github.com/matter-labs/zksync-os-server/issues/1175)) ([14c8bb6](https://github.com/matter-labs/zksync-os-server/commit/14c8bb624f7ab0196e61ded57a23e1ba3afae60a)), closes [#1176](https://github.com/matter-labs/zksync-os-server/issues/1176)
* async batch provider init ([#1160](https://github.com/matter-labs/zksync-os-server/issues/1160)) ([d5867dc](https://github.com/matter-labs/zksync-os-server/commit/d5867dce972429c0c6bd54114632c5b6111d5542))
* bytecodes supplier support ([#1155](https://github.com/matter-labs/zksync-os-server/issues/1155)) ([bc1c98c](https://github.com/matter-labs/zksync-os-server/commit/bc1c98cc307515b6ffc9511aa2e7a0062a20178f))
* detect SL intervals using L1 ([#1185](https://github.com/matter-labs/zksync-os-server/issues/1185)) ([ac97007](https://github.com/matter-labs/zksync-os-server/commit/ac97007392f205f0845f3307acdb12c5492ce797))
* **l1_sender:** configurable tx timeout and inclusion latency metric ([#1162](https://github.com/matter-labs/zksync-os-server/issues/1162)) ([b09f672](https://github.com/matter-labs/zksync-os-server/commit/b09f6726564e2712edd8b380ff68461b1158778b))
* **l1-sender:** In-flight tx detection & minor enhancements ([#1172](https://github.com/matter-labs/zksync-os-server/issues/1172)) ([3defe95](https://github.com/matter-labs/zksync-os-server/commit/3defe9538dcfd0efe4aa8a9e12a0b9bad422b56e))
* **l1-sender:** increase L1 required confirmations ([#1189](https://github.com/matter-labs/zksync-os-server/issues/1189)) ([51b65bf](https://github.com/matter-labs/zksync-os-server/commit/51b65bf03fad370ec1cd6bdfcd5832bee1771390))
* **l1-sender:** Increase required confirmations to 3 ([#1197](https://github.com/matter-labs/zksync-os-server/issues/1197)) ([100f218](https://github.com/matter-labs/zksync-os-server/commit/100f218b6ef1a0381d8610de9245d08c85ed1725))
* **rpc:** add txpool_inspect/content/status RPC methods ([#1200](https://github.com/matter-labs/zksync-os-server/issues/1200)) ([5ee3f21](https://github.com/matter-labs/zksync-os-server/commit/5ee3f21444df61d8ca4a61f756a3a0a683a029bc))
* **rpc:** use log index in eth_getLogs to skip candidate blocks ([#1182](https://github.com/matter-labs/zksync-os-server/issues/1182)) ([8dc21c2](https://github.com/matter-labs/zksync-os-server/commit/8dc21c2d124f71a21c3879f6470288a4c28b25a0))
* **storage:** build RocksDB log index on block write/rollback ([#1158](https://github.com/matter-labs/zksync-os-server/issues/1158)) ([28a0d63](https://github.com/matter-labs/zksync-os-server/commit/28a0d63b57b9ca6aa45baedc5223c839765c9b0d))


### Bug Fixes

* BatchVerificationRunner shutdown ([#1184](https://github.com/matter-labs/zksync-os-server/issues/1184)) ([25048c5](https://github.com/matter-labs/zksync-os-server/commit/25048c5ba889ee1f9a45bb3809292b53dae34471))
* bump rustls-webpki to 0.103.12 ([#1177](https://github.com/matter-labs/zksync-os-server/issues/1177)) ([b427f45](https://github.com/matter-labs/zksync-os-server/commit/b427f45e2f4ae118e087dbaea20a57f25119f603))
* **deps:** patch rustls-webpki advisory ([#1203](https://github.com/matter-labs/zksync-os-server/issues/1203)) ([da7d944](https://github.com/matter-labs/zksync-os-server/commit/da7d944597270974a767d0f14ea02c02313df38b))
* minor issues that lead to panic/error ([#1181](https://github.com/matter-labs/zksync-os-server/issues/1181)) ([fa29459](https://github.com/matter-labs/zksync-os-server/commit/fa29459fbd89914cd8737e7a6e6247a2ee8b440a))
* record sl block used for l1 state fetch ([#1166](https://github.com/matter-labs/zksync-os-server/issues/1166)) ([76f44d2](https://github.com/matter-labs/zksync-os-server/commit/76f44d2ee13251fb804449d7f6d63576ebe885dc))
* resolve misc warnings ([#1180](https://github.com/matter-labs/zksync-os-server/issues/1180)) ([6f2ab9f](https://github.com/matter-labs/zksync-os-server/commit/6f2ab9f1f38a1575a303a0d52bceba35a83abad4))
* return back required confirmations to 1 ([#1205](https://github.com/matter-labs/zksync-os-server/issues/1205)) ([1f4a966](https://github.com/matter-labs/zksync-os-server/commit/1f4a966d7539a6b9a30c60e497eb418658b27724))
* **rpc:** correct error codes and add detailed rejection metrics for eth_sendRawTransaction ([#1199](https://github.com/matter-labs/zksync-os-server/issues/1199)) ([b9960d3](https://github.com/matter-labs/zksync-os-server/commit/b9960d3028c4b4870a2cd99e0e41b422688656e0))

## [0.19.0](https://github.com/matter-labs/zksync-os-server/compare/v0.18.2...v0.19.0) (2026-04-08)


### ⚠ BREAKING CHANGES

* **network:** move batch verification to devp2p ([#1149](https://github.com/matter-labs/zksync-os-server/issues/1149))

### Features

* add config option to `reset_timestamps` for rebuilt blocks ([#1157](https://github.com/matter-labs/zksync-os-server/issues/1157)) ([a59aaf9](https://github.com/matter-labs/zksync-os-server/commit/a59aaf974f867ff2dba3405f53aaade433814f71))
* **batcher:** SLI-based sealing ([#1130](https://github.com/matter-labs/zksync-os-server/issues/1130)) ([9e268e3](https://github.com/matter-labs/zksync-os-server/commit/9e268e3ea3db45bb07de7b6906125e40caf8bf79))
* **integration-tests:** expose fatal node errors from harness ([#1148](https://github.com/matter-labs/zksync-os-server/issues/1148)) ([a196f5b](https://github.com/matter-labs/zksync-os-server/commit/a196f5ba5027be32a880b05e2f12c7d3603d9e21))
* **network:** move batch verification to devp2p ([#1149](https://github.com/matter-labs/zksync-os-server/issues/1149)) ([7a140a7](https://github.com/matter-labs/zksync-os-server/commit/7a140a772034b2d01773ca37d3d5a81e03470ceb))
* **rpc:** track mempool insertion and forwarding latency for eth_sendRawTransaction ([#1153](https://github.com/matter-labs/zksync-os-server/issues/1153)) ([0caa6e2](https://github.com/matter-labs/zksync-os-server/commit/0caa6e2dbd136b0dabe4996355afe5face5df851))
* **storage_api:** add LogIndex supertrait to ReadRepository ([#1156](https://github.com/matter-labs/zksync-os-server/issues/1156)) ([27c1fcf](https://github.com/matter-labs/zksync-os-server/commit/27c1fcfc162d2588f6750592da3f52afd1f63231))
* update v31 state and zksync-os dev ([#1132](https://github.com/matter-labs/zksync-os-server/issues/1132)) ([59d50fc](https://github.com/matter-labs/zksync-os-server/commit/59d50fcb0ca8feb39c4c6edbec41d798596beb3e))
* Use log id for interop root indexing ([#1092](https://github.com/matter-labs/zksync-os-server/issues/1092)) ([8c074ba](https://github.com/matter-labs/zksync-os-server/commit/8c074baa182ce324337830117d728a9bc538e960))


### Bug Fixes

* **loadbase:** Increase max-in-flight from 10 to 1000 in loadbase ([#1136](https://github.com/matter-labs/zksync-os-server/issues/1136)) ([40585b7](https://github.com/matter-labs/zksync-os-server/commit/40585b7dfecaa934c19c1549ee08f009a68e548a))
* **mempool:** track all L2 transaction outflow paths in metrics ([#1151](https://github.com/matter-labs/zksync-os-server/issues/1151)) ([895dd18](https://github.com/matter-labs/zksync-os-server/commit/895dd183ee9feed720aedbb822f12738ed073d35))
* restart node on unexpected l1 commit ([#1147](https://github.com/matter-labs/zksync-os-server/issues/1147)) ([dd558fc](https://github.com/matter-labs/zksync-os-server/commit/dd558fc9c22edceba946e3adddd7b5fdfff70e60))
* **rpc:** map client errors to -32602, keep server errors at -32603 ([#1137](https://github.com/matter-labs/zksync-os-server/issues/1137)) ([fe3275f](https://github.com/matter-labs/zksync-os-server/commit/fe3275f556e0f3b01a8fe1984f5986f7237972cf))
* **rpc:** move wait_for_db_ready_to_process_blocks tobackground ([#1150](https://github.com/matter-labs/zksync-os-server/issues/1150)) ([5f1222f](https://github.com/matter-labs/zksync-os-server/commit/5f1222f08dc13073b18628be2b1deca60c7971eb))

## [0.18.2](https://github.com/matter-labs/zksync-os-server/compare/v0.18.1...v0.18.2) (2026-04-01)


### Features

* **batcher:** add config option to disable batcher subsystem ([#1119](https://github.com/matter-labs/zksync-os-server/issues/1119)) ([0ea9c7f](https://github.com/matter-labs/zksync-os-server/commit/0ea9c7f47bd7a52159d7e513d0e3352b9f2b6600))
* use `getSettlementLayer` to determine SL ([#1116](https://github.com/matter-labs/zksync-os-server/issues/1116)) ([dd1bc0c](https://github.com/matter-labs/zksync-os-server/commit/dd1bc0c0dc1bd3b84c0c6615102777fa314a2520))


### Bug Fixes

* fix batcher shutdown and enable revert test ([#1122](https://github.com/matter-labs/zksync-os-server/issues/1122)) ([a846796](https://github.com/matter-labs/zksync-os-server/commit/a846796043ccc6a44ef9800a7245105a7e49ad10))
* **prover_api:** use drop guards to ensure metrics are always recorded ([#1126](https://github.com/matter-labs/zksync-os-server/issues/1126)) ([e5df063](https://github.com/matter-labs/zksync-os-server/commit/e5df0636b80960a02e6d9476a7a220aa6de7610e))
* **rpc:** record latency for cancelled requests (client disconnect) ([#1125](https://github.com/matter-labs/zksync-os-server/issues/1125)) ([7027315](https://github.com/matter-labs/zksync-os-server/commit/7027315cee8c9943ac5b3ac85989678438da3deb))
* **tracer:** Fix tracer only_top_call handling ([#1127](https://github.com/matter-labs/zksync-os-server/issues/1127)) ([9a88dc1](https://github.com/matter-labs/zksync-os-server/commit/9a88dc13ddb0b761c99dbabf754dbbbf3f8dfc13))

## [0.18.1](https://github.com/matter-labs/zksync-os-server/compare/v0.18.0...v0.18.1) (2026-03-30)


### Features

* **config:** generate declarative config validation ([#1090](https://github.com/matter-labs/zksync-os-server/issues/1090)) ([491b930](https://github.com/matter-labs/zksync-os-server/commit/491b93036e144aa0afc178e07cc07cec87eaaf2e))
* **config:** set production-oriented defaults, extract local dev overrides ([#1062](https://github.com/matter-labs/zksync-os-server/issues/1062)) ([5e850f5](https://github.com/matter-labs/zksync-os-server/commit/5e850f54b0be944b3d1b1ae33f20a2edf9f9fd05))
* **minor:** improve logging of executed transactions ([#1094](https://github.com/matter-labs/zksync-os-server/issues/1094)) ([da4a52a](https://github.com/matter-labs/zksync-os-server/commit/da4a52a31895d308eedc08f672e5b7781825ee94))
* **network:** report metrics from `reth-network` crate ([#1063](https://github.com/matter-labs/zksync-os-server/issues/1063)) ([2e1ec9d](https://github.com/matter-labs/zksync-os-server/commit/2e1ec9d114ca6e325fb05f7a3240c7997d2326cd))
* **network:** support `network_interface` and DNS boot nodes ([#1075](https://github.com/matter-labs/zksync-os-server/issues/1075)) ([f2afb8d](https://github.com/matter-labs/zksync-os-server/commit/f2afb8d544dfad2159ff496ab1c6bec3883e0e7e))
* **tracer:** Meaningful errors for out-of-pubdata reverts ([#1058](https://github.com/matter-labs/zksync-os-server/issues/1058)) ([e62b216](https://github.com/matter-labs/zksync-os-server/commit/e62b216758b795f40061691db497c9f51d1899ba))


### Bug Fixes

* avoid marking rebuild tx source entries invalid ([#1109](https://github.com/matter-labs/zksync-os-server/issues/1109)) ([c5c339f](https://github.com/matter-labs/zksync-os-server/commit/c5c339f345fcdae447fcc02a5a29b13045184758))
* EN preimage persisting before block saved ([#1114](https://github.com/matter-labs/zksync-os-server/issues/1114)) ([fafd039](https://github.com/matter-labs/zksync-os-server/commit/fafd03988cc1ddf4b5a794a3fc24573c7ef9d754))
* fix batch storage in revert case ([#1081](https://github.com/matter-labs/zksync-os-server/issues/1081)) ([4cce328](https://github.com/matter-labs/zksync-os-server/commit/4cce328f3fbe5df9d44ce181e2ccdef2b8e711a4))
* handle already reverted batches in commit watcher ([#1096](https://github.com/matter-labs/zksync-os-server/issues/1096)) ([887ff33](https://github.com/matter-labs/zksync-os-server/commit/887ff3315fa4e2f7b3dbe4457352533c5063af61))
* ignore permission denied for stale port lockfiles ([#1088](https://github.com/matter-labs/zksync-os-server/issues/1088)) ([edafd60](https://github.com/matter-labs/zksync-os-server/commit/edafd604db1d95b8693a73d27d60dd0922800047))
* **l1-watcher:** wait two L1 blocks before processing events ([#1091](https://github.com/matter-labs/zksync-os-server/issues/1091)) ([2afb0b4](https://github.com/matter-labs/zksync-os-server/commit/2afb0b49c07311c719a312d0957e754475babc8e))
* make block executor wait for block applier ([#1108](https://github.com/matter-labs/zksync-os-server/issues/1108)) ([0e23999](https://github.com/matter-labs/zksync-os-server/commit/0e23999e2bb05625152423b50211a095e61c3ca7))
* **network:** retry boot node DNS resolution before startup ([#1100](https://github.com/matter-labs/zksync-os-server/issues/1100)) ([4a635d5](https://github.com/matter-labs/zksync-os-server/commit/4a635d555eb173b110d7395ed884d6abe9e01b8f))
* **priority-tree:** run initialization in background to avoid shutdown bug ([#1067](https://github.com/matter-labs/zksync-os-server/issues/1067)) ([debea8f](https://github.com/matter-labs/zksync-os-server/commit/debea8fab1bac9e83337b684900cd57f71736173))
* **tracer:** map CREATE and CREATE2 correctly ([#1060](https://github.com/matter-labs/zksync-os-server/issues/1060)) ([553e627](https://github.com/matter-labs/zksync-os-server/commit/553e627241a8390eba5fbd7a3affb034df6a9316))

## [0.18.0](https://github.com/matter-labs/zksync-os-server/compare/v0.17.1...v0.18.0) (2026-03-24)


### ⚠ BREAKING CHANGES

* **network:** use chain-aware fork id for filtering discv5 peers ([#1051](https://github.com/matter-labs/zksync-os-server/issues/1051))

### Features

* Add set SL chain Id tx after upgrade ([#1047](https://github.com/matter-labs/zksync-os-server/issues/1047)) ([119e315](https://github.com/matter-labs/zksync-os-server/commit/119e315c02394e9b66638748ff2f082392120709))
* add trace logs to estimate gas with exec results ([#1044](https://github.com/matter-labs/zksync-os-server/issues/1044)) ([0bb4532](https://github.com/matter-labs/zksync-os-server/commit/0bb45329c4e428bc8d57dfc694d6ab1b8bee3ce2))
* consensus integration 2/5: Consensus interface, raft dependency ([#958](https://github.com/matter-labs/zksync-os-server/issues/958)) ([6e88dea](https://github.com/matter-labs/zksync-os-server/commit/6e88dead05f265abf167aceca3e6e84dbf8ecb8f))
* **minor:** small logging and test cleanups ([#1057](https://github.com/matter-labs/zksync-os-server/issues/1057)) ([df40c62](https://github.com/matter-labs/zksync-os-server/commit/df40c62a19a3380cac5aa5d4be4f87bb025e60c3))
* **multivm:** use in-memory app bins for PIG ([#1037](https://github.com/matter-labs/zksync-os-server/issues/1037)) ([49705f6](https://github.com/matter-labs/zksync-os-server/commit/49705f62fcbe7305e8512f0716f7bb2e2e7f7ebe))
* **network:** use chain-aware fork id for filtering discv5 peers ([#1051](https://github.com/matter-labs/zksync-os-server/issues/1051)) ([e9b3586](https://github.com/matter-labs/zksync-os-server/commit/e9b35864d2cbcc1a43f7cfab30aa00410375bbbf))
* **readctor `ReplayRecord`:** extract `BlockStartCursors` struct from flat cursor fields (eg `l1_priority_id`) ([#1034](https://github.com/matter-labs/zksync-os-server/issues/1034)) ([2b6ed46](https://github.com/matter-labs/zksync-os-server/commit/2b6ed46fb040cb44073f97f6e5d9374e936d63d4))
* **rpc:** add gatewayBlockNumber to zks_getL2ToL1LogProof response ([#1064](https://github.com/matter-labs/zksync-os-server/issues/1064)) ([daad643](https://github.com/matter-labs/zksync-os-server/commit/daad6431d1965347ad4966c0b740abd4e08c5dd6))
* **rpc:** Implement `zks_getProof` ([#917](https://github.com/matter-labs/zksync-os-server/issues/917)) ([4c6b676](https://github.com/matter-labs/zksync-os-server/commit/4c6b67642b3213a6e29b27f91aa77293694a2a0e))
* **rpc:** track JSON-RPC error counts by method and error code ([#1040](https://github.com/matter-labs/zksync-os-server/issues/1040)) ([ba5821a](https://github.com/matter-labs/zksync-os-server/commit/ba5821a6bd47abc396c30196f3af475d44fd37f3))
* Sync l1 state with draft-v31 ([#1010](https://github.com/matter-labs/zksync-os-server/issues/1010)) ([2c9fa7a](https://github.com/matter-labs/zksync-os-server/commit/2c9fa7a4c79797712fa85ed668e3165ea64d1eeb))
* **tx_validators:** add deployment filter to restrict contract deployments to an allow-list ([#1013](https://github.com/matter-labs/zksync-os-server/issues/1013)) ([f61b2ec](https://github.com/matter-labs/zksync-os-server/commit/f61b2ecc70ad91c6f666742ef57907949d0fadab))
* Use gateway base token as SL token ([#1042](https://github.com/matter-labs/zksync-os-server/issues/1042)) ([025df77](https://github.com/matter-labs/zksync-os-server/commit/025df77f99402548db4ac204ec5c156f19060be1))
* **zks_getProof:** add L1 verification data to proof response and CLI tool ([#1022](https://github.com/matter-labs/zksync-os-server/issues/1022)) ([fa34042](https://github.com/matter-labs/zksync-os-server/commit/fa34042da3139c5d08fbf5a1a32b8a90ba4c7b27))


### Bug Fixes

* get rid of default debug logs ([#939](https://github.com/matter-labs/zksync-os-server/issues/939)) ([bfb3bd3](https://github.com/matter-labs/zksync-os-server/commit/bfb3bd3de3aeb75deb2d66a3af04becde469cbf3))
* **l1_sender:** fix bug in `parallel_transactions` metric ([#996](https://github.com/matter-labs/zksync-os-server/issues/996)) ([3df0b64](https://github.com/matter-labs/zksync-os-server/commit/3df0b64424678b7cfc97ba97c11cddf1253cd08a))
* **rpc:** Fix `zks_getProof` ([#1032](https://github.com/matter-labs/zksync-os-server/issues/1032)) ([352b7db](https://github.com/matter-labs/zksync-os-server/commit/352b7db30dd7fc4fac717c49f0d43d88f9a80993))
* upgrade lz4_flex to 0.12.1 to address RUSTSEC-2026-0041 ([#1024](https://github.com/matter-labs/zksync-os-server/issues/1024)) ([22e1bee](https://github.com/matter-labs/zksync-os-server/commit/22e1bee73b34de5b99dfcb97986fac18e35ce2c4))

## [0.17.1](https://github.com/matter-labs/zksync-os-server/compare/v0.17.0...v0.17.1) (2026-03-16)


### Bug Fixes

* batch storage persist delay ([#1015](https://github.com/matter-labs/zksync-os-server/issues/1015)) ([ce075bc](https://github.com/matter-labs/zksync-os-server/commit/ce075bcf740ec5842fd4d2d2cdad5194c1333c70))

## [0.17.0](https://github.com/matter-labs/zksync-os-server/compare/v0.16.0...v0.17.0) (2026-03-16)


### ⚠ BREAKING CHANGES

* Remove unnecessary configs for EN ([#986](https://github.com/matter-labs/zksync-os-server/issues/986))
* Store FRI proofs locally, not in S3 ([#891](https://github.com/matter-labs/zksync-os-server/issues/891))
* Commit encoding v4 support ([#899](https://github.com/matter-labs/zksync-os-server/issues/899))

### Features

* add gateway interop fee updater ([#968](https://github.com/matter-labs/zksync-os-server/issues/968)) ([fe50e31](https://github.com/matter-labs/zksync-os-server/commit/fe50e31ba453f0aa24a192a71fabdd3ea6779f01))
* Add proper gateway migration watcher ([#921](https://github.com/matter-labs/zksync-os-server/issues/921)) ([c9e3622](https://github.com/matter-labs/zksync-os-server/commit/c9e36227614d63bf2d36ebd6d58c22436e8ffadf))
* Adding operator signing with HSM ([#956](https://github.com/matter-labs/zksync-os-server/issues/956)) ([5008730](https://github.com/matter-labs/zksync-os-server/commit/5008730299c23f6fb5c0bfcd4965620ba49b9e41))
* Bump zksync-os dev version ([#911](https://github.com/matter-labs/zksync-os-server/issues/911)) ([2bab2b8](https://github.com/matter-labs/zksync-os-server/commit/2bab2b8be287b882541d456a0cab26ab3407b336))
* Commit encoding v4 support ([#899](https://github.com/matter-labs/zksync-os-server/issues/899)) ([f95ddbd](https://github.com/matter-labs/zksync-os-server/commit/f95ddbdc837b035bc944009ae4dfce47c5579e9d))
* consensus integration 1/5: Sequencer split in BlockExecutor and BlockApplier ([#953](https://github.com/matter-labs/zksync-os-server/issues/953)) ([2f588c2](https://github.com/matter-labs/zksync-os-server/commit/2f588c2c43db788e8e2e27bed8ee38cbb75e1001))
* **genesis:** derive execution_version from protocol version, remove from genesis.json ([#940](https://github.com/matter-labs/zksync-os-server/issues/940)) ([38a77fa](https://github.com/matter-labs/zksync-os-server/commit/38a77facc7e3cfbb481ebf5e9710c2bb0338be3b))
* make operator signing keys optional for External Nodes ([#929](https://github.com/matter-labs/zksync-os-server/issues/929)) ([3894215](https://github.com/matter-labs/zksync-os-server/commit/38942150d7c9b8fe106ed5d3e9c136d435cae01c))
* **merkle-tree:** Implement storage proofs for `zks_getProof` ([#904](https://github.com/matter-labs/zksync-os-server/issues/904)) ([eaa38d3](https://github.com/matter-labs/zksync-os-server/commit/eaa38d36818b6d64a11565b6032745c4afa2df12))
* proper gateway settlement and local gateway setup ([#919](https://github.com/matter-labs/zksync-os-server/issues/919)) ([14b202f](https://github.com/matter-labs/zksync-os-server/commit/14b202f5c4a5bed26c308014e83cb00ed4a46bb4))
* **rpc:** Additional format of l2_to_l1_log_proof ([#964](https://github.com/matter-labs/zksync-os-server/issues/964)) ([6397e96](https://github.com/matter-labs/zksync-os-server/commit/6397e968b47260ff922640bbec59bd4c83d9ec33))
* scale eth_gasPrice by configurable factor ([#957](https://github.com/matter-labs/zksync-os-server/issues/957)) ([2240028](https://github.com/matter-labs/zksync-os-server/commit/2240028eee22c26a69082762767ae5595ce61bed))
* some gateway features ([#886](https://github.com/matter-labs/zksync-os-server/issues/886)) ([ba995d7](https://github.com/matter-labs/zksync-os-server/commit/ba995d72d24b41769439ab85966f667d8265294a))
* Store FRI proofs locally, not in S3 ([#891](https://github.com/matter-labs/zksync-os-server/issues/891)) ([2895b90](https://github.com/matter-labs/zksync-os-server/commit/2895b903e4f7c3929ec7f9f4e73944887543f475))
* update rustc version; use prover binary in test ([#901](https://github.com/matter-labs/zksync-os-server/issues/901)) ([2ca6c08](https://github.com/matter-labs/zksync-os-server/commit/2ca6c086a9974f97c1641cf75744af7099690d2c))


### Bug Fixes

* Add more metrics for 2FA ([#1001](https://github.com/matter-labs/zksync-os-server/issues/1001)) ([ccb9ce8](https://github.com/matter-labs/zksync-os-server/commit/ccb9ce80af4001978e9b66631831cfc2aa071b0a))
* Compare block hash during block replay ([#918](https://github.com/matter-labs/zksync-os-server/issues/918)) ([039a9ba](https://github.com/matter-labs/zksync-os-server/commit/039a9ba38e08c11ae4be8c80691988b640b3e3fc))
* Decouple v1 batch verification transport ([#997](https://github.com/matter-labs/zksync-os-server/issues/997)) ([53c09b5](https://github.com/matter-labs/zksync-os-server/commit/53c09b542b83c9d32d46f02a5a18c74f828410f9))
* do not do migration to set `execute_sl_block_number` for old batches ([#976](https://github.com/matter-labs/zksync-os-server/issues/976)) ([c4a00c7](https://github.com/matter-labs/zksync-os-server/commit/c4a00c76c0967d95e14b64ec45c21d220241f6dc))
* fix legacy batch processing in persist batch watcher ([#975](https://github.com/matter-labs/zksync-os-server/issues/975)) ([e1d07c7](https://github.com/matter-labs/zksync-os-server/commit/e1d07c71fd46777f019a258516fa71a6f84922b4))
* keep `StoredBatchInfo::last_block_timestamp` ([#977](https://github.com/matter-labs/zksync-os-server/issues/977)) ([73e5fe5](https://github.com/matter-labs/zksync-os-server/commit/73e5fe5c502f74e8c2932295b680c0611751d19d))
* mempool pending fee refresh ([#955](https://github.com/matter-labs/zksync-os-server/issues/955)) ([07693c8](https://github.com/matter-labs/zksync-os-server/commit/07693c84cdca821efc779dd2180c6cb438e2e850))
* multivm app path caching across tempdirs ([#948](https://github.com/matter-labs/zksync-os-server/issues/948)) ([69a457d](https://github.com/matter-labs/zksync-os-server/commit/69a457d8cc61c70c0e835ebdcbbc256e03e59efd))
* Remove unnecessary configs for EN ([#986](https://github.com/matter-labs/zksync-os-server/issues/986)) ([b68775b](https://github.com/matter-labs/zksync-os-server/commit/b68775b670ad5a67d54ff1f82b34d08465318986))
* rename aggregated root to multichain root ([#924](https://github.com/matter-labs/zksync-os-server/issues/924)) ([6cbc17b](https://github.com/matter-labs/zksync-os-server/commit/6cbc17b5886444284b12abc65640ba2fbe420a2d))
* retry on pending commit tx in L1 watcher instead of panicking ([#952](https://github.com/matter-labs/zksync-os-server/issues/952)) ([8589852](https://github.com/matter-labs/zksync-os-server/commit/8589852c24cd59235cb25e5748589896e942cc22))
* **rpc:** adjust latency histogram bucket range (1µs-32s) ([#990](https://github.com/matter-labs/zksync-os-server/issues/990)) ([10e4200](https://github.com/matter-labs/zksync-os-server/commit/10e4200960b3bd36b30addefc54b011e08a9ec03))
* **rpc:** camelCase `batchNumber` is L2-&gt;L1 log proof ([#923](https://github.com/matter-labs/zksync-os-server/issues/923)) ([9f8bcdd](https://github.com/matter-labs/zksync-os-server/commit/9f8bcdd472af299615b08ea8718f98b3a67a853f))
* **rpc:** lower eth_getLogs default limits to match industry standard ([#992](https://github.com/matter-labs/zksync-os-server/issues/992)) ([4f51503](https://github.com/matter-labs/zksync-os-server/commit/4f51503f353d89f87046cb0be21c005cd6e0a606))
* **sequencer:** handle low-fee L2 transactions without stalling block production ([#927](https://github.com/matter-labs/zksync-os-server/issues/927)) ([c0d7385](https://github.com/matter-labs/zksync-os-server/commit/c0d73850edc30223af6965bc86f98408c3da6f37))
* support 0x-prefixed hex in all config fields ([#931](https://github.com/matter-labs/zksync-os-server/issues/931)) ([a876bec](https://github.com/matter-labs/zksync-os-server/commit/a876becd96231a5a0b109f7ad5377310283482a1))
* **tests:** decompress L1 state in build.rs instead of in-process cache ([#966](https://github.com/matter-labs/zksync-os-server/issues/966)) ([03ae2ed](https://github.com/matter-labs/zksync-os-server/commit/03ae2ed7fb110ecac99bdfa9a893d89768a8992e))
* Use warn for server disconnects ([#998](https://github.com/matter-labs/zksync-os-server/issues/998)) ([266e134](https://github.com/matter-labs/zksync-os-server/commit/266e134cadf575680cabed60ebbfb2877beba244))
* Warn on batch verification threshold mismatch ([#984](https://github.com/matter-labs/zksync-os-server/issues/984)) ([ad0fcab](https://github.com/matter-labs/zksync-os-server/commit/ad0fcab716fc0485c17c842815fb497285127852))

## [0.16.0](https://github.com/matter-labs/zksync-os-server/compare/v0.15.1...v0.16.0) (2026-02-25)


### ⚠ BREAKING CHANGES

* **network:** fully migrate replay transport to p2p network ([#873](https://github.com/matter-labs/zksync-os-server/issues/873))
* change api l2 l1 log format ([#875](https://github.com/matter-labs/zksync-os-server/issues/875))

### Features

* add block hash to revm divergence panic message ([#880](https://github.com/matter-labs/zksync-os-server/issues/880)) ([92a9eaf](https://github.com/matter-labs/zksync-os-server/commit/92a9eafb6c5e89eb27f931a9e9892b99334323ac))
* **batch-verification:** make HTTPS connection a 2-way stream ([#862](https://github.com/matter-labs/zksync-os-server/issues/862)) ([a96e9a0](https://github.com/matter-labs/zksync-os-server/commit/a96e9a0974f7d13d86a3eaa9ab8ef03f9ebe5f29))
* change api l2 l1 log format ([#875](https://github.com/matter-labs/zksync-os-server/issues/875)) ([26ea56f](https://github.com/matter-labs/zksync-os-server/commit/26ea56f6e84febede0278995dbdd5c670c36eb88))
* index reverted blocks by hash ([#867](https://github.com/matter-labs/zksync-os-server/issues/867)) ([8e360fb](https://github.com/matter-labs/zksync-os-server/commit/8e360fb75f774e8acee65ef1c380308a4e7ece61))
* **mempool:** rewrite via in-memory subpools ([#869](https://github.com/matter-labs/zksync-os-server/issues/869)) ([b3bbca8](https://github.com/matter-labs/zksync-os-server/commit/b3bbca84481b624b959a00af98cb06f0af459927))
* **network:** bounded channel + shared starting block state ([#884](https://github.com/matter-labs/zksync-os-server/issues/884)) ([5de34e2](https://github.com/matter-labs/zksync-os-server/commit/5de34e26404e727ee0f2e92258714c12a6f73547))
* **network:** fully migrate replay transport to p2p network ([#873](https://github.com/matter-labs/zksync-os-server/issues/873)) ([a8e963a](https://github.com/matter-labs/zksync-os-server/commit/a8e963a00aa287a625eb57e63a628ec22101de10))


### Bug Fixes

* Apply fixes for cargo deny ([#892](https://github.com/matter-labs/zksync-os-server/issues/892)) ([e4eef3c](https://github.com/matter-labs/zksync-os-server/commit/e4eef3c99011aac6b7da6aeea8017d093292d0d5))
* Commit after each tx in revm consistency checker ([#898](https://github.com/matter-labs/zksync-os-server/issues/898)) ([384ff31](https://github.com/matter-labs/zksync-os-server/commit/384ff3134e2ba9dcaed035818845428a2c338647))
* get rid of broadcast in mempool ([#910](https://github.com/matter-labs/zksync-os-server/issues/910)) ([01b53fd](https://github.com/matter-labs/zksync-os-server/commit/01b53fd8dc2b23e7c38c9f4ec62e48d3643a76b8))
* remove transaction r and s paddings ([#890](https://github.com/matter-labs/zksync-os-server/issues/890)) ([3079e59](https://github.com/matter-labs/zksync-os-server/commit/3079e5968def203a09debd34f119dff79c04f700))
* **rpc:** return hex-encoded subscription ids ([#877](https://github.com/matter-labs/zksync-os-server/issues/877)) ([0dbc703](https://github.com/matter-labs/zksync-os-server/commit/0dbc703741dbd429960d9397e250581047016d5d))

## [0.15.1](https://github.com/matter-labs/zksync-os-server/compare/v0.15.0...v0.15.1) (2026-02-10)


### Bug Fixes

* **eth-watch:** don't save batches with divergent hashes ([#871](https://github.com/matter-labs/zksync-os-server/issues/871)) ([5254754](https://github.com/matter-labs/zksync-os-server/commit/52547541f8ea7f3db819bb5ea90f279ee4db6d5f))

## [0.15.0](https://github.com/matter-labs/zksync-os-server/compare/v0.14.2...v0.15.0) (2026-02-10)


### ⚠ BREAKING CHANGES

* drop proving support for v29.x and v30.0 versions ([#822](https://github.com/matter-labs/zksync-os-server/issues/822))

### Features

* Accumulated interop txs ([#848](https://github.com/matter-labs/zksync-os-server/issues/848)) ([feaeeea](https://github.com/matter-labs/zksync-os-server/commit/feaeeeaaaddb44be5521eaa8d1a4ab829ea43bbd))
* drop proving support for v29.x and v30.0 versions ([#822](https://github.com/matter-labs/zksync-os-server/issues/822)) ([f157dbb](https://github.com/matter-labs/zksync-os-server/commit/f157dbbdf30a49b68ccfc60c555a62732ed6cb9a))
* **multivm:** use v0.2.6-simulate-only for V5 simulation ([#855](https://github.com/matter-labs/zksync-os-server/issues/855)) ([c21a107](https://github.com/matter-labs/zksync-os-server/commit/c21a107f4b344e02d8d799d81c8472769d7d67cc))
* Set SL chain id txs ([#849](https://github.com/matter-labs/zksync-os-server/issues/849)) ([f561a9e](https://github.com/matter-labs/zksync-os-server/commit/f561a9e0feb1cf5b4d8036f05d2d3f574915d6be))
* store gzip-compressed anvil states ([#837](https://github.com/matter-labs/zksync-os-server/issues/837)) ([d231609](https://github.com/matter-labs/zksync-os-server/commit/d231609035533db253bb09b6002197286ff2a8e0))
* support multiple config files ([#866](https://github.com/matter-labs/zksync-os-server/issues/866)) ([319b2f9](https://github.com/matter-labs/zksync-os-server/commit/319b2f9b311c23e1292e7880b3c8e41fabf686e5))
* use max_priority_fee_per_gas config value as cap on the priority fee used ([#857](https://github.com/matter-labs/zksync-os-server/issues/857)) ([2331595](https://github.com/matter-labs/zksync-os-server/commit/233159524f069410e23c9059cac84649a00ace8f))


### Bug Fixes

* better recognition for missing `IMultisigCommitter` ([#852](https://github.com/matter-labs/zksync-os-server/issues/852)) ([9e07c51](https://github.com/matter-labs/zksync-os-server/commit/9e07c518bbc2b0bbc5ba7e6e52703b911879adb2))
* **l1-watcher:** skip persisting legacy batches ([#860](https://github.com/matter-labs/zksync-os-server/issues/860)) ([9d818fd](https://github.com/matter-labs/zksync-os-server/commit/9d818fd223183cddf8b51bf3b4cc08693961bf9d))
* rebuild_from_block assert for EN ([#864](https://github.com/matter-labs/zksync-os-server/issues/864)) ([fa2c6c6](https://github.com/matter-labs/zksync-os-server/commit/fa2c6c64b8c6a122b64ae462f6197d1169f42bda))
* **rpc:** respect 0 gas price during gas estimation ([#865](https://github.com/matter-labs/zksync-os-server/issues/865)) ([ed80197](https://github.com/matter-labs/zksync-os-server/commit/ed80197dc1d5d826064dfb37550127a38d04114e))
* Update time crate to 0.3.47 to address security vulnerability ([#870](https://github.com/matter-labs/zksync-os-server/issues/870)) ([82a0537](https://github.com/matter-labs/zksync-os-server/commit/82a05377bb5929956ccea8d4f5ed76decb31f449))

## [0.14.2](https://github.com/matter-labs/zksync-os-server/compare/v0.14.1...v0.14.2) (2026-01-29)


### Features

* add metric for base fee and native price ([#844](https://github.com/matter-labs/zksync-os-server/issues/844)) ([3aa0b70](https://github.com/matter-labs/zksync-os-server/commit/3aa0b709068d0b01a585e3639346cb152d818da9))
* add pubdata price cap ([#842](https://github.com/matter-labs/zksync-os-server/issues/842)) ([9d9803d](https://github.com/matter-labs/zksync-os-server/commit/9d9803d94a20372e13966d0985e0e637a05b389a))
* do not require S3 for RPC ([#827](https://github.com/matter-labs/zksync-os-server/issues/827)) ([a923d83](https://github.com/matter-labs/zksync-os-server/commit/a923d833f876063f02b7c6cddffe350692b1180f))
* validate genesis batch info against L1 ([#832](https://github.com/matter-labs/zksync-os-server/issues/832)) ([affbc1f](https://github.com/matter-labs/zksync-os-server/commit/affbc1f31deafe35d39955320bd5ab2aef970ae8))


### Bug Fixes

* increase default value for `estimate_gas_pubdata_price_factor` ([#831](https://github.com/matter-labs/zksync-os-server/issues/831)) ([6180db3](https://github.com/matter-labs/zksync-os-server/commit/6180db314fbfca08fbab6e7c9f4f33eeb71b22bc))

## [0.14.1](https://github.com/matter-labs/zksync-os-server/compare/v0.14.0...v0.14.1) (2026-01-27)


### Features

* Add metric for blacklisted addresses count ([#820](https://github.com/matter-labs/zksync-os-server/issues/820)) ([078368a](https://github.com/matter-labs/zksync-os-server/commit/078368a4e80ea3ac3f2d0418a3a99bc901cc5f00))
* do not require batch storage for priority tree ([#825](https://github.com/matter-labs/zksync-os-server/issues/825)) ([6a73d20](https://github.com/matter-labs/zksync-os-server/commit/6a73d2031567d9a2c281d807164bd3637d7184b0))


### Bug Fixes

* **rpc:** revert "make `eth_estimateGas` work when sender has no balance ([#807](https://github.com/matter-labs/zksync-os-server/issues/807))" ([#826](https://github.com/matter-labs/zksync-os-server/issues/826)) ([e1018d6](https://github.com/matter-labs/zksync-os-server/commit/e1018d6da7031bf07aa030f14ff7f0d0d0344b70))


### Performance Improvements

* speed up priority tree init for EN ([#824](https://github.com/matter-labs/zksync-os-server/issues/824)) ([5e1b951](https://github.com/matter-labs/zksync-os-server/commit/5e1b95127f4c272d14989b5763ab8bd28a400ec2))

## [0.14.0](https://github.com/matter-labs/zksync-os-server/compare/v0.13.0...v0.14.0) (2026-01-23)


### ⚠ BREAKING CHANGES

* Execution of service interop transactions ([#803](https://github.com/matter-labs/zksync-os-server/issues/803))
* use token prices in fee model ([#787](https://github.com/matter-labs/zksync-os-server/issues/787))
* token price updater component ([#779](https://github.com/matter-labs/zksync-os-server/issues/779))
* Basic V31 Support ([#759](https://github.com/matter-labs/zksync-os-server/issues/759))

### Features

* 2FA L1 integration ([#726](https://github.com/matter-labs/zksync-os-server/issues/726)) ([43a466f](https://github.com/matter-labs/zksync-os-server/commit/43a466fd341532bdbdc79642e74b86639aad7b6a))
* add bash script to run local chains ([#777](https://github.com/matter-labs/zksync-os-server/issues/777)) ([b786ad8](https://github.com/matter-labs/zksync-os-server/commit/b786ad8ef27a728c6394eb286aa00e48b061f4ea))
* add more eth-sender metrics. Bump fee limit. ([#789](https://github.com/matter-labs/zksync-os-server/issues/789)) ([6b6f13b](https://github.com/matter-labs/zksync-os-server/commit/6b6f13b648739e92bc7f2356b1e2b67dca1da87e))
* add support for YAML config files ([#785](https://github.com/matter-labs/zksync-os-server/issues/785)) ([5f3de80](https://github.com/matter-labs/zksync-os-server/commit/5f3de80747df543202496e737b37ec528bf2b3bb))
* add toHex helper for JS tracer ([#761](https://github.com/matter-labs/zksync-os-server/issues/761)) ([f9e14aa](https://github.com/matter-labs/zksync-os-server/commit/f9e14aa0ccc49c53d052b9425294eeb1d8776453))
* adjust pubdata price based on blob fill ratio ([#700](https://github.com/matter-labs/zksync-os-server/issues/700)) ([a8e6de4](https://github.com/matter-labs/zksync-os-server/commit/a8e6de4f4f260ab33bb2ac57c441c0bec4a8fb2c))
* adjust pubdata price based on blob fill ratio (2nd attempt) ([#756](https://github.com/matter-labs/zksync-os-server/issues/756)) ([167d874](https://github.com/matter-labs/zksync-os-server/commit/167d874bfd4e5e4870ba85405a2a1fbdfd22ac5c))
* Basic V31 Support ([#759](https://github.com/matter-labs/zksync-os-server/issues/759)) ([1103ab8](https://github.com/matter-labs/zksync-os-server/commit/1103ab882b6e7ccc94db08375cb2049cb142e5e5))
* **batcher:** make the limit of transaction count per batch configurable ([#796](https://github.com/matter-labs/zksync-os-server/issues/796)) ([f09de09](https://github.com/matter-labs/zksync-os-server/commit/f09de09e30f586244967e94bb74bd24a0dac76e9))
* **deposit tool:** Make it work with https provider; use ether as unit ([#794](https://github.com/matter-labs/zksync-os-server/issues/794)) ([c6b7839](https://github.com/matter-labs/zksync-os-server/commit/c6b78399be1c4b308d333a85963379b455064169))
* do not require batch storage (S3) for ENs ([#810](https://github.com/matter-labs/zksync-os-server/issues/810)) ([d542f07](https://github.com/matter-labs/zksync-os-server/commit/d542f0777f53c5df5591139448da864a77bc1763))
* Execution of service interop transactions ([#803](https://github.com/matter-labs/zksync-os-server/issues/803)) ([20f5ed2](https://github.com/matter-labs/zksync-os-server/commit/20f5ed296c4913a9cd9964f34ce545eec18fce8d))
* ignore vulnerability to recover cargo-audit ([#754](https://github.com/matter-labs/zksync-os-server/issues/754)) ([309887e](https://github.com/matter-labs/zksync-os-server/commit/309887efe8ed2355d802a60319f72a6d1d5b22cc))
* Implement interop system transaction ([#712](https://github.com/matter-labs/zksync-os-server/issues/712)) ([0310dbc](https://github.com/matter-labs/zksync-os-server/commit/0310dbc504b254596f088a3f66c7206104293981))
* Interop roots watcher ([#819](https://github.com/matter-labs/zksync-os-server/issues/819)) ([66c8fc5](https://github.com/matter-labs/zksync-os-server/commit/66c8fc5f41933ad074c45f3a65a43265a61abf72))
* introduce `CommittedBatchProvider` ([#764](https://github.com/matter-labs/zksync-os-server/issues/764)) ([d3a1cf4](https://github.com/matter-labs/zksync-os-server/commit/d3a1cf4859186a024e54191a061dbeecd16fb864))
* make block-related logging consistent ([#792](https://github.com/matter-labs/zksync-os-server/issues/792)) ([485c13c](https://github.com/matter-labs/zksync-os-server/commit/485c13cd21a9e2dc385e7de9efd01e8c54e2888b))
* more granular buckets for `prove_time_per_million_native` ([#763](https://github.com/matter-labs/zksync-os-server/issues/763)) ([4e0fe7d](https://github.com/matter-labs/zksync-os-server/commit/4e0fe7dbb269981600e63eacfec00147639a0dc9))
* **network:** add runnable `NetworkService` (disabled by default) ([#773](https://github.com/matter-labs/zksync-os-server/issues/773)) ([88fdf39](https://github.com/matter-labs/zksync-os-server/commit/88fdf39a42cf054a92193badf0443b97a33bba6e))
* **network:** implement bare-bones `zks` RLPx subprotocol ([#716](https://github.com/matter-labs/zksync-os-server/issues/716)) ([417c6ad](https://github.com/matter-labs/zksync-os-server/commit/417c6ad00d73f5e4add37f4db08d0bc4e2699eeb))
* record prove time per native ([#757](https://github.com/matter-labs/zksync-os-server/issues/757)) ([63fd801](https://github.com/matter-labs/zksync-os-server/commit/63fd801284a53da3bdff08ecf2f1ddf2053eb6bc))
* remove hardcoded config constants ([#762](https://github.com/matter-labs/zksync-os-server/issues/762)) ([adfc998](https://github.com/matter-labs/zksync-os-server/commit/adfc99875228a0bd3cd8945504355d8fe6dcf478))
* return zeroes in `reward` in `eth_feeHistory` ([#800](https://github.com/matter-labs/zksync-os-server/issues/800)) ([8f09ae7](https://github.com/matter-labs/zksync-os-server/commit/8f09ae7c89a409ceb4fa7fc2eef2da19385441eb))
* Revert "feat: adjust pubdata price based on blob fill ratio" ([#753](https://github.com/matter-labs/zksync-os-server/issues/753)) ([d7a7f54](https://github.com/matter-labs/zksync-os-server/commit/d7a7f54141b9db61773cba6235409f8aa7fdf347))
* set total difficulty in rpc block headers ([#801](https://github.com/matter-labs/zksync-os-server/issues/801)) ([6dac957](https://github.com/matter-labs/zksync-os-server/commit/6dac957fc826d89485a0e5f1eb26b91a1c2121c2))
* support JSON config files ([#752](https://github.com/matter-labs/zksync-os-server/issues/752)) ([f94d846](https://github.com/matter-labs/zksync-os-server/commit/f94d8463ef726f0c5fd8e68ba5ec564147120ae8))
* token price updater component ([#779](https://github.com/matter-labs/zksync-os-server/issues/779)) ([863b909](https://github.com/matter-labs/zksync-os-server/commit/863b909a8d85e11727927618c037df1cfdb6db4c))
* use newer version of zkyns-os-revm ([#798](https://github.com/matter-labs/zksync-os-server/issues/798)) ([aa97f62](https://github.com/matter-labs/zksync-os-server/commit/aa97f627874b5fdd446cfe35aecfb537ee17226b))
* use token prices in fee model ([#787](https://github.com/matter-labs/zksync-os-server/issues/787)) ([1f2375f](https://github.com/matter-labs/zksync-os-server/commit/1f2375f50e370234785a1b792c28f21056ee05db))


### Bug Fixes

* `zksync_os_types` compiles without features ([#815](https://github.com/matter-labs/zksync-os-server/issues/815)) ([b7dbe66](https://github.com/matter-labs/zksync-os-server/commit/b7dbe661e705af617399228c1da6039d7b4671b0))
* construct pending block context in `eth_call`-like methods ([#758](https://github.com/matter-labs/zksync-os-server/issues/758)) ([1e1086a](https://github.com/matter-labs/zksync-os-server/commit/1e1086af9e8a2c4449653958bae0601608e1c693))
* local chain config file is required to start the node ([#771](https://github.com/matter-labs/zksync-os-server/issues/771)) ([4597cae](https://github.com/matter-labs/zksync-os-server/commit/4597cae68267e67159c0340f9c6ff9cf8853dcc8))
* prevent "subtract with overflow" error on EN startup  ([#802](https://github.com/matter-labs/zksync-os-server/issues/802)) ([0678f56](https://github.com/matter-labs/zksync-os-server/commit/0678f56fdbd53daa8d4defc70924c700b78da883))
* refactor local-chains structure and update with anvil 1.5.1 ([#776](https://github.com/matter-labs/zksync-os-server/issues/776)) ([24d3852](https://github.com/matter-labs/zksync-os-server/commit/24d38529dbeb2f0bdd016005b1e5e0bc491b692f))
* rename sandbox to ephemeral ([#778](https://github.com/matter-labs/zksync-os-server/issues/778)) ([16f6bad](https://github.com/matter-labs/zksync-os-server/commit/16f6bad391fc502041750c6e6e3e1d854bf6099a))
* **rpc:** make `eth_estimateGas` work when sender has no balance ([#807](https://github.com/matter-labs/zksync-os-server/issues/807)) ([4ce1018](https://github.com/matter-labs/zksync-os-server/commit/4ce1018e436063ba8d480fd3a5cdb19d6022ac72))
* run RPC/status components later in the flow ([#817](https://github.com/matter-labs/zksync-os-server/issues/817)) ([387999e](https://github.com/matter-labs/zksync-os-server/commit/387999e8d65e19ef9eec2634b5c6e4af2a7b3929))

## [0.13.0](https://github.com/matter-labs/zksync-os-server/compare/v0.12.1...v0.13.0) (2025-12-22)


### ⚠ BREAKING CHANGES

* protocol upgrade v0.30.1 (zksync-os v0.2.5) ([#743](https://github.com/matter-labs/zksync-os-server/issues/743))
* **network:** use real HTTP server/client for batch verification ([#737](https://github.com/matter-labs/zksync-os-server/issues/737))
* **network:** use real HTTP server/client for replay transport ([#729](https://github.com/matter-labs/zksync-os-server/issues/729))

### Features

* add sequencer ephemeral mode ([#730](https://github.com/matter-labs/zksync-os-server/issues/730)) ([b55cdcd](https://github.com/matter-labs/zksync-os-server/commit/b55cdcd652e6ba8a70e82aa451fbddfc597b9aa8))
* config option to disable priority tree ([#738](https://github.com/matter-labs/zksync-os-server/issues/738)) ([36fbd35](https://github.com/matter-labs/zksync-os-server/commit/36fbd3536a28d231fb1fb5899cd46e9268d23d33))
* **config:** make mempool tx_fee_cap configurable ([#717](https://github.com/matter-labs/zksync-os-server/issues/717)) ([4548357](https://github.com/matter-labs/zksync-os-server/commit/4548357ee2d9e4a9da6709d3f301f8ff7dd80499))
* make bytecode supplier address config value optional ([#735](https://github.com/matter-labs/zksync-os-server/issues/735)) ([1e6f363](https://github.com/matter-labs/zksync-os-server/commit/1e6f363db7dae74bbf923a052498ce353018bacf))
* **network:** use real HTTP server/client for batch verification ([#737](https://github.com/matter-labs/zksync-os-server/issues/737)) ([d4aca72](https://github.com/matter-labs/zksync-os-server/commit/d4aca725a7fe7ba86d9a2df3010cc6bc440f7563))
* **network:** use real HTTP server/client for replay transport ([#729](https://github.com/matter-labs/zksync-os-server/issues/729)) ([5537d28](https://github.com/matter-labs/zksync-os-server/commit/5537d2888aa62fc41e772607b203de9af1b572aa))
* protocol upgrade v0.30.1 (zksync-os v0.2.5) ([#743](https://github.com/matter-labs/zksync-os-server/issues/743)) ([2cd6a6e](https://github.com/matter-labs/zksync-os-server/commit/2cd6a6ef8dfe7eb94a1fd54539753b791c7c460b))
* **rpc:** Add zks_getBlockMetadataByNumber ([#724](https://github.com/matter-labs/zksync-os-server/issues/724)) ([184c4bd](https://github.com/matter-labs/zksync-os-server/commit/184c4bd32e49b8717ed51132be5f1c067d115f20))
* **tracer:** Add error message for out-of-native ([#720](https://github.com/matter-labs/zksync-os-server/issues/720)) ([79d035f](https://github.com/matter-labs/zksync-os-server/commit/79d035f9007bf867fe8518d0995cfd939f9e4532))


### Bug Fixes

* don't require genesis_chain_id for ENs ([#734](https://github.com/matter-labs/zksync-os-server/issues/734)) ([95c0512](https://github.com/matter-labs/zksync-os-server/commit/95c051267f74b281c10669277852788053c5cfc2))
* **l1-watcher:** pick the most recent upgrade cut ([#742](https://github.com/matter-labs/zksync-os-server/issues/742)) ([f86e558](https://github.com/matter-labs/zksync-os-server/commit/f86e558e6ed298439e60f7f7ab718d32efc31f55))
* Replace DashMap with RwLock and HashMap ([#722](https://github.com/matter-labs/zksync-os-server/issues/722)) ([a6e658e](https://github.com/matter-labs/zksync-os-server/commit/a6e658e9f4a9748170cc49cd7b186de76d521c70))
* revm-consistency-checker legacy pre-eip155 transactions ([#740](https://github.com/matter-labs/zksync-os-server/issues/740)) ([b2bd059](https://github.com/matter-labs/zksync-os-server/commit/b2bd05917beae97081e4bf0d8e32be508eabf3f1))
* **tracer:** Fix call tracer behavior for 'empty' transactions ([#718](https://github.com/matter-labs/zksync-os-server/issues/718)) ([81b5e82](https://github.com/matter-labs/zksync-os-server/commit/81b5e82b406041823257dc5f3eb94614e6e1f437))
* **tracer:** Fix handling of errors in subcalls ([#719](https://github.com/matter-labs/zksync-os-server/issues/719)) ([1af589d](https://github.com/matter-labs/zksync-os-server/commit/1af589dd8b53cadb481c75e5305b97b971510d3d))
* Update revm to v0.0.2 ([#732](https://github.com/matter-labs/zksync-os-server/issues/732)) ([e502499](https://github.com/matter-labs/zksync-os-server/commit/e502499c9d8b33decf2456ad67ea3961c9df7644))

## [0.12.1](https://github.com/matter-labs/zksync-os-server/compare/v0.12.0...v0.12.1) (2025-12-11)


### Features

* **batcher:** re-create batches using L1 watcher's data ([#672](https://github.com/matter-labs/zksync-os-server/issues/672)) ([11fefc4](https://github.com/matter-labs/zksync-os-server/commit/11fefc41c7c55f88b40ecab5e31464ef1e68e8e4))
* blob computation overhead for pubdata price ([#693](https://github.com/matter-labs/zksync-os-server/issues/693)) ([bf69d65](https://github.com/matter-labs/zksync-os-server/commit/bf69d65f29b0a6bf4a38093a6f26fea1dad97167))
* **config:** Add config command ([#697](https://github.com/matter-labs/zksync-os-server/issues/697)) ([cd8a611](https://github.com/matter-labs/zksync-os-server/commit/cd8a61186406aaf940510ce948aefd21fc1a6c22))
* **config:** use EtherAmount for fee-related configs ([#676](https://github.com/matter-labs/zksync-os-server/issues/676)) ([28c27b1](https://github.com/matter-labs/zksync-os-server/commit/28c27b1a215898bcd7aa27437bcce641eb88636c))
* Don't report Passthrough in batch_number metrics ([#683](https://github.com/matter-labs/zksync-os-server/issues/683)) ([7719fb3](https://github.com/matter-labs/zksync-os-server/commit/7719fb34a6047e98596b26ffcb2abc12917a97e0))
* JS tracer ([#569](https://github.com/matter-labs/zksync-os-server/issues/569)) ([c991043](https://github.com/matter-labs/zksync-os-server/commit/c99104389a790f29237fe7c880d01d67c9319032))
* remove failed transcations from block_output.tx_results ([#714](https://github.com/matter-labs/zksync-os-server/issues/714)) ([23b5323](https://github.com/matter-labs/zksync-os-server/commit/23b5323d0ce6911ded4bb5566b0f93fcc61f696a))
* upgrade reth to 1.9.3/revm to 31.0.2 ([#709](https://github.com/matter-labs/zksync-os-server/issues/709)) ([521d473](https://github.com/matter-labs/zksync-os-server/commit/521d473854423e01dbf011efda04f007e9156e7a))


### Bug Fixes

* **l1-watcher:** handle L1 reverts during state recovery ([#692](https://github.com/matter-labs/zksync-os-server/issues/692)) ([d915174](https://github.com/matter-labs/zksync-os-server/commit/d9151748ada061800611eac8e89a6843c2c57875))
* **rpc:** move executed block check earlier in `zks_getL2ToL1LogProof` ([#704](https://github.com/matter-labs/zksync-os-server/issues/704)) ([117faa8](https://github.com/matter-labs/zksync-os-server/commit/117faa85db69889ff76bffe781fa4ed754d2a6e7))
* state tracking for sequencer ([#715](https://github.com/matter-labs/zksync-os-server/issues/715)) ([01c3a6b](https://github.com/matter-labs/zksync-os-server/commit/01c3a6bb93795a9ce3542e32d59d3a1c53ed55ff))
* upgrade issues in block context provider ([#666](https://github.com/matter-labs/zksync-os-server/issues/666)) ([e80cb85](https://github.com/matter-labs/zksync-os-server/commit/e80cb8539e5a986516a8b01e7a1d0aaa9ec1e9ac))

## [0.12.0](https://github.com/matter-labs/zksync-os-server/compare/v0.11.1...v0.12.0) (2025-11-28)


### ⚠ BREAKING CHANGES

* allow EN to sync with overriden records ([#657](https://github.com/matter-labs/zksync-os-server/issues/657))
* Remove deprecated legacy prover API ([#674](https://github.com/matter-labs/zksync-os-server/issues/674))

### Features

* add internal config; use it in revm checker ([#608](https://github.com/matter-labs/zksync-os-server/issues/608)) ([13e6d18](https://github.com/matter-labs/zksync-os-server/commit/13e6d18ca67561e1c8789b91a0dadc31bd5ab781))
* allow EN to sync with overriden records ([#657](https://github.com/matter-labs/zksync-os-server/issues/657)) ([9422a14](https://github.com/matter-labs/zksync-os-server/commit/9422a1482d82a87f25a9d3f5344299cde9821da0))
* **db:** keep overwritten replay records ([#620](https://github.com/matter-labs/zksync-os-server/issues/620)) ([35bdab6](https://github.com/matter-labs/zksync-os-server/commit/35bdab69403d20b67a87555f81e2593f3bdd14e4))
* **l1-sender:** send EIP-7594 blobs when Fusaka is activated ([#664](https://github.com/matter-labs/zksync-os-server/issues/664)) ([0b41a19](https://github.com/matter-labs/zksync-os-server/commit/0b41a194157a84bb3ee6c2ab1c750e34847c9529))
* **l1-watcher:** monitor `ReportCommittedBatchRangeZKsyncOS` events ([#661](https://github.com/matter-labs/zksync-os-server/issues/661)) ([f21e876](https://github.com/matter-labs/zksync-os-server/commit/f21e876456a04458fbf54f43da4bf87058cb6d20))
* **mempool-config:** make minimal_protocol_basefee configurable ([#671](https://github.com/matter-labs/zksync-os-server/issues/671)) ([9a65250](https://github.com/matter-labs/zksync-os-server/commit/9a65250ffdb8dd22a2cb17362ea4bbaf08ba83b3))
* Remove deprecated legacy prover API ([#674](https://github.com/matter-labs/zksync-os-server/issues/674)) ([728c177](https://github.com/matter-labs/zksync-os-server/commit/728c177dc488198cf886907e2afd279fc5a891be))
* **rpc:** use pubdata price factor during gas estimation ([#669](https://github.com/matter-labs/zksync-os-server/issues/669)) ([8dd8377](https://github.com/matter-labs/zksync-os-server/commit/8dd8377ea88ff41244ed57ae131348475333d16d))
* support multiple SNARKers; enhance proving observability ([#631](https://github.com/matter-labs/zksync-os-server/issues/631)) ([8541de8](https://github.com/matter-labs/zksync-os-server/commit/8541de8ac81bd3f26b595733148221f47570dce9))


### Bug Fixes

* 2FA followup ([#662](https://github.com/matter-labs/zksync-os-server/issues/662)) ([954b322](https://github.com/matter-labs/zksync-os-server/commit/954b322b60a6b919f6b655765f4447a0b324f3fa))
* batch verification config ([#654](https://github.com/matter-labs/zksync-os-server/issues/654)) ([941edbd](https://github.com/matter-labs/zksync-os-server/commit/941edbd64912a02320dcf7132f0357ffa052890c))
* **en:** handle missing blocks on main node ([#677](https://github.com/matter-labs/zksync-os-server/issues/677)) ([d7e2291](https://github.com/matter-labs/zksync-os-server/commit/d7e2291e923214266aa87fd51b4ba616d35d0b6e))
* Sealing empty blocks ([#653](https://github.com/matter-labs/zksync-os-server/issues/653)) ([fcb43d8](https://github.com/matter-labs/zksync-os-server/commit/fcb43d8072d00a576006d324d727d5ea9a1533cf))

## [0.11.1](https://github.com/matter-labs/zksync-os-server/compare/v0.11.0...v0.11.1) (2025-11-24)


### Features

* Add time_since metrics ([#628](https://github.com/matter-labs/zksync-os-server/issues/628)) ([33a7224](https://github.com/matter-labs/zksync-os-server/commit/33a722440f5399f74b8f80b95d9386f285c16c5e))
* config option to disable batcher hash assertion when rebuilding batches ([#647](https://github.com/matter-labs/zksync-os-server/issues/647)) ([34d45e1](https://github.com/matter-labs/zksync-os-server/commit/34d45e1f3b1420664c6a0e1f4367a47e7d10e27c))
* update zksync-os with p256 fix ([#642](https://github.com/matter-labs/zksync-os-server/issues/642)) ([ea04463](https://github.com/matter-labs/zksync-os-server/commit/ea044637adb94336999d0e5031dd61c007defc11))
* upgrade smart-config to 0.4.0; simplify parsing ([#644](https://github.com/matter-labs/zksync-os-server/issues/644)) ([a0c1da9](https://github.com/matter-labs/zksync-os-server/commit/a0c1da9fea1312d46be0f6594d55787ea3ae45dc))


### Bug Fixes

* **batcher:** rebuild batches from S3 even when they are not committed ([#645](https://github.com/matter-labs/zksync-os-server/issues/645)) ([608153d](https://github.com/matter-labs/zksync-os-server/commit/608153d83dee7d37d03c9e53120a496454658df5))
* Update ZKsync REVM deps ([#648](https://github.com/matter-labs/zksync-os-server/issues/648)) ([d66af50](https://github.com/matter-labs/zksync-os-server/commit/d66af5089b5f616da1387d05c7efa480ba5d0b92))

## [0.11.0](https://github.com/matter-labs/zksync-os-server/compare/v0.10.1...v0.11.0) (2025-11-20)


### ⚠ BREAKING CHANGES

* v30 zksync os protocol upgrade support ([#594](https://github.com/matter-labs/zksync-os-server/issues/594))
* upgrade system (part 1 of N) ([#582](https://github.com/matter-labs/zksync-os-server/issues/582))

### Features

* add config for l2 signer blacklist ([#596](https://github.com/matter-labs/zksync-os-server/issues/596)) ([bc30cc9](https://github.com/matter-labs/zksync-os-server/commit/bc30cc967ed79119158ce90f6f0c4b93561f17a2))
* add some prover metrics ([#611](https://github.com/matter-labs/zksync-os-server/issues/611)) ([b2483cf](https://github.com/matter-labs/zksync-os-server/commit/b2483cf3c2d36b49e2c9b078d30f30cd94397cb5))
* **api:** forward EN transactions to main node ([#624](https://github.com/matter-labs/zksync-os-server/issues/624)) ([9a7583c](https://github.com/matter-labs/zksync-os-server/commit/9a7583c87b6e46a13a2cfc69a3796d95cfafa69f))
* **api:** implement EIP-7966 eth_sendRawTransactionSync ([#621](https://github.com/matter-labs/zksync-os-server/issues/621)) ([0fbf615](https://github.com/matter-labs/zksync-os-server/commit/0fbf615a3d4d99ea4c85296ea8ed0e8e1203c52a))
* handle reorgs for EN ([#610](https://github.com/matter-labs/zksync-os-server/issues/610)) ([055136d](https://github.com/matter-labs/zksync-os-server/commit/055136d8f5ce8a41048e0be48437e2bf04c16fac))
* **l1_watcher:** Make l1 watcher processor-agnostic ([#634](https://github.com/matter-labs/zksync-os-server/issues/634)) ([a3fe619](https://github.com/matter-labs/zksync-os-server/commit/a3fe6198be7ec4abd3ef6b2fd8af6337035e0a60))
* Read force deploys from a file ([#612](https://github.com/matter-labs/zksync-os-server/issues/612)) ([b90473a](https://github.com/matter-labs/zksync-os-server/commit/b90473ad45676c307510d84cb64464bf4c728b97))
* upgrade system (part 1 of N) ([#582](https://github.com/matter-labs/zksync-os-server/issues/582)) ([4de5e84](https://github.com/matter-labs/zksync-os-server/commit/4de5e841a3fce8eadcfba2c4cb430de022d20d25))
* upgrade system (part 2 of N) ([#609](https://github.com/matter-labs/zksync-os-server/issues/609)) ([b9a303d](https://github.com/matter-labs/zksync-os-server/commit/b9a303d58adea7a9d8558e374bb28f5944a244f9))
* v30 zksync os protocol upgrade support ([#594](https://github.com/matter-labs/zksync-os-server/issues/594)) ([c8698a6](https://github.com/matter-labs/zksync-os-server/commit/c8698a683546e29a6e9e2fc58cac4371bbb4c80c))


### Bug Fixes

* **config:** add config attributes to fee overrides ([#603](https://github.com/matter-labs/zksync-os-server/issues/603)) ([5539e91](https://github.com/matter-labs/zksync-os-server/commit/5539e918cbfbdb3ad292c442364f04f56d5375bf))
* fix calculation of da fields for validium v4 ([#636](https://github.com/matter-labs/zksync-os-server/issues/636)) ([72282d2](https://github.com/matter-labs/zksync-os-server/commit/72282d25f64b22d18c791f540438bd457c97cb37))
* move BlacklistedSigner error to different enum ([#605](https://github.com/matter-labs/zksync-os-server/issues/605)) ([fd9f1bd](https://github.com/matter-labs/zksync-os-server/commit/fd9f1bdabd1d7247ae381df8da8cc40b38646dd3))
* upgrade issues ([#638](https://github.com/matter-labs/zksync-os-server/issues/638)) ([15697bb](https://github.com/matter-labs/zksync-os-server/commit/15697bb7ec837a06308254e13acae64a2560f224))
* upgrade issues second part ([#639](https://github.com/matter-labs/zksync-os-server/issues/639)) ([a06bb32](https://github.com/matter-labs/zksync-os-server/commit/a06bb32a0ba71978171e16b8a4a5b15b7838f750))

## [0.10.1](https://github.com/matter-labs/zksync-os-server/compare/v0.10.0...v0.10.1) (2025-11-12)


### Features

* Add REVM support of multiple execution versions ([#597](https://github.com/matter-labs/zksync-os-server/issues/597)) ([cccdba0](https://github.com/matter-labs/zksync-os-server/commit/cccdba0d7e88878438191079326463c9760c0aa4))
* set default block time to 250ms ([#598](https://github.com/matter-labs/zksync-os-server/issues/598)) ([3f7c724](https://github.com/matter-labs/zksync-os-server/commit/3f7c724eb671a873064548293f70dff8a6290cb0))
* set sensible global debug levels ([#600](https://github.com/matter-labs/zksync-os-server/issues/600)) ([5e2cdcf](https://github.com/matter-labs/zksync-os-server/commit/5e2cdcfd46ca6fc0f76c6fc36e393dcc003854f5))


### Bug Fixes

* register misc mempool metrics ([#599](https://github.com/matter-labs/zksync-os-server/issues/599)) ([02164b0](https://github.com/matter-labs/zksync-os-server/commit/02164b05fa753e051024bd13bc599a1f2e927336))

## [0.10.0](https://github.com/matter-labs/zksync-os-server/compare/v0.9.2...v0.10.0) (2025-11-06)


### ⚠ BREAKING CHANGES

* support zksync-os v0.1.0 ([#557](https://github.com/matter-labs/zksync-os-server/issues/557))

### Features

* add last_execution_version metric ([#590](https://github.com/matter-labs/zksync-os-server/issues/590)) ([9343794](https://github.com/matter-labs/zksync-os-server/commit/9343794c7a27bd315a7a3096591265abb961247f))
* get rid of batch rescheduling (preparation to get rid of BatchStorage) ([#587](https://github.com/matter-labs/zksync-os-server/issues/587)) ([62dd891](https://github.com/matter-labs/zksync-os-server/commit/62dd89119749fcfe51280676bbc569e189d30626))
* remove app_bin_unpack_path from config ([#588](https://github.com/matter-labs/zksync-os-server/issues/588)) ([e55b0d4](https://github.com/matter-labs/zksync-os-server/commit/e55b0d43f631efbc39f2a24bbb8dcb08e5474727))
* support zksync-os v0.1.0 ([#557](https://github.com/matter-labs/zksync-os-server/issues/557)) ([178a1a9](https://github.com/matter-labs/zksync-os-server/commit/178a1a975dc682a24be5dc6d7e33733c7786f493))

## [0.9.2](https://github.com/matter-labs/zksync-os-server/compare/v0.9.1...v0.9.2) (2025-11-06)


### Features

* 2FA EN batch signing without L1 verification ([#459](https://github.com/matter-labs/zksync-os-server/issues/459)) ([e6d41ab](https://github.com/matter-labs/zksync-os-server/commit/e6d41abf581e5baeeda73b8a772ab7572a8d2b2e))
* get rid of l1_gas_pricing_multiplier ([#576](https://github.com/matter-labs/zksync-os-server/issues/576)) ([3699956](https://github.com/matter-labs/zksync-os-server/commit/36999561aa64f3af7b730e0bae8b461fd903a8b5))
* Protocol upgrade support for provers ([#577](https://github.com/matter-labs/zksync-os-server/issues/577)) ([a60bb89](https://github.com/matter-labs/zksync-os-server/commit/a60bb89c9c7a52c166cc208b98bdf2a3644bec3c))
* **sentry:** Use CLUSTER_NAME as environment tag ([#570](https://github.com/matter-labs/zksync-os-server/issues/570)) ([0befa23](https://github.com/matter-labs/zksync-os-server/commit/0befa239c7b6576cae986eb1e4f0398131dd17b2))


### Bug Fixes

* Consistency checker nonce for failed creates ([#574](https://github.com/matter-labs/zksync-os-server/issues/574)) ([8159d64](https://github.com/matter-labs/zksync-os-server/commit/8159d64d4dff8b1188ce45d6b45dd7e754bed3ad))
* proving empty blocks - fix division by zero error in metrics tracking ([#584](https://github.com/matter-labs/zksync-os-server/issues/584)) ([3c7d3bd](https://github.com/matter-labs/zksync-os-server/commit/3c7d3bd3ea713dd4b71af687f1110504a767ca87))
* set WORKDIR to /app ([#573](https://github.com/matter-labs/zksync-os-server/issues/573)) ([265dc34](https://github.com/matter-labs/zksync-os-server/commit/265dc347daba05e82d669486588a8b6980defd9f))

## [0.9.1](https://github.com/matter-labs/zksync-os-server/compare/v0.9.0...v0.9.1) (2025-10-29)


### Features

* add block rebuild options ([#565](https://github.com/matter-labs/zksync-os-server/issues/565)) ([eab9bdf](https://github.com/matter-labs/zksync-os-server/commit/eab9bdfa7ec205421e55251a2213a406995bc8aa))


### Bug Fixes

* consume l1 txs processed in rebuild commands ([#568](https://github.com/matter-labs/zksync-os-server/issues/568)) ([ff74bec](https://github.com/matter-labs/zksync-os-server/commit/ff74bece2252626782d31fd9358ce41ed5289649))

## [0.9.0](https://github.com/matter-labs/zksync-os-server/compare/v0.8.4...v0.9.0) (2025-10-28)


### ⚠ BREAKING CHANGES

* Opentelemetry support + config schema change ([#559](https://github.com/matter-labs/zksync-os-server/issues/559))

### Features

* eth_estimateGas state overrides ([#560](https://github.com/matter-labs/zksync-os-server/issues/560)) ([44a2281](https://github.com/matter-labs/zksync-os-server/commit/44a228151fb814d122b9afb75e88e980176c9902))
* Opentelemetry support + config schema change ([#559](https://github.com/matter-labs/zksync-os-server/issues/559)) ([592d6bb](https://github.com/matter-labs/zksync-os-server/commit/592d6bb080c561687f6f39a4c18badf27df640cf))
* pubdata price calculation ([#549](https://github.com/matter-labs/zksync-os-server/issues/549)) ([d1700ba](https://github.com/matter-labs/zksync-os-server/commit/d1700babcb7ac5cf4519a2771941050dc217a870))
* revm consistency checker ([#525](https://github.com/matter-labs/zksync-os-server/issues/525)) ([2061a01](https://github.com/matter-labs/zksync-os-server/commit/2061a01f2ae09923b00b33f1705d36ea7b62feb5))

## [0.8.4](https://github.com/matter-labs/zksync-os-server/compare/v0.8.3...v0.8.4) (2025-10-21)


### Features

* config in sequencer to limit block production for operations/debug ([#537](https://github.com/matter-labs/zksync-os-server/issues/537)) ([ebdde51](https://github.com/matter-labs/zksync-os-server/commit/ebdde5129cc15e03600378501744c52eca231263))
* eth_call state overrides ([#539](https://github.com/matter-labs/zksync-os-server/issues/539)) ([bdf32ab](https://github.com/matter-labs/zksync-os-server/commit/bdf32ab4875df087cee8a384456f6ade738c5bb6))
* **l1-sender:** use alloy-based tx inclusion ([#541](https://github.com/matter-labs/zksync-os-server/issues/541)) ([48202cd](https://github.com/matter-labs/zksync-os-server/commit/48202cdbd70381f3689670e7d76bfe53dcdd2801))
* **l1-watcher:** move pagination/polling into shared component ([#548](https://github.com/matter-labs/zksync-os-server/issues/548)) ([d98d0ef](https://github.com/matter-labs/zksync-os-server/commit/d98d0ef66e2c232141a72cf8b8d31fc23be14721))
* make pipelines repository-agnostic ([#536](https://github.com/matter-labs/zksync-os-server/issues/536)) ([e28635b](https://github.com/matter-labs/zksync-os-server/commit/e28635bdc12432857cbeb84056a684bba8e1edf9))
* **storage:** move replay DB to storage crate ([#535](https://github.com/matter-labs/zksync-os-server/issues/535)) ([9c43a90](https://github.com/matter-labs/zksync-os-server/commit/9c43a90011bcb63c69029c2a2505c1ad4576180d))


### Bug Fixes

* Disable warning on connection retries ([#545](https://github.com/matter-labs/zksync-os-server/issues/545)) ([1a56284](https://github.com/matter-labs/zksync-os-server/commit/1a5628418b11cec0e5b99cfcb6df10115a8e05a2))
* Persisting some info about the failed batch ([#532](https://github.com/matter-labs/zksync-os-server/issues/532)) ([ccc9a9f](https://github.com/matter-labs/zksync-os-server/commit/ccc9a9fe48279820731b46094404ccb3a57bdd21))
* **sequencer:** save replay record first ([#556](https://github.com/matter-labs/zksync-os-server/issues/556)) ([1f3fe08](https://github.com/matter-labs/zksync-os-server/commit/1f3fe08bfa9cc45cd499cc84fc789490e1e22497))

## [0.8.3](https://github.com/matter-labs/zksync-os-server/compare/v0.8.2...v0.8.3) (2025-10-15)


### Features

* add execution version enum ([#517](https://github.com/matter-labs/zksync-os-server/issues/517)) ([c5703f9](https://github.com/matter-labs/zksync-os-server/commit/c5703f9736bbe3511a833b75070b593bf854bf03))
* **l1-watcher:** poll events actively when behind ([#523](https://github.com/matter-labs/zksync-os-server/issues/523)) ([93d6b4b](https://github.com/matter-labs/zksync-os-server/commit/93d6b4becbc1bf27ca5331df72fbb3184c4fdc2f))
* **l1:** move `{Commit,Stored}BatchInfo` + introduce `BatchInfo` ([#505](https://github.com/matter-labs/zksync-os-server/issues/505)) ([fe0a6bd](https://github.com/matter-labs/zksync-os-server/commit/fe0a6bdf7df9f3488dff48fe779a383d337ebe23))
* **l1:** move L1 discovery out of `L1Sender` ([#502](https://github.com/matter-labs/zksync-os-server/issues/502)) ([32aff65](https://github.com/matter-labs/zksync-os-server/commit/32aff6570eec9b6e2061e6ac791d08f588da7c96))
* **mempool:** export even more metrics ([#529](https://github.com/matter-labs/zksync-os-server/issues/529)) ([1152166](https://github.com/matter-labs/zksync-os-server/commit/1152166d3516e6e6cd28878e3074fcd5e3ab6378))
* **mempool:** expose metrics ([#522](https://github.com/matter-labs/zksync-os-server/issues/522)) ([6de3a50](https://github.com/matter-labs/zksync-os-server/commit/6de3a50f50536676533ce356ef22989bcd9e688f))
* replace str with module name for app bin unpack path ([#516](https://github.com/matter-labs/zksync-os-server/issues/516)) ([3f90248](https://github.com/matter-labs/zksync-os-server/commit/3f90248b620088449fcdf60b6b608c5d533d2a74))
* Saving failed proofs to bucket and exposing endpoint to get them ([#507](https://github.com/matter-labs/zksync-os-server/issues/507)) ([0dc2093](https://github.com/matter-labs/zksync-os-server/commit/0dc2093b97115266b40589efd4a9bf54e68d1d66))
* **sequencer:** validate last 256 blocks for replayed blocks ([#524](https://github.com/matter-labs/zksync-os-server/issues/524)) ([9b17514](https://github.com/matter-labs/zksync-os-server/commit/9b175143313ff33294507aa790fe2276ff30f3c3))


### Bug Fixes

* **pipeline:** simplify task spawning ([#519](https://github.com/matter-labs/zksync-os-server/issues/519)) ([cdcfec5](https://github.com/matter-labs/zksync-os-server/commit/cdcfec5a0724ac9c06ea0e7c27cc320064980f7d))
* Reduced tracing level for debug functions ([#531](https://github.com/matter-labs/zksync-os-server/issues/531)) ([b960deb](https://github.com/matter-labs/zksync-os-server/commit/b960debd6a41b448a2f10d64551710334d5422b5))
* **storage:** read replay record atomically ([#521](https://github.com/matter-labs/zksync-os-server/issues/521)) ([ff474a7](https://github.com/matter-labs/zksync-os-server/commit/ff474a76c4b864c1041b5a4a32b0ac0450fb5a5d))
* **tree:** report backpressure ([#520](https://github.com/matter-labs/zksync-os-server/issues/520)) ([7efb8a7](https://github.com/matter-labs/zksync-os-server/commit/7efb8a701158234bec88ede6d142e5145b1189b3))

## [0.8.2](https://github.com/matter-labs/zksync-os-server/compare/v0.8.1...v0.8.2) (2025-10-13)


### Bug Fixes

* **l1-sender:** allow non-empty buffer for rescheduling ([#511](https://github.com/matter-labs/zksync-os-server/issues/511)) ([beec7ec](https://github.com/matter-labs/zksync-os-server/commit/beec7ec87ac1547b353c8a4db4b177896e1cb280))
* **l1-watcher:** update batch finality ([#506](https://github.com/matter-labs/zksync-os-server/issues/506)) ([ca11ba7](https://github.com/matter-labs/zksync-os-server/commit/ca11ba7593883ddbdadbe4e1d65dbd7b82a33857))

## [0.8.1](https://github.com/matter-labs/zksync-os-server/compare/v0.8.0...v0.8.1) (2025-10-11)


### Features

* **genesis:** Add genesis root hash to genesis.json ([#494](https://github.com/matter-labs/zksync-os-server/issues/494)) ([4887597](https://github.com/matter-labs/zksync-os-server/commit/4887597e1dbff1bd101af32eea91383c31b6c998))
* **l1:** retry RPC requests on internal error ([#496](https://github.com/matter-labs/zksync-os-server/issues/496)) ([e89d88a](https://github.com/matter-labs/zksync-os-server/commit/e89d88a46fe1319177bd6a24584eb09faca94faf))
* pipeline framework (8/X) - migrate executor l1 and batch sink ([#481](https://github.com/matter-labs/zksync-os-server/issues/481)) ([44d5776](https://github.com/matter-labs/zksync-os-server/commit/44d577669fa8a3c722c4e212563c9d59f1edc510))
* **rpc:** implement `web3` namespace ([#497](https://github.com/matter-labs/zksync-os-server/issues/497)) ([0ff0cc4](https://github.com/matter-labs/zksync-os-server/commit/0ff0cc4bd607ddd22883b5dce61177b609251bfa))
* track `execution_version` in genesis config ([#498](https://github.com/matter-labs/zksync-os-server/issues/498)) ([136a9a9](https://github.com/matter-labs/zksync-os-server/commit/136a9a982dc2ed132d31efe9b5b26b3c22dfe7a5))


### Bug Fixes

* add default v,r,s,yParity fields in L1TxType during serialization ([#500](https://github.com/matter-labs/zksync-os-server/issues/500)) ([a1f28ab](https://github.com/matter-labs/zksync-os-server/commit/a1f28ab7bfabe659bffc9902bee036fadd7ed406))

## [0.8.0](https://github.com/matter-labs/zksync-os-server/compare/v0.7.5...v0.8.0) (2025-10-09)


### ⚠ BREAKING CHANGES

* Protocol upgrade v1.1 ([#487](https://github.com/matter-labs/zksync-os-server/issues/487))

### Features

* add config for fee params override ([#489](https://github.com/matter-labs/zksync-os-server/issues/489)) ([13587e5](https://github.com/matter-labs/zksync-os-server/commit/13587e529f24f5b1ea6158626403b751e3504b56))
* add more general metrics ([#468](https://github.com/matter-labs/zksync-os-server/issues/468)) ([079a285](https://github.com/matter-labs/zksync-os-server/commit/079a28539dad438d5c483f9103661ef3f52d7e6e))
* Adding more documentation ([#455](https://github.com/matter-labs/zksync-os-server/issues/455)) ([2ed7bc7](https://github.com/matter-labs/zksync-os-server/commit/2ed7bc766d55d3bd682b7c4dcbce04a6a35a6bd3))
* ensure L1 tx is deserializable from RPC response ([#484](https://github.com/matter-labs/zksync-os-server/issues/484)) ([80abbcb](https://github.com/matter-labs/zksync-os-server/commit/80abbcb56c5f876d468bac34b5380ce08a6b4027))
* get rid of `Source`/`Sink` ([#461](https://github.com/matter-labs/zksync-os-server/issues/461)) ([762c9b7](https://github.com/matter-labs/zksync-os-server/commit/762c9b788743813a4b55d138701f7c620e3cc901))
* **l1-watcher:** track last committed/executed batch in finality ([#485](https://github.com/matter-labs/zksync-os-server/issues/485)) ([11c715c](https://github.com/matter-labs/zksync-os-server/commit/11c715c51522e2f2d90421aab2d483274bd81d40))
* make mempool configurable ([#464](https://github.com/matter-labs/zksync-os-server/issues/464)) ([63f9f69](https://github.com/matter-labs/zksync-os-server/commit/63f9f69fcb6486f9e57dc983c7efcf14c0623a69))
* Peek batch data from State ([#458](https://github.com/matter-labs/zksync-os-server/issues/458)) ([05ed98b](https://github.com/matter-labs/zksync-os-server/commit/05ed98b3977bebea91f489e7f33c95612a55d4c8))
* Peek FRI Proofs from ProofStorage ([#470](https://github.com/matter-labs/zksync-os-server/issues/470)) ([0b5bbec](https://github.com/matter-labs/zksync-os-server/commit/0b5bbeca5285e26c68e5bb91d1050d33b1bfdf31))
* pipeline framework (3/X) - migrate FriJobManager ([#465](https://github.com/matter-labs/zksync-os-server/issues/465)) ([2e012d9](https://github.com/matter-labs/zksync-os-server/commit/2e012d9ce4ffa9217dbb471293735a12c30f1e46))
* pipeline framework (4/X): migrate gapless committer ([#467](https://github.com/matter-labs/zksync-os-server/issues/467)) ([07cccce](https://github.com/matter-labs/zksync-os-server/commit/07cccce96472794d0e4dfb322b73ae832d7980de))
* pipeline framework (5/X) - migrate l1 committer ([#472](https://github.com/matter-labs/zksync-os-server/issues/472)) ([2ead9a0](https://github.com/matter-labs/zksync-os-server/commit/2ead9a0f857fb4af828332be9ce66a3544234efa))
* pipeline framework (PR 2/X) - `pipe()` syntax; consume `self`; migrate batcher ([#448](https://github.com/matter-labs/zksync-os-server/issues/448)) ([7366acc](https://github.com/matter-labs/zksync-os-server/commit/7366accd4f587da5b789fa0a730f49ba0e9c294c))
* pipeline framework PR 6/X - migrate l1 sender proves and SnarkJobsManager ([#477](https://github.com/matter-labs/zksync-os-server/issues/477)) ([84d87d6](https://github.com/matter-labs/zksync-os-server/commit/84d87d6b4f777467072b1b5398690fb8daa2e4d7))
* pipeline framework PR 7/X - priority tree migrated ([#479](https://github.com/matter-labs/zksync-os-server/issues/479)) ([2bc7250](https://github.com/matter-labs/zksync-os-server/commit/2bc72500e65301600cd25c4768e65ad9d46e6871))
* Protocol upgrade v1.1 ([#487](https://github.com/matter-labs/zksync-os-server/issues/487)) ([3f49fbc](https://github.com/matter-labs/zksync-os-server/commit/3f49fbc6640223fe02b90b38d5ef34f4731002a9))
* refactor priority tree ([#483](https://github.com/matter-labs/zksync-os-server/issues/483)) ([d12b99f](https://github.com/matter-labs/zksync-os-server/commit/d12b99f1780d3fbc2b3518a88db570d065d60083))
* set pubdata price to `1` ([#476](https://github.com/matter-labs/zksync-os-server/issues/476)) ([dcd060c](https://github.com/matter-labs/zksync-os-server/commit/dcd060ce6ffc1f5ab5b00a706d1d61dc2697fb09))
* update zksync-os to v0.0.26 and interface to v0.0.7 ([#429](https://github.com/matter-labs/zksync-os-server/issues/429)) ([f22e478](https://github.com/matter-labs/zksync-os-server/commit/f22e478bd14f9342bbf88ec3c0516434e6cab265))
* wait for tx in block context provider ([#478](https://github.com/matter-labs/zksync-os-server/issues/478)) ([d6e87b7](https://github.com/matter-labs/zksync-os-server/commit/d6e87b7522289f83c6b5f90ad41ae63b80e8abf3))


### Bug Fixes

* Add TxValidatorConfig to schema ([#475](https://github.com/matter-labs/zksync-os-server/issues/475)) ([797a0b5](https://github.com/matter-labs/zksync-os-server/commit/797a0b5744f6281b3fb2ac2f567c1a12f2638478))
* **multivm:** use correct directories and default version ([#490](https://github.com/matter-labs/zksync-os-server/issues/490)) ([35e5440](https://github.com/matter-labs/zksync-os-server/commit/35e54407a7574aaedb7c0d61292d1436cc8404fe))

## [0.7.5](https://github.com/matter-labs/zksync-os-server/compare/v0.7.4...v0.7.5) (2025-10-06)


### Features

* add net namespace and net_version RPC call support ([#436](https://github.com/matter-labs/zksync-os-server/issues/436)) ([e7b6ff5](https://github.com/matter-labs/zksync-os-server/commit/e7b6ff52d73506670ff5f2ffb03cdc8784fe2f96))
* add Sentry support ([#430](https://github.com/matter-labs/zksync-os-server/issues/430)) ([afed980](https://github.com/matter-labs/zksync-os-server/commit/afed98050b513d36efee32ea85cee2424203e225))
* drop GCP support and reduce dependencies ([#375](https://github.com/matter-labs/zksync-os-server/issues/375)) ([a4bd9e1](https://github.com/matter-labs/zksync-os-server/commit/a4bd9e1dd22b595a74584041152e653e155404ef))
* pipeline framework (1/X) - tree, sequencer and prover_input_gen ([#447](https://github.com/matter-labs/zksync-os-server/issues/447)) ([ba2186e](https://github.com/matter-labs/zksync-os-server/commit/ba2186edb5131e4138917ee4972f2d61c1a5945c))
* re-implement alloy tx types ([#438](https://github.com/matter-labs/zksync-os-server/issues/438)) ([9f993fc](https://github.com/matter-labs/zksync-os-server/commit/9f993fc2264a5c8c9c3820c9d00b29a6dad5616b))


### Bug Fixes

* report error on reverting `eth_call` ([#449](https://github.com/matter-labs/zksync-os-server/issues/449)) ([39ff0ae](https://github.com/matter-labs/zksync-os-server/commit/39ff0aef8ff5012437ea7638fd795e1fc978deed))

## [0.7.4](https://github.com/matter-labs/zksync-os-server/compare/v0.7.3...v0.7.4) (2025-09-30)


### Features

* add logging configuration (json/terminal/logfmt) ([#407](https://github.com/matter-labs/zksync-os-server/issues/407)) ([06ef2f5](https://github.com/matter-labs/zksync-os-server/commit/06ef2f51f92264f6a80d94d841d1921a60d41809))
* **en:** remote en config ([#387](https://github.com/matter-labs/zksync-os-server/issues/387)) ([550f3c4](https://github.com/matter-labs/zksync-os-server/commit/550f3c468977ae64aa28b44af62840cc2db37e39))
* set gas per pubdata to `1` ([#406](https://github.com/matter-labs/zksync-os-server/issues/406)) ([528ea85](https://github.com/matter-labs/zksync-os-server/commit/528ea85cde0d4494d32cb4db99336511a6f173e7))


### Bug Fixes

* hack to allow forcing null bridgehub in config ([#435](https://github.com/matter-labs/zksync-os-server/issues/435)) ([60c007b](https://github.com/matter-labs/zksync-os-server/commit/60c007b8da71ce6fceb2a15b74157642fd15afae))


### Reverts

* feat: set gas per pubdata to `1` ([#431](https://github.com/matter-labs/zksync-os-server/issues/431)) ([1ca638b](https://github.com/matter-labs/zksync-os-server/commit/1ca638b4bed6cfd9630d524fa80f627743a1e306))

## [0.7.3](https://github.com/matter-labs/zksync-os-server/compare/v0.7.2...v0.7.3) (2025-09-26)


### Features

* configurable fee collector ([#383](https://github.com/matter-labs/zksync-os-server/issues/383)) ([2d89f45](https://github.com/matter-labs/zksync-os-server/commit/2d89f45ce0105ae31bf3c19a9ce8e74aa8077d53))

## [0.7.2](https://github.com/matter-labs/zksync-os-server/compare/v0.7.1...v0.7.2) (2025-09-25)


### Bug Fixes

* missing unwrap_or in submit_proof ([#418](https://github.com/matter-labs/zksync-os-server/issues/418)) ([32f8ade](https://github.com/matter-labs/zksync-os-server/commit/32f8ade4748c4867dbdce69383071e5f34d158ad))

## [0.7.1](https://github.com/matter-labs/zksync-os-server/compare/v0.7.0...v0.7.1) (2025-09-25)


### Features

* more metrics and logs - gas per second, transaction status ([#415](https://github.com/matter-labs/zksync-os-server/issues/415)) ([6f7711a](https://github.com/matter-labs/zksync-os-server/commit/6f7711aa5a3df28070f718cf31f6371bbf7656dd))


### Bug Fixes

* unwrap_or in pick_real_job  ([#416](https://github.com/matter-labs/zksync-os-server/issues/416)) ([9097d00](https://github.com/matter-labs/zksync-os-server/commit/9097d0014785557b6d922b0442d73d31b83ad043))

## [0.7.0](https://github.com/matter-labs/zksync-os-server/compare/v0.6.4...v0.7.0) (2025-09-25)


### ⚠ BREAKING CHANGES

* add `execution_version` 2 ([#409](https://github.com/matter-labs/zksync-os-server/issues/409))

### Features

* add `execution_version` 2 ([#409](https://github.com/matter-labs/zksync-os-server/issues/409)) ([a661115](https://github.com/matter-labs/zksync-os-server/commit/a6611152b7eeab51d2bd3ea4fcfef5d15ccd5a40))


### Bug Fixes

* backward compatible deserialization for proofs ([#414](https://github.com/matter-labs/zksync-os-server/issues/414)) ([84e5182](https://github.com/matter-labs/zksync-os-server/commit/84e51827a4cbb4fb6cb060d4a7663622636b3fe7))

## [0.6.4](https://github.com/matter-labs/zksync-os-server/compare/v0.6.3...v0.6.4) (2025-09-22)


### Features

* config option to force starting block number ([#402](https://github.com/matter-labs/zksync-os-server/issues/402)) ([b6024ab](https://github.com/matter-labs/zksync-os-server/commit/b6024abb9a1461aacc2973b7dd823cd930971cc7))
* improve debug logging ([#401](https://github.com/matter-labs/zksync-os-server/issues/401)) ([d996338](https://github.com/matter-labs/zksync-os-server/commit/d996338b9b0264ede512f85370a58c0607d97c36))
* make batcher skip blocks that are already processed ([#404](https://github.com/matter-labs/zksync-os-server/issues/404)) ([edb2c27](https://github.com/matter-labs/zksync-os-server/commit/edb2c27cf0ca445d86688e2b5f4befcef11fc8b8))

## [0.6.3](https://github.com/matter-labs/zksync-os-server/compare/v0.6.2...v0.6.3) (2025-09-22)


### Bug Fixes

* priority tree caching ([#399](https://github.com/matter-labs/zksync-os-server/issues/399)) ([b8c4e8d](https://github.com/matter-labs/zksync-os-server/commit/b8c4e8dca86ddbeb054c594ec437d923c0c62824))

## [0.6.2](https://github.com/matter-labs/zksync-os-server/compare/v0.6.1...v0.6.2) (2025-09-22)


### Bug Fixes

* priority tree trim ([#397](https://github.com/matter-labs/zksync-os-server/issues/397)) ([e908c4e](https://github.com/matter-labs/zksync-os-server/commit/e908c4e0cbf5dfd90063cb4273f5551b55685795))

## [0.6.1](https://github.com/matter-labs/zksync-os-server/compare/v0.6.0...v0.6.1) (2025-09-22)


### Features

* **l1:** optimistic RPC retry policy ([#385](https://github.com/matter-labs/zksync-os-server/issues/385)) ([16f816b](https://github.com/matter-labs/zksync-os-server/commit/16f816bea3d50b2c98f0f836c60adec16fd5dde1))


### Bug Fixes

* **state:** do not overwrite full diffs ([#386](https://github.com/matter-labs/zksync-os-server/issues/386)) ([c715709](https://github.com/matter-labs/zksync-os-server/commit/c715709afa36edf4831c1c1ef3aacd85fd158d19))
* use correct previous_block_timestamp on server restart ([#384](https://github.com/matter-labs/zksync-os-server/issues/384)) ([941b1d5](https://github.com/matter-labs/zksync-os-server/commit/941b1d52e51321f524956b08d1568eeea6c2f247))

## [0.6.0](https://github.com/matter-labs/zksync-os-server/compare/v0.5.0...v0.6.0) (2025-09-17)


### ⚠ BREAKING CHANGES

* folder with risc-v binaries + handle protocol version in batch components ([#369](https://github.com/matter-labs/zksync-os-server/issues/369))

### Features

* add retry layer for l1 provider ([#377](https://github.com/matter-labs/zksync-os-server/issues/377)) ([8f2bfda](https://github.com/matter-labs/zksync-os-server/commit/8f2bfda76c8d0c8cbfec953aa14d7fa6d09c6d42))
* config option to disable l1 senders ([#372](https://github.com/matter-labs/zksync-os-server/issues/372)) ([51253ca](https://github.com/matter-labs/zksync-os-server/commit/51253cae83485ab8b23e370dabfc5bd1d2283a0b))
* folder with risc-v binaries + handle protocol version in batch components ([#369](https://github.com/matter-labs/zksync-os-server/issues/369)) ([39ff2cf](https://github.com/matter-labs/zksync-os-server/commit/39ff2cf7d657ecbea83ac640b02b485c9490c488))
* support L1-&gt;L2 tx gas estimation ([#370](https://github.com/matter-labs/zksync-os-server/issues/370)) ([11febe4](https://github.com/matter-labs/zksync-os-server/commit/11febe428708aaa69d96bef725654ef20bf60562))

## [0.5.0](https://github.com/matter-labs/zksync-os-server/compare/v0.4.0...v0.5.0) (2025-09-15)


### ⚠ BREAKING CHANGES

* Update state - contracts: zkos-v0.29.6, zkstack tool: origin/main ([#364](https://github.com/matter-labs/zksync-os-server/issues/364))
* zksync os inteface/multivm ([#345](https://github.com/matter-labs/zksync-os-server/issues/345))
* Update state - contracts from zkos-0.29.5 + scripts changes ([#356](https://github.com/matter-labs/zksync-os-server/issues/356))
* make EN replay streams HTTP 1.0 ([#341](https://github.com/matter-labs/zksync-os-server/issues/341))

### Features

* add persistence for priority tree ([#321](https://github.com/matter-labs/zksync-os-server/issues/321)) ([2107932](https://github.com/matter-labs/zksync-os-server/commit/210793218f104c6249ca061215959d389f7d89c6))
* additional metrics to various components ([#352](https://github.com/matter-labs/zksync-os-server/issues/352)) ([821f319](https://github.com/matter-labs/zksync-os-server/commit/821f319373ecab6bd0a9041000eb195a205a8526))
* delay the termination, expose health endpoint ([#348](https://github.com/matter-labs/zksync-os-server/issues/348)) ([ab4c709](https://github.com/matter-labs/zksync-os-server/commit/ab4c70956af9d118390b1db0f99f30fb59a5a622))
* Enhance documentation for zkos and era contracts updates ([#337](https://github.com/matter-labs/zksync-os-server/issues/337)) ([cfc42e2](https://github.com/matter-labs/zksync-os-server/commit/cfc42e20767410163f54de7c199853075a2e5ca7))
* have all user-facing config values in one file ([#349](https://github.com/matter-labs/zksync-os-server/issues/349)) ([14cf17c](https://github.com/matter-labs/zksync-os-server/commit/14cf17c4219222ef0d30154a93dd4f2ab6fc5648))
* implement `debug_traceCall` ([#359](https://github.com/matter-labs/zksync-os-server/issues/359)) ([1d11649](https://github.com/matter-labs/zksync-os-server/commit/1d1164938da483175ded72ac38ec24789657623b))
* **l1-sender:** wait for pending state to finalize ([#311](https://github.com/matter-labs/zksync-os-server/issues/311)) ([2aebbb5](https://github.com/matter-labs/zksync-os-server/commit/2aebbb5fee094b3a63843e30c27feb6861ce0109))
* make EN replay streams HTTP 1.0 ([#341](https://github.com/matter-labs/zksync-os-server/issues/341)) ([f78e184](https://github.com/matter-labs/zksync-os-server/commit/f78e184c76a8ecca081b5255e3eb49638f3d7d06))
* split l1_state metrics; fix typo in l1_sender metrics ([#357](https://github.com/matter-labs/zksync-os-server/issues/357)) ([b100eda](https://github.com/matter-labs/zksync-os-server/commit/b100eda5554081c8b8f08a99c832984f4dd6ff0b))
* Update state - contracts from zkos-0.29.5 + scripts changes ([#356](https://github.com/matter-labs/zksync-os-server/issues/356)) ([246618e](https://github.com/matter-labs/zksync-os-server/commit/246618e4fac6e95a060681ee7724ad5c303bf88b))
* Update state - contracts: zkos-v0.29.6, zkstack tool: origin/main ([#364](https://github.com/matter-labs/zksync-os-server/issues/364)) ([282919c](https://github.com/matter-labs/zksync-os-server/commit/282919cfaf8542d1cea15b06c80cf8c3e0aabd36))
* zksync os inteface/multivm ([#345](https://github.com/matter-labs/zksync-os-server/issues/345)) ([0498f2b](https://github.com/matter-labs/zksync-os-server/commit/0498f2b7e760b7ab16c7cc157d6b917eff08da8e))


### Bug Fixes

* `eth_getTransactionCount` takes mempool into account ([#360](https://github.com/matter-labs/zksync-os-server/issues/360)) ([2141089](https://github.com/matter-labs/zksync-os-server/commit/2141089dead809862114bc7e962bb95842cae2ee))
* gas field calculation in tx receipt ([#361](https://github.com/matter-labs/zksync-os-server/issues/361)) ([9bb51f4](https://github.com/matter-labs/zksync-os-server/commit/9bb51f4d20a4cc1135fef37047fee0c6c5c742a7))

## [0.4.0](https://github.com/matter-labs/zksync-os-server/compare/v0.3.0...v0.4.0) (2025-09-09)


### ⚠ BREAKING CHANGES

* external node can read previous replay version ([#224](https://github.com/matter-labs/zksync-os-server/issues/224))

### Features

* external node can read previous replay version ([#224](https://github.com/matter-labs/zksync-os-server/issues/224)) ([a4bd5f5](https://github.com/matter-labs/zksync-os-server/commit/a4bd5f5e7b1576e6af7dced62434488a2ab6c292))
* RPC monitoring middleware ([#306](https://github.com/matter-labs/zksync-os-server/issues/306)) ([8837e43](https://github.com/matter-labs/zksync-os-server/commit/8837e433cb76ef3b481e51c84018f3cf4af105cb))

## [0.3.0](https://github.com/matter-labs/zksync-os-server/compare/v0.2.0...v0.3.0) (2025-09-05)


### ⚠ BREAKING CHANGES

* update l1 contracts interface ([#339](https://github.com/matter-labs/zksync-os-server/issues/339))
* change L1->L2/upgrade tx type id ([#333](https://github.com/matter-labs/zksync-os-server/issues/333))

### Features

* **api:** implement `debug_traceBlockBy{Hash,Number}` ([#310](https://github.com/matter-labs/zksync-os-server/issues/310)) ([3fa831a](https://github.com/matter-labs/zksync-os-server/commit/3fa831aca46b6a0449fde705c19fc891b1a405a5)), closes [#309](https://github.com/matter-labs/zksync-os-server/issues/309)
* change L1-&gt;L2/upgrade tx type id ([#333](https://github.com/matter-labs/zksync-os-server/issues/333)) ([d62892c](https://github.com/matter-labs/zksync-os-server/commit/d62892cc4bab249106684c42332d3b10ae78bb92))
* metric for tx execution ([#323](https://github.com/matter-labs/zksync-os-server/issues/323)) ([ea889bf](https://github.com/matter-labs/zksync-os-server/commit/ea889bf165aaa20f6965c7812f1c49073de21499))
* update l1 contracts interface ([#339](https://github.com/matter-labs/zksync-os-server/issues/339)) ([c7b149e](https://github.com/matter-labs/zksync-os-server/commit/c7b149ee6618fb544d4d2edbf1ee8a3f4c3b161f))
* update tracing-subscriber version ([#325](https://github.com/matter-labs/zksync-os-server/issues/325)) ([b2e7442](https://github.com/matter-labs/zksync-os-server/commit/b2e74424a8bd9f8e8127981946499760534ff70a))


### Bug Fixes

* add forgotten state.compact_peridoically() ([#324](https://github.com/matter-labs/zksync-os-server/issues/324)) ([e38846a](https://github.com/matter-labs/zksync-os-server/commit/e38846aff6061b23d5aeea833a3b3805303e43d7))

## [0.2.0](https://github.com/matter-labs/zksync-os-server/compare/v0.1.2...v0.2.0) (2025-09-02)


### ⚠ BREAKING CHANGES

* adapt server for v29 ([#284](https://github.com/matter-labs/zksync-os-server/issues/284))

### Features

* adapt server for v29 ([#284](https://github.com/matter-labs/zksync-os-server/issues/284)) ([df2d66e](https://github.com/matter-labs/zksync-os-server/commit/df2d66e46668db6812be628b7c1e49658e12b3a2))
* add observability on node init ([#290](https://github.com/matter-labs/zksync-os-server/issues/290)) ([895fd6b](https://github.com/matter-labs/zksync-os-server/commit/895fd6b2bfc720a1c0462d161f3068e1aaf2441d))
* **api:** implement `debug_traceTransaction` ([#231](https://github.com/matter-labs/zksync-os-server/issues/231)) ([15cf104](https://github.com/matter-labs/zksync-os-server/commit/15cf1044a174b539548cde2bc7abf22e4b12bfb6))
* **docker:** use new crate ([#294](https://github.com/matter-labs/zksync-os-server/issues/294)) ([3a92eae](https://github.com/matter-labs/zksync-os-server/commit/3a92eae6430389104e8881d6cd33e0fbfcd45840))
* ERC20 integration tests ([#285](https://github.com/matter-labs/zksync-os-server/issues/285)) ([3d7dac5](https://github.com/matter-labs/zksync-os-server/commit/3d7dac5bece2431ea428040c72b3802aab9e4fe0))
* move sequencer implementation to its own crate ([#291](https://github.com/matter-labs/zksync-os-server/issues/291)) ([183ee2a](https://github.com/matter-labs/zksync-os-server/commit/183ee2ae1423c3f17921d87eac301def4e2150b0))
* refactor lib.rs in sequencer ([#280](https://github.com/matter-labs/zksync-os-server/issues/280)) ([454b104](https://github.com/matter-labs/zksync-os-server/commit/454b104bb335e3183f6a46662a06b09b79172801))
* Update state - contracts: zkos-v0.29.2, zkstack tool: 0267d99b366c97 ([#305](https://github.com/matter-labs/zksync-os-server/issues/305)) ([62d234d](https://github.com/matter-labs/zksync-os-server/commit/62d234ddecfa81bbb3a8cc5534dd3c96747315cf))
* update to zkos v0.0.20 and airbender 0.4.3 ([#301](https://github.com/matter-labs/zksync-os-server/issues/301)) ([be23bef](https://github.com/matter-labs/zksync-os-server/commit/be23bef943d4ff44c6af79020d0b3ac15430958c))
* use open source prover ([#300](https://github.com/matter-labs/zksync-os-server/issues/300)) ([82370e9](https://github.com/matter-labs/zksync-os-server/commit/82370e9decad8c5625b51a9e461938d1df3a374f))


### Bug Fixes

* block count limit ([#297](https://github.com/matter-labs/zksync-os-server/issues/297)) ([080dcc5](https://github.com/matter-labs/zksync-os-server/commit/080dcc5beea9fcf34fa805c6cd7e75ea5ba024ac))
* state recovery edge case ([#299](https://github.com/matter-labs/zksync-os-server/issues/299)) ([ccee05b](https://github.com/matter-labs/zksync-os-server/commit/ccee05b01095c2c92e86abd3682b7ba3a8651892))

## [0.1.2](https://github.com/matter-labs/zksync-os-server/compare/v0.1.1...v0.1.2) (2025-08-27)


### Features

* Allow loading configs from old yaml files ([#230](https://github.com/matter-labs/zksync-os-server/issues/230)) ([272b6e7](https://github.com/matter-labs/zksync-os-server/commit/272b6e7790dc5bef6f0d6688a815f67e1ce1ef7f))
* **api:** populate RPC block size ([#217](https://github.com/matter-labs/zksync-os-server/issues/217)) ([ce24acf](https://github.com/matter-labs/zksync-os-server/commit/ce24acf026ace7a49f0271ed03e8e3da6816a863))
* **api:** safeguard `zks_getL2ToL1LogProof` to work on executed batches ([#242](https://github.com/matter-labs/zksync-os-server/issues/242)) ([1450bf1](https://github.com/matter-labs/zksync-os-server/commit/1450bf14ec853824205d9c45bbfe04274bcb1230))
* basic validium support ([73fc1d1](https://github.com/matter-labs/zksync-os-server/commit/73fc1d112aff0b4096782a727cd12bdb1d163301))
* batcher seal criteria ([#213](https://github.com/matter-labs/zksync-os-server/issues/213)) ([fe8250a](https://github.com/matter-labs/zksync-os-server/commit/fe8250a04f2c7153a3ea36ebee66ed27e03c0395))
* **docker:** use clang/LLVM 19 on Trixie ([#229](https://github.com/matter-labs/zksync-os-server/issues/229)) ([0ff5c5b](https://github.com/matter-labs/zksync-os-server/commit/0ff5c5b8d2c540b8c75aa5686e430ea8892762d1))
* external node ([#163](https://github.com/matter-labs/zksync-os-server/issues/163)) ([d595e64](https://github.com/matter-labs/zksync-os-server/commit/d595e64f29112fa221a3ecdbf1499f5f3d14f15e))
* more metrics ([686cc12](https://github.com/matter-labs/zksync-os-server/commit/686cc12c7b328458240f594965bf92deaf25c9df))
* new state impl ([#278](https://github.com/matter-labs/zksync-os-server/issues/278)) ([6410653](https://github.com/matter-labs/zksync-os-server/commit/6410653e1f2c1ee8305f7013b503c56a094dd788))
* periodic collections of component states ([3b20513](https://github.com/matter-labs/zksync-os-server/commit/3b20513515f2f4bd116189bc4104296606ed8f1f))
* process genesis upgrade tx ([#201](https://github.com/matter-labs/zksync-os-server/issues/201)) ([9cc9a9c](https://github.com/matter-labs/zksync-os-server/commit/9cc9a9c79b3c44a242c1a8c66eaa7fb0014bfb09))
* **proof-storage:** use object store ([#225](https://github.com/matter-labs/zksync-os-server/issues/225)) ([0342daa](https://github.com/matter-labs/zksync-os-server/commit/0342daae9ba404df55cb2fbd6fca76dcf80773c7))
* refactor config ([#246](https://github.com/matter-labs/zksync-os-server/issues/246)) ([6ef1f06](https://github.com/matter-labs/zksync-os-server/commit/6ef1f061150fc639c42d24acf1e3f3847108d795))
* refine component state tracking ([#256](https://github.com/matter-labs/zksync-os-server/issues/256)) ([8b64257](https://github.com/matter-labs/zksync-os-server/commit/8b64257866d052e1d121735d3faf7c195082bfaf))
* speed-up batch storage lookup ([#273](https://github.com/matter-labs/zksync-os-server/issues/273)) ([1d24514](https://github.com/matter-labs/zksync-os-server/commit/1d24514cd8f33f41cdc9aaa45623df5b8aa03bf9))
* **storage:** add `ReadStateHistory` trait ([#244](https://github.com/matter-labs/zksync-os-server/issues/244)) ([1e7a4bb](https://github.com/matter-labs/zksync-os-server/commit/1e7a4bb22dd686c0dfe4ad99e4ff4dc1fb128dc7))
* Update codebase to use v0.3.3 verifiers ([#223](https://github.com/matter-labs/zksync-os-server/issues/223)) ([f457bcf](https://github.com/matter-labs/zksync-os-server/commit/f457bcf68f7cf4e8e4ec39e1cbf1d2b40ce74363))
* upgrade bincode to v2 ([#274](https://github.com/matter-labs/zksync-os-server/issues/274)) ([b5066b1](https://github.com/matter-labs/zksync-os-server/commit/b5066b12f80482df9026f70d29aad96ac7901768))
* zksync os bump to 0.0.13 ([#283](https://github.com/matter-labs/zksync-os-server/issues/283)) ([177364a](https://github.com/matter-labs/zksync-os-server/commit/177364a33b064897d77b47d41ae4a98460d3f6f2))


### Bug Fixes

* always replay at least one block ([#281](https://github.com/matter-labs/zksync-os-server/issues/281)) ([b298988](https://github.com/matter-labs/zksync-os-server/commit/b2989887dbf773cd82dce26701229d96154036f3))
* **api:** flatten L1 tx envelopes ([#234](https://github.com/matter-labs/zksync-os-server/issues/234)) ([f4e4296](https://github.com/matter-labs/zksync-os-server/commit/f4e429601644de63564bc17138db841d80ed2a79))
* **api:** proper type id for txs in api ([#269](https://github.com/matter-labs/zksync-os-server/issues/269)) ([c6993b7](https://github.com/matter-labs/zksync-os-server/commit/c6993b761ba5713411e697485e20b0842ecddf41))
* commit- and execute- watchers - fix one-off error in batch numbers ([53976e0](https://github.com/matter-labs/zksync-os-server/commit/53976e09522bdaf256f96ed529cc1b1435b43f51))
* **docker:** add genesis.json to docker image ([#220](https://github.com/matter-labs/zksync-os-server/issues/220)) ([2b2c3d0](https://github.com/matter-labs/zksync-os-server/commit/2b2c3d0eed11e8c4a2f36a80f935433109b8f63b))
* EN and handle errors more gracefully ([#247](https://github.com/matter-labs/zksync-os-server/issues/247)) ([0af3d9c](https://github.com/matter-labs/zksync-os-server/commit/0af3d9ca9991f65100f0f0c594292cbef7fa9d9f))
* **l1:** various `alloy::Provider` improvements ([#272](https://github.com/matter-labs/zksync-os-server/issues/272)) ([1f4fca4](https://github.com/matter-labs/zksync-os-server/commit/1f4fca47d991c63f161d2227312e0d8d5131d191))
* main after EN, serde/bincode accident ([#221](https://github.com/matter-labs/zksync-os-server/issues/221)) ([a7b4a2f](https://github.com/matter-labs/zksync-os-server/commit/a7b4a2f357d7427a116ff165181744da5a139a85))
* make get_transaction_receipt fallible ([#279](https://github.com/matter-labs/zksync-os-server/issues/279)) ([16cce7b](https://github.com/matter-labs/zksync-os-server/commit/16cce7be82ac39d68abb0facdfdd68bf1c833c70))
* set correct default for pubdata limit ([#241](https://github.com/matter-labs/zksync-os-server/issues/241)) ([2beb101](https://github.com/matter-labs/zksync-os-server/commit/2beb10194040cbc32220f56b4d3bb2dbe42b650d))
* skip already committed blocks before main batcher loop ([#286](https://github.com/matter-labs/zksync-os-server/issues/286)) ([7e9ea74](https://github.com/matter-labs/zksync-os-server/commit/7e9ea74c09d48b6fea677335d2d847e452fb17a1))
* start from batch number instead of block number ([#228](https://github.com/matter-labs/zksync-os-server/issues/228)) ([241a00e](https://github.com/matter-labs/zksync-os-server/commit/241a00e73a4d32bb317843205f7d5e9a3d67bf3e))
* temporary disable l1 commit and execute watchers ([99bdfbc](https://github.com/matter-labs/zksync-os-server/commit/99bdfbc627276e8c80f08e9c8320d5b0e5d4ab44))
* track timeout seal criteria in batcher ([b136822](https://github.com/matter-labs/zksync-os-server/commit/b1368224e51d5458921e817d952e1e495a12994b))
* use validium-rollup setting from L1 - not config; fix integration tests ([#255](https://github.com/matter-labs/zksync-os-server/issues/255)) ([19a1a82](https://github.com/matter-labs/zksync-os-server/commit/19a1a8283c6162fc0d822e241d5a5c5aa7f0ed27))

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
