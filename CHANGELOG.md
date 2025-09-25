# Changelog

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
