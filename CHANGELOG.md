# Changelog

## [0.19.0](https://github.com/matter-labs/zksync-os-server/compare/v0.18.0...v0.19.0) (2026-03-26)


### ⚠ BREAKING CHANGES

* **network:** use chain-aware fork id for filtering discv5 peers ([#1051](https://github.com/matter-labs/zksync-os-server/issues/1051))
* Remove unnecessary configs for EN ([#986](https://github.com/matter-labs/zksync-os-server/issues/986))
* Store FRI proofs locally, not in S3 ([#891](https://github.com/matter-labs/zksync-os-server/issues/891))
* Commit encoding v4 support ([#899](https://github.com/matter-labs/zksync-os-server/issues/899))
* **network:** fully migrate replay transport to p2p network ([#873](https://github.com/matter-labs/zksync-os-server/issues/873))
* change api l2 l1 log format ([#875](https://github.com/matter-labs/zksync-os-server/issues/875))
* drop proving support for v29.x and v30.0 versions ([#822](https://github.com/matter-labs/zksync-os-server/issues/822))
* Execution of service interop transactions ([#803](https://github.com/matter-labs/zksync-os-server/issues/803))
* use token prices in fee model ([#787](https://github.com/matter-labs/zksync-os-server/issues/787))
* token price updater component ([#779](https://github.com/matter-labs/zksync-os-server/issues/779))
* Basic V31 Support ([#759](https://github.com/matter-labs/zksync-os-server/issues/759))
* protocol upgrade v0.30.1 (zksync-os v0.2.5) ([#743](https://github.com/matter-labs/zksync-os-server/issues/743))
* **network:** use real HTTP server/client for batch verification ([#737](https://github.com/matter-labs/zksync-os-server/issues/737))
* **network:** use real HTTP server/client for replay transport ([#729](https://github.com/matter-labs/zksync-os-server/issues/729))
* allow EN to sync with overriden records ([#657](https://github.com/matter-labs/zksync-os-server/issues/657))
* Remove deprecated legacy prover API ([#674](https://github.com/matter-labs/zksync-os-server/issues/674))
* v30 zksync os protocol upgrade support ([#594](https://github.com/matter-labs/zksync-os-server/issues/594))
* upgrade system (part 1 of N) ([#582](https://github.com/matter-labs/zksync-os-server/issues/582))
* support zksync-os v0.1.0 ([#557](https://github.com/matter-labs/zksync-os-server/issues/557))
* Opentelemetry support + config schema change ([#559](https://github.com/matter-labs/zksync-os-server/issues/559))
* Protocol upgrade v1.1 ([#487](https://github.com/matter-labs/zksync-os-server/issues/487))
* add `execution_version` 2 ([#409](https://github.com/matter-labs/zksync-os-server/issues/409))
* folder with risc-v binaries + handle protocol version in batch components ([#369](https://github.com/matter-labs/zksync-os-server/issues/369))
* Update state - contracts: zkos-v0.29.6, zkstack tool: origin/main ([#364](https://github.com/matter-labs/zksync-os-server/issues/364))
* zksync os inteface/multivm ([#345](https://github.com/matter-labs/zksync-os-server/issues/345))
* Update state - contracts from zkos-0.29.5 + scripts changes ([#356](https://github.com/matter-labs/zksync-os-server/issues/356))
* make EN replay streams HTTP 1.0 ([#341](https://github.com/matter-labs/zksync-os-server/issues/341))
* external node can read previous replay version ([#224](https://github.com/matter-labs/zksync-os-server/issues/224))
* update l1 contracts interface ([#339](https://github.com/matter-labs/zksync-os-server/issues/339))
* change L1->L2/upgrade tx type id ([#333](https://github.com/matter-labs/zksync-os-server/issues/333))
* adapt server for v29 ([#284](https://github.com/matter-labs/zksync-os-server/issues/284))

### Features

* 2FA EN batch signing without L1 verification ([#459](https://github.com/matter-labs/zksync-os-server/issues/459)) ([fe5a575](https://github.com/matter-labs/zksync-os-server/commit/fe5a575c66013815ebcf44596d070da02cda0104))
* 2FA L1 integration ([#726](https://github.com/matter-labs/zksync-os-server/issues/726)) ([b1e6348](https://github.com/matter-labs/zksync-os-server/commit/b1e6348a8a0570c748a89b26b697d1edd286d6c8))
* Accumulated interop txs ([#848](https://github.com/matter-labs/zksync-os-server/issues/848)) ([ff86df6](https://github.com/matter-labs/zksync-os-server/commit/ff86df6f46995d7c182292b645d3337ada016e8f))
* adapt server for v29 ([#284](https://github.com/matter-labs/zksync-os-server/issues/284)) ([0aa77a6](https://github.com/matter-labs/zksync-os-server/commit/0aa77a6cbf57b1674c925113ecd933e75f6be281))
* add `execution_version` 2 ([#409](https://github.com/matter-labs/zksync-os-server/issues/409)) ([ef248b9](https://github.com/matter-labs/zksync-os-server/commit/ef248b9b2789841b81cde24ebe592e9c7b398e34))
* add bash script to run local chains ([#777](https://github.com/matter-labs/zksync-os-server/issues/777)) ([acc21e5](https://github.com/matter-labs/zksync-os-server/commit/acc21e54e47ba0f3ea2fab71f4e0acc8f8628f2a))
* add block hash to revm divergence panic message ([#880](https://github.com/matter-labs/zksync-os-server/issues/880)) ([d43c330](https://github.com/matter-labs/zksync-os-server/commit/d43c3302915242c933e29cb9622b40a8bfce825c))
* add block rebuild options ([#565](https://github.com/matter-labs/zksync-os-server/issues/565)) ([3c78b44](https://github.com/matter-labs/zksync-os-server/commit/3c78b44a92b5a1c3dbca54388c2825083ece879e))
* add config for fee params override ([#489](https://github.com/matter-labs/zksync-os-server/issues/489)) ([2e0f26f](https://github.com/matter-labs/zksync-os-server/commit/2e0f26fe9c8557f5cc3d55e57f5f66e8734c319a))
* add config for l2 signer blacklist ([#596](https://github.com/matter-labs/zksync-os-server/issues/596)) ([4506384](https://github.com/matter-labs/zksync-os-server/commit/4506384f3f90c52b12407db01584373cbfa63351))
* add execution version enum ([#517](https://github.com/matter-labs/zksync-os-server/issues/517)) ([6699999](https://github.com/matter-labs/zksync-os-server/commit/6699999ee8a77725144f3b88f57926dcf785dc7a))
* add gateway interop fee updater ([#968](https://github.com/matter-labs/zksync-os-server/issues/968)) ([147e557](https://github.com/matter-labs/zksync-os-server/commit/147e557050a2b5318f4e4900c7e15faf89fcbcf2))
* add internal config; use it in revm checker ([#608](https://github.com/matter-labs/zksync-os-server/issues/608)) ([bb68062](https://github.com/matter-labs/zksync-os-server/commit/bb68062a4a6f915986ea37e72a9333be73d14657))
* add last_execution_version metric ([#590](https://github.com/matter-labs/zksync-os-server/issues/590)) ([2e64f76](https://github.com/matter-labs/zksync-os-server/commit/2e64f76f3a1ebaf7d91354587fb5c4540e57fe43))
* add logging configuration (json/terminal/logfmt) ([#407](https://github.com/matter-labs/zksync-os-server/issues/407)) ([d343f98](https://github.com/matter-labs/zksync-os-server/commit/d343f984b195284546af910db62346c373dcb3d8))
* add metric for base fee and native price ([#844](https://github.com/matter-labs/zksync-os-server/issues/844)) ([5514cee](https://github.com/matter-labs/zksync-os-server/commit/5514cee288ba2356fbacaa37a69f11d0790bb65c))
* Add metric for blacklisted addresses count ([#820](https://github.com/matter-labs/zksync-os-server/issues/820)) ([c0e7e7b](https://github.com/matter-labs/zksync-os-server/commit/c0e7e7bb630664a742194c014fa4db5e77e448bf))
* add more eth-sender metrics. Bump fee limit. ([#789](https://github.com/matter-labs/zksync-os-server/issues/789)) ([e808b4c](https://github.com/matter-labs/zksync-os-server/commit/e808b4c901d29c9f20eae4247f76489828f9dd1c))
* add more general metrics ([#468](https://github.com/matter-labs/zksync-os-server/issues/468)) ([0ff22cc](https://github.com/matter-labs/zksync-os-server/commit/0ff22cc6703776ac9ba563cbf73164756cf9c35b))
* add net namespace and net_version RPC call support ([#436](https://github.com/matter-labs/zksync-os-server/issues/436)) ([836f982](https://github.com/matter-labs/zksync-os-server/commit/836f9827b460701c6d8941cadb34bda3fe0a6efa))
* add observability on node init ([#290](https://github.com/matter-labs/zksync-os-server/issues/290)) ([daf2c63](https://github.com/matter-labs/zksync-os-server/commit/daf2c6334ae5ec9b899eeae45b6668e34712e918))
* add persistence for priority tree ([#321](https://github.com/matter-labs/zksync-os-server/issues/321)) ([5682527](https://github.com/matter-labs/zksync-os-server/commit/5682527f1f294e34d08606f738a9c6034c56e88c))
* Add proper gateway migration watcher ([#921](https://github.com/matter-labs/zksync-os-server/issues/921)) ([063911b](https://github.com/matter-labs/zksync-os-server/commit/063911b4635c912c99b984b9cf5c28e1b6ee4276))
* add pubdata price cap ([#842](https://github.com/matter-labs/zksync-os-server/issues/842)) ([c5988d5](https://github.com/matter-labs/zksync-os-server/commit/c5988d544b2b117ade853f434dbdc43a2943115a))
* add retry layer for l1 provider ([#377](https://github.com/matter-labs/zksync-os-server/issues/377)) ([66d27ce](https://github.com/matter-labs/zksync-os-server/commit/66d27ce624df9412ebc5477b34e8bda77189d6d2))
* Add REVM support of multiple execution versions ([#597](https://github.com/matter-labs/zksync-os-server/issues/597)) ([83f633e](https://github.com/matter-labs/zksync-os-server/commit/83f633e53d5796114dd39ff6392d3db8b9947c74))
* add Sentry support ([#430](https://github.com/matter-labs/zksync-os-server/issues/430)) ([ec57018](https://github.com/matter-labs/zksync-os-server/commit/ec570182051fbd79e5c30f75b495b389e275523f))
* add sequencer sandbox mode ([#730](https://github.com/matter-labs/zksync-os-server/issues/730)) ([53ab9fe](https://github.com/matter-labs/zksync-os-server/commit/53ab9fe3a519675f0f97c8ff8e3b0552c694cf53))
* Add set SL chain Id tx after upgrade ([#1047](https://github.com/matter-labs/zksync-os-server/issues/1047)) ([0cbab2e](https://github.com/matter-labs/zksync-os-server/commit/0cbab2e7ac1491605c9ff1f601d48126de8d4efe))
* add some prover metrics ([#611](https://github.com/matter-labs/zksync-os-server/issues/611)) ([1a4b89f](https://github.com/matter-labs/zksync-os-server/commit/1a4b89fc1a78bfe1b7333fbf6b4cae9efeda0873))
* add support for YAML config files ([#785](https://github.com/matter-labs/zksync-os-server/issues/785)) ([16573e7](https://github.com/matter-labs/zksync-os-server/commit/16573e7e7e7890311f2ce125a30cd91aff7d0110))
* Add time_since metrics ([#628](https://github.com/matter-labs/zksync-os-server/issues/628)) ([c350eba](https://github.com/matter-labs/zksync-os-server/commit/c350ebad27a41e97b09648b0916c50bc51a26b3b))
* add toHex helper for JS tracer ([#761](https://github.com/matter-labs/zksync-os-server/issues/761)) ([21108e4](https://github.com/matter-labs/zksync-os-server/commit/21108e412d5d289bcb42e21c4c4c36e577690a13))
* add trace logs to estimate gas with exec results ([#1044](https://github.com/matter-labs/zksync-os-server/issues/1044)) ([a20e6aa](https://github.com/matter-labs/zksync-os-server/commit/a20e6aaf464abe6844578b8ad12a4def30f21bd1))
* Adding more documentation ([#455](https://github.com/matter-labs/zksync-os-server/issues/455)) ([56487db](https://github.com/matter-labs/zksync-os-server/commit/56487dbe9deed2554f1cae4f67d756bd29c79b87))
* Adding operator signing with HSM ([#956](https://github.com/matter-labs/zksync-os-server/issues/956)) ([e26ee56](https://github.com/matter-labs/zksync-os-server/commit/e26ee5638b7ae4e73652c0d15805770722260865))
* additional metrics to various components ([#352](https://github.com/matter-labs/zksync-os-server/issues/352)) ([a8948f4](https://github.com/matter-labs/zksync-os-server/commit/a8948f4280b8314f2efcc15d3fd58aaa7beb5425))
* adjust pubdata price based on blob fill ratio ([#700](https://github.com/matter-labs/zksync-os-server/issues/700)) ([f5727b8](https://github.com/matter-labs/zksync-os-server/commit/f5727b8da7562548924d859f35884200f1fef5fc))
* adjust pubdata price based on blob fill ratio (2nd attempt) ([#756](https://github.com/matter-labs/zksync-os-server/issues/756)) ([ad62fec](https://github.com/matter-labs/zksync-os-server/commit/ad62fec099bce333d60025c97de12e10bc151db2))
* allow EN to sync with overriden records ([#657](https://github.com/matter-labs/zksync-os-server/issues/657)) ([45dedbe](https://github.com/matter-labs/zksync-os-server/commit/45dedbe13ad6998a37f8349d6710f7e83c823c36))
* **api:** forward EN transactions to main node ([#624](https://github.com/matter-labs/zksync-os-server/issues/624)) ([6259e29](https://github.com/matter-labs/zksync-os-server/commit/6259e2901e8d35e51847f8c725f501867405f0d3))
* **api:** implement `debug_traceBlockBy{Hash,Number}` ([#310](https://github.com/matter-labs/zksync-os-server/issues/310)) ([4b77119](https://github.com/matter-labs/zksync-os-server/commit/4b77119914a205eed5f8b27f25b8e5bc3db57c3e)), closes [#309](https://github.com/matter-labs/zksync-os-server/issues/309)
* **api:** implement `debug_traceTransaction` ([#231](https://github.com/matter-labs/zksync-os-server/issues/231)) ([888b2aa](https://github.com/matter-labs/zksync-os-server/commit/888b2aa6f59decedde301283516696d2259545ad))
* **api:** implement EIP-7966 eth_sendRawTransactionSync ([#621](https://github.com/matter-labs/zksync-os-server/issues/621)) ([ae64407](https://github.com/matter-labs/zksync-os-server/commit/ae64407da992764d4c4a0228441d925259f5a2ad))
* Basic V31 Support ([#759](https://github.com/matter-labs/zksync-os-server/issues/759)) ([b43d04a](https://github.com/matter-labs/zksync-os-server/commit/b43d04a4c859c8cd324e640f5875d3d16a1ef852))
* **batch-verification:** make HTTPS connection a 2-way stream ([#862](https://github.com/matter-labs/zksync-os-server/issues/862)) ([4c87a77](https://github.com/matter-labs/zksync-os-server/commit/4c87a77690844ee064c23853f8a7baf5785ae767))
* **batcher:** make the limit of transaction count per batch configurable ([#796](https://github.com/matter-labs/zksync-os-server/issues/796)) ([f29faaf](https://github.com/matter-labs/zksync-os-server/commit/f29faaf1380ac1b79d348b68f2366a3ea9be3fa1))
* **batcher:** re-create batches using L1 watcher's data ([#672](https://github.com/matter-labs/zksync-os-server/issues/672)) ([95d5f54](https://github.com/matter-labs/zksync-os-server/commit/95d5f54d369267c3471752d29c2a3f05e83e7456))
* blob computation overhead for pubdata price ([#693](https://github.com/matter-labs/zksync-os-server/issues/693)) ([7300860](https://github.com/matter-labs/zksync-os-server/commit/7300860ff24d8785ff1f17ef39d90401d912ae7a))
* Bump zksync-os dev version ([#911](https://github.com/matter-labs/zksync-os-server/issues/911)) ([9dc454c](https://github.com/matter-labs/zksync-os-server/commit/9dc454c383680276f69101e658c0f94a6f89c258))
* change api l2 l1 log format ([#875](https://github.com/matter-labs/zksync-os-server/issues/875)) ([c77a31f](https://github.com/matter-labs/zksync-os-server/commit/c77a31f1e236fc1a2734c7242a5cfad78b7ec802))
* change L1-&gt;L2/upgrade tx type id ([#333](https://github.com/matter-labs/zksync-os-server/issues/333)) ([811d9fe](https://github.com/matter-labs/zksync-os-server/commit/811d9fe5d778119122bd3d3bc5d20bedfdc9b7b0))
* Commit encoding v4 support ([#899](https://github.com/matter-labs/zksync-os-server/issues/899)) ([0d9abe9](https://github.com/matter-labs/zksync-os-server/commit/0d9abe9f056065ef4d11b4c7d1392be9489c81b7))
* config in sequencer to limit block production for operations/debug ([#537](https://github.com/matter-labs/zksync-os-server/issues/537)) ([c422bbb](https://github.com/matter-labs/zksync-os-server/commit/c422bbb8353cb5cdf5c3d5cba9b5e4d5d13fb3f4))
* config option to disable batcher hash assertion when rebuilding batches ([#647](https://github.com/matter-labs/zksync-os-server/issues/647)) ([dbc58e2](https://github.com/matter-labs/zksync-os-server/commit/dbc58e2cceed337f2a4be841935f16a31bf79b9f))
* config option to disable l1 senders ([#372](https://github.com/matter-labs/zksync-os-server/issues/372)) ([d37ba6b](https://github.com/matter-labs/zksync-os-server/commit/d37ba6ba85ad6098b4ff665d253eb158be2ab54b))
* config option to disable priority tree ([#738](https://github.com/matter-labs/zksync-os-server/issues/738)) ([7c5cbff](https://github.com/matter-labs/zksync-os-server/commit/7c5cbff00008fa296faabb7f06bd2df0035fba47))
* config option to force starting block number ([#402](https://github.com/matter-labs/zksync-os-server/issues/402)) ([7b17962](https://github.com/matter-labs/zksync-os-server/commit/7b17962943d22d822e88eb303f52e0f8cca2c562))
* **config:** Add config command ([#697](https://github.com/matter-labs/zksync-os-server/issues/697)) ([a76da2c](https://github.com/matter-labs/zksync-os-server/commit/a76da2cb3b69fd26ea98e723d4d56c3d53a7cc54))
* **config:** make mempool tx_fee_cap configurable ([#717](https://github.com/matter-labs/zksync-os-server/issues/717)) ([ee3e782](https://github.com/matter-labs/zksync-os-server/commit/ee3e782f566a5361f1d275c3868b758ccfa956f2))
* **config:** set production-oriented defaults, extract local dev overrides ([#1062](https://github.com/matter-labs/zksync-os-server/issues/1062)) ([edbc62d](https://github.com/matter-labs/zksync-os-server/commit/edbc62df70292cc227540d7602998aaeeb5c317f))
* configurable fee collector ([#383](https://github.com/matter-labs/zksync-os-server/issues/383)) ([4c7a208](https://github.com/matter-labs/zksync-os-server/commit/4c7a2085972a79fd19acad6dcce78d48ea5c5446))
* **config:** use EtherAmount for fee-related configs ([#676](https://github.com/matter-labs/zksync-os-server/issues/676)) ([22a9929](https://github.com/matter-labs/zksync-os-server/commit/22a99298209bbf640b5d2ff6ba83f62f64d0b7f8))
* consensus integration 1/5: Sequencer split in BlockExecutor and BlockApplier ([#953](https://github.com/matter-labs/zksync-os-server/issues/953)) ([cc795f9](https://github.com/matter-labs/zksync-os-server/commit/cc795f94f922c30fb81ee06379e55e46847de49a))
* consensus integration 2/5: Consensus interface, raft dependency ([#958](https://github.com/matter-labs/zksync-os-server/issues/958)) ([a234e51](https://github.com/matter-labs/zksync-os-server/commit/a234e51a6052174649db6d352a20705babaaec26))
* **db:** keep overwritten replay records ([#620](https://github.com/matter-labs/zksync-os-server/issues/620)) ([6377d2b](https://github.com/matter-labs/zksync-os-server/commit/6377d2b18acffe79c5ebc51258691f0868f6b545))
* delay the termination, expose health endpoint ([#348](https://github.com/matter-labs/zksync-os-server/issues/348)) ([cfcf5be](https://github.com/matter-labs/zksync-os-server/commit/cfcf5be10b096579dba9da84a5e643e7d1a02f0f))
* **deposit tool:** Make it work with https provider; use ether as unit ([#794](https://github.com/matter-labs/zksync-os-server/issues/794)) ([c928fc9](https://github.com/matter-labs/zksync-os-server/commit/c928fc9ec5bdcc0420ed034f2f4c565a64fb623a))
* do not require batch storage (S3) for ENs ([#810](https://github.com/matter-labs/zksync-os-server/issues/810)) ([9729d14](https://github.com/matter-labs/zksync-os-server/commit/9729d14a9425e860a4a403da075944c03f3a8627))
* do not require batch storage for priority tree ([#825](https://github.com/matter-labs/zksync-os-server/issues/825)) ([9c111d4](https://github.com/matter-labs/zksync-os-server/commit/9c111d465f7601e0a9d55ee8885b0ee56e04cf30))
* do not require S3 for RPC ([#827](https://github.com/matter-labs/zksync-os-server/issues/827)) ([8f905a3](https://github.com/matter-labs/zksync-os-server/commit/8f905a3a6106974425711bac410d5f97a80ed595))
* **docker:** use new crate ([#294](https://github.com/matter-labs/zksync-os-server/issues/294)) ([af91675](https://github.com/matter-labs/zksync-os-server/commit/af916752fda16d99c6c31da1ecd8696fb16d6874))
* Don't report Passthrough in batch_number metrics ([#683](https://github.com/matter-labs/zksync-os-server/issues/683)) ([9d760c9](https://github.com/matter-labs/zksync-os-server/commit/9d760c9d196d87e0d8d23d226d106fa0ecc4cfcb))
* drop GCP support and reduce dependencies ([#375](https://github.com/matter-labs/zksync-os-server/issues/375)) ([e0d030c](https://github.com/matter-labs/zksync-os-server/commit/e0d030c97ed5ab944bcafab751bee0457a515a6a))
* drop proving support for v29.x and v30.0 versions ([#822](https://github.com/matter-labs/zksync-os-server/issues/822)) ([afeb4af](https://github.com/matter-labs/zksync-os-server/commit/afeb4af2f1db36a7f431da450a4649e9bf43d96c))
* Enhance documentation for zkos and era contracts updates ([#337](https://github.com/matter-labs/zksync-os-server/issues/337)) ([430e355](https://github.com/matter-labs/zksync-os-server/commit/430e35539df86d9a2931a1cc713c99750de8c73e))
* **en:** remote en config ([#387](https://github.com/matter-labs/zksync-os-server/issues/387)) ([c0364bf](https://github.com/matter-labs/zksync-os-server/commit/c0364bf03911a76155306dab6e16bfd405ab89f7))
* ensure L1 tx is deserializable from RPC response ([#484](https://github.com/matter-labs/zksync-os-server/issues/484)) ([77ef03e](https://github.com/matter-labs/zksync-os-server/commit/77ef03e9ae41e5e667c816ad39061847da001f70))
* ERC20 integration tests ([#285](https://github.com/matter-labs/zksync-os-server/issues/285)) ([4d42ecc](https://github.com/matter-labs/zksync-os-server/commit/4d42ecc1ffa8f8c75bf2c9e5ee1505c75bee6801))
* eth_call state overrides ([#539](https://github.com/matter-labs/zksync-os-server/issues/539)) ([2da5997](https://github.com/matter-labs/zksync-os-server/commit/2da5997991783f7eb90a34c258cd269f81c8ad73))
* eth_estimateGas state overrides ([#560](https://github.com/matter-labs/zksync-os-server/issues/560)) ([e671ae9](https://github.com/matter-labs/zksync-os-server/commit/e671ae9ad56e8b65f23e9e9a82dd2f94bba60e90))
* Execution of service interop transactions ([#803](https://github.com/matter-labs/zksync-os-server/issues/803)) ([b38c408](https://github.com/matter-labs/zksync-os-server/commit/b38c40875b5f7b0d297db3a6ea7e469b322e997a))
* external node can read previous replay version ([#224](https://github.com/matter-labs/zksync-os-server/issues/224)) ([054c45d](https://github.com/matter-labs/zksync-os-server/commit/054c45d73d2609c193af516937df5bdbd1608da3))
* folder with risc-v binaries + handle protocol version in batch components ([#369](https://github.com/matter-labs/zksync-os-server/issues/369)) ([27232e0](https://github.com/matter-labs/zksync-os-server/commit/27232e0d078575d0b1b3ff037be668d3bc6dd90f))
* **genesis:** Add genesis root hash to genesis.json ([#494](https://github.com/matter-labs/zksync-os-server/issues/494)) ([5a2fa1b](https://github.com/matter-labs/zksync-os-server/commit/5a2fa1ba4151fe26dfeb74315b6a2652df498af7))
* **genesis:** derive execution_version from protocol version, remove from genesis.json ([#940](https://github.com/matter-labs/zksync-os-server/issues/940)) ([4c62d1b](https://github.com/matter-labs/zksync-os-server/commit/4c62d1b8980017bf801b1fadf5919d19c77fd89e))
* get rid of `Source`/`Sink` ([#461](https://github.com/matter-labs/zksync-os-server/issues/461)) ([b338129](https://github.com/matter-labs/zksync-os-server/commit/b338129254814f1f332426bee801e68b31c5796e))
* get rid of batch rescheduling (preparation to get rid of BatchStorage) ([#587](https://github.com/matter-labs/zksync-os-server/issues/587)) ([bd1348b](https://github.com/matter-labs/zksync-os-server/commit/bd1348be9599566a2675bff56c45695c095f97b4))
* get rid of l1_gas_pricing_multiplier ([#576](https://github.com/matter-labs/zksync-os-server/issues/576)) ([d882cd4](https://github.com/matter-labs/zksync-os-server/commit/d882cd425cde6a4759bd92b04b9b4deec2f2293a))
* handle reorgs for EN ([#610](https://github.com/matter-labs/zksync-os-server/issues/610)) ([28a0a54](https://github.com/matter-labs/zksync-os-server/commit/28a0a542667a6bf66c2beb72e33e73217a5ebf74))
* have all user-facing config values in one file ([#349](https://github.com/matter-labs/zksync-os-server/issues/349)) ([41b6314](https://github.com/matter-labs/zksync-os-server/commit/41b6314f8c2ba0c8724ce53be22004cd8753050b))
* ignore vulnerability to recover cargo-audit ([#754](https://github.com/matter-labs/zksync-os-server/issues/754)) ([9baf143](https://github.com/matter-labs/zksync-os-server/commit/9baf143d342b25ab33d23fadf11bb431d1169a97))
* implement `debug_traceCall` ([#359](https://github.com/matter-labs/zksync-os-server/issues/359)) ([ed00bbc](https://github.com/matter-labs/zksync-os-server/commit/ed00bbc2fd7f0c4d17a8317f4d4715084bbaad51))
* Implement interop system transaction ([#712](https://github.com/matter-labs/zksync-os-server/issues/712)) ([26bfd0c](https://github.com/matter-labs/zksync-os-server/commit/26bfd0cf05994b87fdc4e201a1244fb2d357dff7))
* improve debug logging ([#401](https://github.com/matter-labs/zksync-os-server/issues/401)) ([2adcc92](https://github.com/matter-labs/zksync-os-server/commit/2adcc92860a3b4cf5f39dbf203afa369bd11031b))
* index reverted blocks by hash ([#867](https://github.com/matter-labs/zksync-os-server/issues/867)) ([2690317](https://github.com/matter-labs/zksync-os-server/commit/2690317f8472063fb62e7ebb72cd545fb53f1653))
* Interop roots watcher ([#819](https://github.com/matter-labs/zksync-os-server/issues/819)) ([6c5de83](https://github.com/matter-labs/zksync-os-server/commit/6c5de830a6320c4e872c027f110b64cf6d672e4b))
* introduce `CommittedBatchProvider` ([#764](https://github.com/matter-labs/zksync-os-server/issues/764)) ([71b11a1](https://github.com/matter-labs/zksync-os-server/commit/71b11a1871465b5c74351b9866ab3642e69a1aa5))
* JS tracer ([#569](https://github.com/matter-labs/zksync-os-server/issues/569)) ([3c852e1](https://github.com/matter-labs/zksync-os-server/commit/3c852e1b33d016033aebc4f67120e9b4ff32efd7))
* **l1_watcher:** Make l1 watcher processor-agnostic ([#634](https://github.com/matter-labs/zksync-os-server/issues/634)) ([552a59e](https://github.com/matter-labs/zksync-os-server/commit/552a59e57cbb2ccc7631211448a332d915030621))
* **l1-sender:** send EIP-7594 blobs when Fusaka is activated ([#664](https://github.com/matter-labs/zksync-os-server/issues/664)) ([7691422](https://github.com/matter-labs/zksync-os-server/commit/7691422d74a341cff14dda43c11630650d936058))
* **l1-sender:** use alloy-based tx inclusion ([#541](https://github.com/matter-labs/zksync-os-server/issues/541)) ([bcd4d4f](https://github.com/matter-labs/zksync-os-server/commit/bcd4d4fe1946cd74fe2bf43d74c37584c234b80f))
* **l1-sender:** wait for pending state to finalize ([#311](https://github.com/matter-labs/zksync-os-server/issues/311)) ([01ea574](https://github.com/matter-labs/zksync-os-server/commit/01ea574a5faef3a8fc1668d8f1bfbc0ed0b383e5))
* **l1-watcher:** monitor `ReportCommittedBatchRangeZKsyncOS` events ([#661](https://github.com/matter-labs/zksync-os-server/issues/661)) ([8ccd29a](https://github.com/matter-labs/zksync-os-server/commit/8ccd29a1e5051c9a70efee23f820f53b6612d8ef))
* **l1-watcher:** move pagination/polling into shared component ([#548](https://github.com/matter-labs/zksync-os-server/issues/548)) ([52f84dd](https://github.com/matter-labs/zksync-os-server/commit/52f84dd84a9d8cc23094ececb33dc48c9daeafaf))
* **l1-watcher:** poll events actively when behind ([#523](https://github.com/matter-labs/zksync-os-server/issues/523)) ([cbc76bb](https://github.com/matter-labs/zksync-os-server/commit/cbc76bb8fac0baede6a9f85c57c8eb49fe559b80))
* **l1-watcher:** track last committed/executed batch in finality ([#485](https://github.com/matter-labs/zksync-os-server/issues/485)) ([85a7669](https://github.com/matter-labs/zksync-os-server/commit/85a7669447946ee0e9a4b87be5a6229ba4d8420b))
* **l1:** move `{Commit,Stored}BatchInfo` + introduce `BatchInfo` ([#505](https://github.com/matter-labs/zksync-os-server/issues/505)) ([3658862](https://github.com/matter-labs/zksync-os-server/commit/36588629e44dc6104b3775ea7d05e425b72a0de7))
* **l1:** move L1 discovery out of `L1Sender` ([#502](https://github.com/matter-labs/zksync-os-server/issues/502)) ([4d2d2fa](https://github.com/matter-labs/zksync-os-server/commit/4d2d2fa656997d0b39556e81537720493b914c2a))
* **l1:** optimistic RPC retry policy ([#385](https://github.com/matter-labs/zksync-os-server/issues/385)) ([14ba68b](https://github.com/matter-labs/zksync-os-server/commit/14ba68b3bc05100bde116fa0bafca2fb79dcb841))
* **l1:** retry RPC requests on internal error ([#496](https://github.com/matter-labs/zksync-os-server/issues/496)) ([4628321](https://github.com/matter-labs/zksync-os-server/commit/46283210b83461bfab011a0595c7e8296035aff4))
* make batcher skip blocks that are already processed ([#404](https://github.com/matter-labs/zksync-os-server/issues/404)) ([6943ea6](https://github.com/matter-labs/zksync-os-server/commit/6943ea6ba06d2445148864ea138f91074dada2f3))
* make block-related logging consistent ([#792](https://github.com/matter-labs/zksync-os-server/issues/792)) ([83f64ce](https://github.com/matter-labs/zksync-os-server/commit/83f64ce32035ebd489549b74f8319d30277927e3))
* make bytecode supplier address config value optional ([#735](https://github.com/matter-labs/zksync-os-server/issues/735)) ([7ac39c9](https://github.com/matter-labs/zksync-os-server/commit/7ac39c99b0d4a707721bfe12ac48fd5f8c513f79))
* make EN replay streams HTTP 1.0 ([#341](https://github.com/matter-labs/zksync-os-server/issues/341)) ([cbcedd6](https://github.com/matter-labs/zksync-os-server/commit/cbcedd6a7346c5a6bd692b250b78dbb436c3645c))
* make mempool configurable ([#464](https://github.com/matter-labs/zksync-os-server/issues/464)) ([8d4c7e6](https://github.com/matter-labs/zksync-os-server/commit/8d4c7e6fda0bb5f38c73a3728857b80a9f92e5cb))
* make operator signing keys optional for External Nodes ([#929](https://github.com/matter-labs/zksync-os-server/issues/929)) ([d5af054](https://github.com/matter-labs/zksync-os-server/commit/d5af054b3d9142e494c89c03325e16751c60d3e8))
* make pipelines repository-agnostic ([#536](https://github.com/matter-labs/zksync-os-server/issues/536)) ([1f145ba](https://github.com/matter-labs/zksync-os-server/commit/1f145ba3d95d711178290ea43a4f647ff9a12af9))
* **mempool-config:** make minimal_protocol_basefee configurable ([#671](https://github.com/matter-labs/zksync-os-server/issues/671)) ([1c11bfc](https://github.com/matter-labs/zksync-os-server/commit/1c11bfca3d64d0d2d606166d7a59d7ca2b9d58f1))
* **mempool:** export even more metrics ([#529](https://github.com/matter-labs/zksync-os-server/issues/529)) ([5f339b0](https://github.com/matter-labs/zksync-os-server/commit/5f339b08da228cc765133d070abf5ad9eb01d73f))
* **mempool:** expose metrics ([#522](https://github.com/matter-labs/zksync-os-server/issues/522)) ([1487d08](https://github.com/matter-labs/zksync-os-server/commit/1487d08735e0fe3c02d149cda9338cb6956bcbe9))
* **mempool:** rewrite via in-memory subpools ([#869](https://github.com/matter-labs/zksync-os-server/issues/869)) ([76ca804](https://github.com/matter-labs/zksync-os-server/commit/76ca8047e0d0126b85e7a13b3c1f0cd0bcbd2c85))
* **merkle-tree:** Implement storage proofs for `zks_getProof` ([#904](https://github.com/matter-labs/zksync-os-server/issues/904)) ([dcccfda](https://github.com/matter-labs/zksync-os-server/commit/dcccfda583569c95dbf41014a91eff6883b7f4b9))
* metric for tx execution ([#323](https://github.com/matter-labs/zksync-os-server/issues/323)) ([378c643](https://github.com/matter-labs/zksync-os-server/commit/378c6439e9144e6e18c46be0b51ae4a510ba3e38))
* **minor:** small logging and test cleanups ([#1057](https://github.com/matter-labs/zksync-os-server/issues/1057)) ([0b997a8](https://github.com/matter-labs/zksync-os-server/commit/0b997a8a922410664b7afc0a7a60c9fa69d38d74))
* more granular buckets for `prove_time_per_million_native` ([#763](https://github.com/matter-labs/zksync-os-server/issues/763)) ([3f34fc8](https://github.com/matter-labs/zksync-os-server/commit/3f34fc8ed20f8b359829f65043d6d9ab76209e49))
* more metrics ([188d213](https://github.com/matter-labs/zksync-os-server/commit/188d2131120da9238bc79a6e5fd3d547a00ee00b))
* more metrics and logs - gas per second, transaction status ([#415](https://github.com/matter-labs/zksync-os-server/issues/415)) ([a397af2](https://github.com/matter-labs/zksync-os-server/commit/a397af23e492c4a73aa2b3ffc55b94860313df9c))
* move sequencer implementation to its own crate ([#291](https://github.com/matter-labs/zksync-os-server/issues/291)) ([11c2d4a](https://github.com/matter-labs/zksync-os-server/commit/11c2d4a9cb5753457183375c458e284ca1b61a7a))
* **multivm:** use in-memory app bins for PIG ([#1037](https://github.com/matter-labs/zksync-os-server/issues/1037)) ([78616a3](https://github.com/matter-labs/zksync-os-server/commit/78616a3e6051f91b9b3587ca63a6b39bfc247d0a))
* **multivm:** use v0.2.6-simulate-only for V5 simulation ([#855](https://github.com/matter-labs/zksync-os-server/issues/855)) ([f3f8bfb](https://github.com/matter-labs/zksync-os-server/commit/f3f8bfbba5a1044cec5af9a2d89bcee0578e54b6))
* **network:** add runnable `NetworkService` (disabled by default) ([#773](https://github.com/matter-labs/zksync-os-server/issues/773)) ([6dd32e8](https://github.com/matter-labs/zksync-os-server/commit/6dd32e8558b8da43ad5186505d4aedfff7cfe403))
* **network:** bounded channel + shared starting block state ([#884](https://github.com/matter-labs/zksync-os-server/issues/884)) ([34dffed](https://github.com/matter-labs/zksync-os-server/commit/34dffed0d534cc7765812c20f7722025d4c9d1d7))
* **network:** fully migrate replay transport to p2p network ([#873](https://github.com/matter-labs/zksync-os-server/issues/873)) ([dd9e3bb](https://github.com/matter-labs/zksync-os-server/commit/dd9e3bb5f7f2f3d68eada833d6fa5e22f44e44bb))
* **network:** implement bare-bones `zks` RLPx subprotocol ([#716](https://github.com/matter-labs/zksync-os-server/issues/716)) ([ba59c9f](https://github.com/matter-labs/zksync-os-server/commit/ba59c9fd5b499cde7e81030dbf2c31597c4ff1dd))
* **network:** report metrics from `reth-network` crate ([#1063](https://github.com/matter-labs/zksync-os-server/issues/1063)) ([ce8e225](https://github.com/matter-labs/zksync-os-server/commit/ce8e225ec70db8472bfe952d0fbd396a88a27c3a))
* **network:** support `network_interface` and DNS boot nodes ([#1075](https://github.com/matter-labs/zksync-os-server/issues/1075)) ([6b2213c](https://github.com/matter-labs/zksync-os-server/commit/6b2213ca38d473c72c23084ac823d06dc31946d2))
* **network:** use chain-aware fork id for filtering discv5 peers ([#1051](https://github.com/matter-labs/zksync-os-server/issues/1051)) ([806348a](https://github.com/matter-labs/zksync-os-server/commit/806348a842b0e1e968f160e14e3a0b92d9a89d3e))
* **network:** use real HTTP server/client for batch verification ([#737](https://github.com/matter-labs/zksync-os-server/issues/737)) ([ac5488c](https://github.com/matter-labs/zksync-os-server/commit/ac5488c321ea36146612935e74cada495d826cce))
* **network:** use real HTTP server/client for replay transport ([#729](https://github.com/matter-labs/zksync-os-server/issues/729)) ([76a8434](https://github.com/matter-labs/zksync-os-server/commit/76a8434b02ddb32db7e293024cdb7f5de1c2dc8d))
* new state impl ([#278](https://github.com/matter-labs/zksync-os-server/issues/278)) ([462745d](https://github.com/matter-labs/zksync-os-server/commit/462745d284d994500bf490c5598200cf84dc2b9c))
* Opentelemetry support + config schema change ([#559](https://github.com/matter-labs/zksync-os-server/issues/559)) ([c2335de](https://github.com/matter-labs/zksync-os-server/commit/c2335de232993cb8322cd6e1a2d930013f676b08))
* Peek batch data from State ([#458](https://github.com/matter-labs/zksync-os-server/issues/458)) ([aec1dfa](https://github.com/matter-labs/zksync-os-server/commit/aec1dfaf098bd10ca01b5d00f465f8d1c09badcb))
* Peek FRI Proofs from ProofStorage ([#470](https://github.com/matter-labs/zksync-os-server/issues/470)) ([4fe8a9f](https://github.com/matter-labs/zksync-os-server/commit/4fe8a9fa2bbd612fb1a9e5e317136fe1dc8f2108))
* pipeline framework (1/X) - tree, sequencer and prover_input_gen ([#447](https://github.com/matter-labs/zksync-os-server/issues/447)) ([e83142b](https://github.com/matter-labs/zksync-os-server/commit/e83142bd3510052033a13003013dd439377324a1))
* pipeline framework (3/X) - migrate FriJobManager ([#465](https://github.com/matter-labs/zksync-os-server/issues/465)) ([f9b4e7c](https://github.com/matter-labs/zksync-os-server/commit/f9b4e7c268ae9cd4328fbeaa63ffe68e152ce5ed))
* pipeline framework (4/X): migrate gapless committer ([#467](https://github.com/matter-labs/zksync-os-server/issues/467)) ([de606a5](https://github.com/matter-labs/zksync-os-server/commit/de606a5fdf19a71ea1732c24d1731a9ff52d605d))
* pipeline framework (5/X) - migrate l1 committer ([#472](https://github.com/matter-labs/zksync-os-server/issues/472)) ([0742209](https://github.com/matter-labs/zksync-os-server/commit/074220991386e6a26467fdd703d2157faf1c77fc))
* pipeline framework (8/X) - migrate executor l1 and batch sink ([#481](https://github.com/matter-labs/zksync-os-server/issues/481)) ([d8e8fd1](https://github.com/matter-labs/zksync-os-server/commit/d8e8fd1aaa40e4eb5a81383e8abfaf4449401c85))
* pipeline framework (PR 2/X) - `pipe()` syntax; consume `self`; migrate batcher ([#448](https://github.com/matter-labs/zksync-os-server/issues/448)) ([d77c32b](https://github.com/matter-labs/zksync-os-server/commit/d77c32b582124ce0a3574fcfb14af32ebb43d7e3))
* pipeline framework PR 6/X - migrate l1 sender proves and SnarkJobsManager ([#477](https://github.com/matter-labs/zksync-os-server/issues/477)) ([70d91e3](https://github.com/matter-labs/zksync-os-server/commit/70d91e328e86c03da1aa502cfcab51fc73855dd5))
* pipeline framework PR 7/X - priority tree migrated ([#479](https://github.com/matter-labs/zksync-os-server/issues/479)) ([3966024](https://github.com/matter-labs/zksync-os-server/commit/39660249ad388244d2d4f394bd1f237ac432d892))
* proper gateway settlement and local gateway setup ([#919](https://github.com/matter-labs/zksync-os-server/issues/919)) ([4e2efd8](https://github.com/matter-labs/zksync-os-server/commit/4e2efd8845b3572b2b80cf2e01859dd2f7bfeabc))
* Protocol upgrade support for provers ([#577](https://github.com/matter-labs/zksync-os-server/issues/577)) ([146cfca](https://github.com/matter-labs/zksync-os-server/commit/146cfcad33a65878d9f5860dc6215382f6557784))
* protocol upgrade v0.30.1 (zksync-os v0.2.5) ([#743](https://github.com/matter-labs/zksync-os-server/issues/743)) ([a5a8269](https://github.com/matter-labs/zksync-os-server/commit/a5a826937dab247e2dd728628456effe0c068331))
* Protocol upgrade v1.1 ([#487](https://github.com/matter-labs/zksync-os-server/issues/487)) ([1afa4dd](https://github.com/matter-labs/zksync-os-server/commit/1afa4dd71032c3615b27bbefd587de9020e81b93))
* pubdata price calculation ([#549](https://github.com/matter-labs/zksync-os-server/issues/549)) ([5078aeb](https://github.com/matter-labs/zksync-os-server/commit/5078aeb2f30bea9ef484b207d5fdb80dfca8b626))
* re-implement alloy tx types ([#438](https://github.com/matter-labs/zksync-os-server/issues/438)) ([1f75cab](https://github.com/matter-labs/zksync-os-server/commit/1f75cabb4080f72f898ed5fefbe5913cf796466a))
* Read force deploys from a file ([#612](https://github.com/matter-labs/zksync-os-server/issues/612)) ([9f9608a](https://github.com/matter-labs/zksync-os-server/commit/9f9608a15a97014a713f83ee75f490c07ed5eb55))
* **readctor `ReplayRecord`:** extract `BlockStartCursors` struct from flat cursor fields (eg `l1_priority_id`) ([#1034](https://github.com/matter-labs/zksync-os-server/issues/1034)) ([1fa42c5](https://github.com/matter-labs/zksync-os-server/commit/1fa42c5c6c7610b46c198e030196da8c41746645))
* record prove time per native ([#757](https://github.com/matter-labs/zksync-os-server/issues/757)) ([3efb9ed](https://github.com/matter-labs/zksync-os-server/commit/3efb9ed81bac1225db1cc616787ab6bbd3999a1f))
* refactor lib.rs in sequencer ([#280](https://github.com/matter-labs/zksync-os-server/issues/280)) ([a226546](https://github.com/matter-labs/zksync-os-server/commit/a2265468437db5b8c1af9b6496e889a823ad06c6))
* refactor priority tree ([#483](https://github.com/matter-labs/zksync-os-server/issues/483)) ([83fef3a](https://github.com/matter-labs/zksync-os-server/commit/83fef3a15f8644c970782136ec773192d2ca0991))
* refine component state tracking ([#256](https://github.com/matter-labs/zksync-os-server/issues/256)) ([7cec41c](https://github.com/matter-labs/zksync-os-server/commit/7cec41c106f23db8d2aeb70136b6f850068f2dce))
* remove app_bin_unpack_path from config ([#588](https://github.com/matter-labs/zksync-os-server/issues/588)) ([a6ccd63](https://github.com/matter-labs/zksync-os-server/commit/a6ccd63674056d43c9e8781d1a4d4694bf881d87))
* Remove deprecated legacy prover API ([#674](https://github.com/matter-labs/zksync-os-server/issues/674)) ([103c43a](https://github.com/matter-labs/zksync-os-server/commit/103c43a1793d24daff07e77ee05a759a850cd415))
* remove failed transcations from block_output.tx_results ([#714](https://github.com/matter-labs/zksync-os-server/issues/714)) ([87cb85c](https://github.com/matter-labs/zksync-os-server/commit/87cb85cefc0928b8879484fa98de60ee0fd7fa69))
* remove hardcoded config constants ([#762](https://github.com/matter-labs/zksync-os-server/issues/762)) ([b0f9ba3](https://github.com/matter-labs/zksync-os-server/commit/b0f9ba340d8a70226274c5dcb18a082a8e3155fa))
* replace str with module name for app bin unpack path ([#516](https://github.com/matter-labs/zksync-os-server/issues/516)) ([5566289](https://github.com/matter-labs/zksync-os-server/commit/5566289077cc9fb55de9262432eaf768435cf3d0))
* return zeroes in `reward` in `eth_feeHistory` ([#800](https://github.com/matter-labs/zksync-os-server/issues/800)) ([1dc6aef](https://github.com/matter-labs/zksync-os-server/commit/1dc6aef7c314ef4a240f08a0d2445f476e20cca7))
* Revert "feat: adjust pubdata price based on blob fill ratio" ([#753](https://github.com/matter-labs/zksync-os-server/issues/753)) ([a215e1f](https://github.com/matter-labs/zksync-os-server/commit/a215e1fb1a352704a076c50c902ddcf5a82c754d))
* revm consistency checker ([#525](https://github.com/matter-labs/zksync-os-server/issues/525)) ([f117e0e](https://github.com/matter-labs/zksync-os-server/commit/f117e0eca574a9a41923ca2fe8610dc5dee55c12))
* RPC monitoring middleware ([#306](https://github.com/matter-labs/zksync-os-server/issues/306)) ([8dc6c28](https://github.com/matter-labs/zksync-os-server/commit/8dc6c28313e1bd430b5d73bb7a348d85b4715f3a))
* **rpc:** add gatewayBlockNumber to zks_getL2ToL1LogProof response ([#1064](https://github.com/matter-labs/zksync-os-server/issues/1064)) ([24dacc0](https://github.com/matter-labs/zksync-os-server/commit/24dacc0e754e3125f9e028c9853c45d3440a1893))
* **rpc:** Add zks_getBlockMetadataByNumber ([#724](https://github.com/matter-labs/zksync-os-server/issues/724)) ([dfbd534](https://github.com/matter-labs/zksync-os-server/commit/dfbd534632f213d8eb8e75c15a0653054e4a5ada))
* **rpc:** Additional format of l2_to_l1_log_proof ([#964](https://github.com/matter-labs/zksync-os-server/issues/964)) ([ffec632](https://github.com/matter-labs/zksync-os-server/commit/ffec63299af3739c7ad27cd7bf394d9dea69961c))
* **rpc:** implement `web3` namespace ([#497](https://github.com/matter-labs/zksync-os-server/issues/497)) ([58c80f8](https://github.com/matter-labs/zksync-os-server/commit/58c80f815c7fc9d1c5902d7e35325cc32eb93e82))
* **rpc:** Implement `zks_getProof` ([#917](https://github.com/matter-labs/zksync-os-server/issues/917)) ([7cc605d](https://github.com/matter-labs/zksync-os-server/commit/7cc605d0dcb9f948fcd2913e8aeb5d12fd073c32))
* **rpc:** track JSON-RPC error counts by method and error code ([#1040](https://github.com/matter-labs/zksync-os-server/issues/1040)) ([ee455ec](https://github.com/matter-labs/zksync-os-server/commit/ee455ecc099791c0a9f987375443f00dd1bdc7ca))
* **rpc:** use pubdata price factor during gas estimation ([#669](https://github.com/matter-labs/zksync-os-server/issues/669)) ([4b2b6d3](https://github.com/matter-labs/zksync-os-server/commit/4b2b6d344bddcc1f9ce21280022888a9de1cce48))
* Saving failed proofs to bucket and exposing endpoint to get them ([#507](https://github.com/matter-labs/zksync-os-server/issues/507)) ([d695e1d](https://github.com/matter-labs/zksync-os-server/commit/d695e1de1f510513f8d7a7cb3038092e8322f9e6))
* scale eth_gasPrice by configurable factor ([#957](https://github.com/matter-labs/zksync-os-server/issues/957)) ([89874ca](https://github.com/matter-labs/zksync-os-server/commit/89874cab956f9a5df79b9fd20a60ddb2db409a20))
* **sentry:** Use CLUSTER_NAME as environment tag ([#570](https://github.com/matter-labs/zksync-os-server/issues/570)) ([af11c09](https://github.com/matter-labs/zksync-os-server/commit/af11c0901922293d39820bbe44991f67a82a1591))
* **sequencer:** validate last 256 blocks for replayed blocks ([#524](https://github.com/matter-labs/zksync-os-server/issues/524)) ([85b0a53](https://github.com/matter-labs/zksync-os-server/commit/85b0a537e1b239731cfcf887897aab7afbd9610a))
* set default block time to 250ms ([#598](https://github.com/matter-labs/zksync-os-server/issues/598)) ([a65641b](https://github.com/matter-labs/zksync-os-server/commit/a65641b324b86f146c153fce61bf6daadc9373b1))
* set gas per pubdata to `1` ([#406](https://github.com/matter-labs/zksync-os-server/issues/406)) ([8abd288](https://github.com/matter-labs/zksync-os-server/commit/8abd288046597923f25004cf98622260d64122b9))
* set pubdata price to `1` ([#476](https://github.com/matter-labs/zksync-os-server/issues/476)) ([0b81bab](https://github.com/matter-labs/zksync-os-server/commit/0b81babf385af983246b607a957161e1b781a91f))
* set sensible global debug levels ([#600](https://github.com/matter-labs/zksync-os-server/issues/600)) ([2f20948](https://github.com/matter-labs/zksync-os-server/commit/2f2094811d6b11acc0e2ed1b5fb7f3622946596a))
* Set SL chain id txs ([#849](https://github.com/matter-labs/zksync-os-server/issues/849)) ([67f5b37](https://github.com/matter-labs/zksync-os-server/commit/67f5b378d5fa7b8ecdbc93b003cd5b5380b44d03))
* set total difficulty in rpc block headers ([#801](https://github.com/matter-labs/zksync-os-server/issues/801)) ([95a5244](https://github.com/matter-labs/zksync-os-server/commit/95a5244abfafaf807295144edcf865673beadec7))
* some gateway features ([#886](https://github.com/matter-labs/zksync-os-server/issues/886)) ([fa25529](https://github.com/matter-labs/zksync-os-server/commit/fa25529777033a21c5153ac16795fd5992b08559))
* speed-up batch storage lookup ([#273](https://github.com/matter-labs/zksync-os-server/issues/273)) ([45ec18e](https://github.com/matter-labs/zksync-os-server/commit/45ec18ec1ef36c2e70734c2dfcfdd414618f0ff8))
* split l1_state metrics; fix typo in l1_sender metrics ([#357](https://github.com/matter-labs/zksync-os-server/issues/357)) ([40dc6f8](https://github.com/matter-labs/zksync-os-server/commit/40dc6f87bbcd0b6ea692ccdb135dc8f41fb0cfe1))
* **storage:** add `ReadStateHistory` trait ([#244](https://github.com/matter-labs/zksync-os-server/issues/244)) ([6c7c8ae](https://github.com/matter-labs/zksync-os-server/commit/6c7c8aefc4e09d32f8ce584fe543e4db4d0d895e))
* **storage:** move replay DB to storage crate ([#535](https://github.com/matter-labs/zksync-os-server/issues/535)) ([73a0842](https://github.com/matter-labs/zksync-os-server/commit/73a08423a980320a24b9d7d5061d621f83dc5ba6))
* Store FRI proofs locally, not in S3 ([#891](https://github.com/matter-labs/zksync-os-server/issues/891)) ([fc81561](https://github.com/matter-labs/zksync-os-server/commit/fc81561bfab437e4ab305b760d5d85273f9f3233))
* store gzip-compressed anvil states ([#837](https://github.com/matter-labs/zksync-os-server/issues/837)) ([40b216a](https://github.com/matter-labs/zksync-os-server/commit/40b216a8c17f8ed3d1c60f45fa08b515e53f81ff))
* support JSON config files ([#752](https://github.com/matter-labs/zksync-os-server/issues/752)) ([c75ad3b](https://github.com/matter-labs/zksync-os-server/commit/c75ad3bd72e97d4283a0e6f2bfbfc4c3cc040dee))
* support L1-&gt;L2 tx gas estimation ([#370](https://github.com/matter-labs/zksync-os-server/issues/370)) ([67f0524](https://github.com/matter-labs/zksync-os-server/commit/67f05240380c2cd7abeb9c05a0813e6c7c6942ca))
* support multiple config files ([#866](https://github.com/matter-labs/zksync-os-server/issues/866)) ([b6c6e5b](https://github.com/matter-labs/zksync-os-server/commit/b6c6e5b2dda1aa21a2d3cb5a38f477f87b4aa020))
* support multiple SNARKers; enhance proving observability ([#631](https://github.com/matter-labs/zksync-os-server/issues/631)) ([0e86bb8](https://github.com/matter-labs/zksync-os-server/commit/0e86bb81b36d8a6a9b500e6b2e5821d1b9da669b))
* support zksync-os v0.1.0 ([#557](https://github.com/matter-labs/zksync-os-server/issues/557)) ([a3d2373](https://github.com/matter-labs/zksync-os-server/commit/a3d2373382b511237620828e33b791cfe9f15eac))
* Sync l1 state with draft-v31 ([#1010](https://github.com/matter-labs/zksync-os-server/issues/1010)) ([c486d02](https://github.com/matter-labs/zksync-os-server/commit/c486d02b431f1bd27b075c0622170c0253cc2a21))
* token price updater component ([#779](https://github.com/matter-labs/zksync-os-server/issues/779)) ([f5df15a](https://github.com/matter-labs/zksync-os-server/commit/f5df15a7e49975ccc0f0c85191091b06142be2e0))
* **tracer:** Add error message for out-of-native ([#720](https://github.com/matter-labs/zksync-os-server/issues/720)) ([e82f5a3](https://github.com/matter-labs/zksync-os-server/commit/e82f5a3d21d530a3ced380ad00f25785e520a661))
* **tracer:** Meaningful errors for out-of-pubdata reverts ([#1058](https://github.com/matter-labs/zksync-os-server/issues/1058)) ([8182e6f](https://github.com/matter-labs/zksync-os-server/commit/8182e6f33850280e465d01adef7cb522e28e53ea))
* track `execution_version` in genesis config ([#498](https://github.com/matter-labs/zksync-os-server/issues/498)) ([1e0d8b5](https://github.com/matter-labs/zksync-os-server/commit/1e0d8b5ed78d2675aa5bf6fd9bfd60ecdc97343b))
* **tx_validators:** add deployment filter to restrict contract deployments to an allow-list ([#1013](https://github.com/matter-labs/zksync-os-server/issues/1013)) ([45e2c51](https://github.com/matter-labs/zksync-os-server/commit/45e2c511d3ed18ffe2b61d8199b392602f65e522))
* update l1 contracts interface ([#339](https://github.com/matter-labs/zksync-os-server/issues/339)) ([d034f1e](https://github.com/matter-labs/zksync-os-server/commit/d034f1e7cb87161902be1fb038e625f8d9d949f1))
* update rustc version; use prover binary in test ([#901](https://github.com/matter-labs/zksync-os-server/issues/901)) ([a534c91](https://github.com/matter-labs/zksync-os-server/commit/a534c918489e510c2a1d5ba34901b8a1c7e88d77))
* Update state - contracts from zkos-0.29.5 + scripts changes ([#356](https://github.com/matter-labs/zksync-os-server/issues/356)) ([5d71ed7](https://github.com/matter-labs/zksync-os-server/commit/5d71ed77c431b9ef008e9b1d45285528165477e1))
* Update state - contracts: zkos-v0.29.2, zkstack tool: 0267d99b366c97 ([#305](https://github.com/matter-labs/zksync-os-server/issues/305)) ([56de25d](https://github.com/matter-labs/zksync-os-server/commit/56de25d55fbc554835566cc8b949e0382608e591))
* Update state - contracts: zkos-v0.29.6, zkstack tool: origin/main ([#364](https://github.com/matter-labs/zksync-os-server/issues/364)) ([f75df47](https://github.com/matter-labs/zksync-os-server/commit/f75df476607525d7b6764513f8846fc491f2c5ef))
* update to zkos v0.0.20 and airbender 0.4.3 ([#301](https://github.com/matter-labs/zksync-os-server/issues/301)) ([d0ca45f](https://github.com/matter-labs/zksync-os-server/commit/d0ca45ffb1b567ea321e7168fdf748447de956bf))
* update tracing-subscriber version ([#325](https://github.com/matter-labs/zksync-os-server/issues/325)) ([ae75321](https://github.com/matter-labs/zksync-os-server/commit/ae75321a4e123ab46a8a3ad1dcbe0c421d91c6f7))
* update zksync-os to v0.0.26 and interface to v0.0.7 ([#429](https://github.com/matter-labs/zksync-os-server/issues/429)) ([aaa2c17](https://github.com/matter-labs/zksync-os-server/commit/aaa2c17608a3b4eecab07add4619aa28d4fffc77))
* update zksync-os with p256 fix ([#642](https://github.com/matter-labs/zksync-os-server/issues/642)) ([8071374](https://github.com/matter-labs/zksync-os-server/commit/80713744b0d6bcf170c9396f4285e277935b120c))
* upgrade bincode to v2 ([#274](https://github.com/matter-labs/zksync-os-server/issues/274)) ([a8e3d46](https://github.com/matter-labs/zksync-os-server/commit/a8e3d462151f30110648d6f1cd24cf9c4f331756))
* upgrade reth to 1.9.3/revm to 31.0.2 ([#709](https://github.com/matter-labs/zksync-os-server/issues/709)) ([7cd8dec](https://github.com/matter-labs/zksync-os-server/commit/7cd8dec98eb40406595efafe501cefcf0cfeb13f))
* upgrade smart-config to 0.4.0; simplify parsing ([#644](https://github.com/matter-labs/zksync-os-server/issues/644)) ([409e00f](https://github.com/matter-labs/zksync-os-server/commit/409e00fd4baa850d8d4293f65610fcaf48fa2de6))
* upgrade system (part 1 of N) ([#582](https://github.com/matter-labs/zksync-os-server/issues/582)) ([708510d](https://github.com/matter-labs/zksync-os-server/commit/708510d38a8d53b253bb984f7cbe750a653ba141))
* upgrade system (part 2 of N) ([#609](https://github.com/matter-labs/zksync-os-server/issues/609)) ([0153272](https://github.com/matter-labs/zksync-os-server/commit/0153272878647a8ce3bbe5568d1e7d5b0b96967c))
* Use gateway base token as SL token ([#1042](https://github.com/matter-labs/zksync-os-server/issues/1042)) ([e149ee6](https://github.com/matter-labs/zksync-os-server/commit/e149ee65afcf4025b2310b0842b8ccc68324457d))
* use max_priority_fee_per_gas config value as cap on the priority fee used ([#857](https://github.com/matter-labs/zksync-os-server/issues/857)) ([ac66d41](https://github.com/matter-labs/zksync-os-server/commit/ac66d413b2448fc82ebed55792c04c11b90dc2fe))
* use newer version of zkyns-os-revm ([#798](https://github.com/matter-labs/zksync-os-server/issues/798)) ([3289065](https://github.com/matter-labs/zksync-os-server/commit/3289065f2c90d2769103344244493a0671a40c52))
* use open source prover ([#300](https://github.com/matter-labs/zksync-os-server/issues/300)) ([4a5d933](https://github.com/matter-labs/zksync-os-server/commit/4a5d933f872485fc72e7277d0dafb5744a98f94d))
* use token prices in fee model ([#787](https://github.com/matter-labs/zksync-os-server/issues/787)) ([298088b](https://github.com/matter-labs/zksync-os-server/commit/298088b2ba183e566741a5d17554b6e8a3acd53f))
* v30 zksync os protocol upgrade support ([#594](https://github.com/matter-labs/zksync-os-server/issues/594)) ([ac453e2](https://github.com/matter-labs/zksync-os-server/commit/ac453e2fc8ad3c18b4beb0fd4bd1ffa0df0c8e39))
* validate genesis batch info against L1 ([#832](https://github.com/matter-labs/zksync-os-server/issues/832)) ([041aa48](https://github.com/matter-labs/zksync-os-server/commit/041aa48292fa9625cb5dc7356b0fa73f1e065507))
* wait for tx in block context provider ([#478](https://github.com/matter-labs/zksync-os-server/issues/478)) ([603a56d](https://github.com/matter-labs/zksync-os-server/commit/603a56d0ab032d99bd3c7bb466ee2aa456cd1490))
* **zks_getProof:** add L1 verification data to proof response and CLI tool ([#1022](https://github.com/matter-labs/zksync-os-server/issues/1022)) ([4093ac7](https://github.com/matter-labs/zksync-os-server/commit/4093ac7a0c880a2ea9e5feef09ade8728839c7ca))
* zksync os bump to 0.0.13 ([#283](https://github.com/matter-labs/zksync-os-server/issues/283)) ([a111cb9](https://github.com/matter-labs/zksync-os-server/commit/a111cb96cd3c107a0e9b7a2e0f8f68ee4100f082))
* zksync os inteface/multivm ([#345](https://github.com/matter-labs/zksync-os-server/issues/345)) ([61d0ffd](https://github.com/matter-labs/zksync-os-server/commit/61d0ffd140a80c0b2c0ba0064f80d7925651ff6e))


### Bug Fixes

* `eth_getTransactionCount` takes mempool into account ([#360](https://github.com/matter-labs/zksync-os-server/issues/360)) ([0f84590](https://github.com/matter-labs/zksync-os-server/commit/0f84590f52bedf77fc33b794cfd33937898ac954))
* `zksync_os_types` compiles without features ([#815](https://github.com/matter-labs/zksync-os-server/issues/815)) ([e6e5233](https://github.com/matter-labs/zksync-os-server/commit/e6e5233f43125756c095f3d53566bed52c21046b))
* 2FA followup ([#662](https://github.com/matter-labs/zksync-os-server/issues/662)) ([3701187](https://github.com/matter-labs/zksync-os-server/commit/37011876b1a4b842bf89de6eb17c60fea5542557))
* add default v,r,s,yParity fields in L1TxType during serialization ([#500](https://github.com/matter-labs/zksync-os-server/issues/500)) ([92fb0b5](https://github.com/matter-labs/zksync-os-server/commit/92fb0b53dc89f26c9fe480fa76bc53243e843d7f))
* add forgotten state.compact_peridoically() ([#324](https://github.com/matter-labs/zksync-os-server/issues/324)) ([45649e0](https://github.com/matter-labs/zksync-os-server/commit/45649e0790e1382b0cfbdd6a57b8c6e35de68261))
* Add more metrics for 2FA ([#1001](https://github.com/matter-labs/zksync-os-server/issues/1001)) ([70229f2](https://github.com/matter-labs/zksync-os-server/commit/70229f23383d3cc63b1ac1da180f600a5ab8c52c))
* Add TxValidatorConfig to schema ([#475](https://github.com/matter-labs/zksync-os-server/issues/475)) ([22d6521](https://github.com/matter-labs/zksync-os-server/commit/22d65214cb61b0f079c5081327755768f76d7243))
* always replay at least one block ([#281](https://github.com/matter-labs/zksync-os-server/issues/281)) ([8240653](https://github.com/matter-labs/zksync-os-server/commit/82406536a409d8943915b3fd29e76bf1df8342b1))
* **api:** proper type id for txs in api ([#269](https://github.com/matter-labs/zksync-os-server/issues/269)) ([d2616af](https://github.com/matter-labs/zksync-os-server/commit/d2616afecf1fc9f928065199e45f5011f24e0dc7))
* Apply fixes for cargo deny ([#892](https://github.com/matter-labs/zksync-os-server/issues/892)) ([c2aa63f](https://github.com/matter-labs/zksync-os-server/commit/c2aa63fe5d0d58c4acab5656343b1bdbe8f43506))
* backward compatible deserialization for proofs ([#414](https://github.com/matter-labs/zksync-os-server/issues/414)) ([823b164](https://github.com/matter-labs/zksync-os-server/commit/823b164dc85d730971253d918ddd98b644ee7285))
* batch storage persist delay ([#1015](https://github.com/matter-labs/zksync-os-server/issues/1015)) ([1ae52b9](https://github.com/matter-labs/zksync-os-server/commit/1ae52b9e6f324a61b8d52e43e3ec66cd3bcaf976))
* batch verification config ([#654](https://github.com/matter-labs/zksync-os-server/issues/654)) ([417bde8](https://github.com/matter-labs/zksync-os-server/commit/417bde8c30d13cd265d12565308b1f2e734d4d0d))
* **batcher:** rebuild batches from S3 even when they are not committed ([#645](https://github.com/matter-labs/zksync-os-server/issues/645)) ([e0277a5](https://github.com/matter-labs/zksync-os-server/commit/e0277a570bad2e81e40cd3e67ca09af32dffeb2e))
* better recognition for missing `IMultisigCommitter` ([#852](https://github.com/matter-labs/zksync-os-server/issues/852)) ([702467e](https://github.com/matter-labs/zksync-os-server/commit/702467e3f9a2744d1e31fed86164a1cc73dcc48f))
* block count limit ([#297](https://github.com/matter-labs/zksync-os-server/issues/297)) ([f81bd33](https://github.com/matter-labs/zksync-os-server/commit/f81bd334297ebb16775d501c499fe0de26366281))
* Commit after each tx in revm consistency checker ([#898](https://github.com/matter-labs/zksync-os-server/issues/898)) ([b5283cb](https://github.com/matter-labs/zksync-os-server/commit/b5283cb2c9aef9b55a066615ceae8e4150f27008))
* Compare block hash during block replay ([#918](https://github.com/matter-labs/zksync-os-server/issues/918)) ([028021b](https://github.com/matter-labs/zksync-os-server/commit/028021b5431029654c90de76472ff52b91a4d0b2))
* **config:** add config attributes to fee overrides ([#603](https://github.com/matter-labs/zksync-os-server/issues/603)) ([627af30](https://github.com/matter-labs/zksync-os-server/commit/627af301d4f42eeaf73dc4c51e274aef3c919293))
* Consistency checker nonce for failed creates ([#574](https://github.com/matter-labs/zksync-os-server/issues/574)) ([de8354f](https://github.com/matter-labs/zksync-os-server/commit/de8354f90b6c9d49cceaa4e568069b58a1980d77))
* construct pending block context in `eth_call`-like methods ([#758](https://github.com/matter-labs/zksync-os-server/issues/758)) ([4f87b6a](https://github.com/matter-labs/zksync-os-server/commit/4f87b6a040cd5368747565d4178b7708ecc50478))
* consume l1 txs processed in rebuild commands ([#568](https://github.com/matter-labs/zksync-os-server/issues/568)) ([abf5ee1](https://github.com/matter-labs/zksync-os-server/commit/abf5ee1260fcdce11e0aa0fa84aaae60a7508399))
* Decouple v1 batch verification transport ([#997](https://github.com/matter-labs/zksync-os-server/issues/997)) ([802c55c](https://github.com/matter-labs/zksync-os-server/commit/802c55cfe57d21d06975ee65447f2dc0a273439d))
* detect Git LFS pointers early and document LFS requirement ([#1084](https://github.com/matter-labs/zksync-os-server/issues/1084)) ([50caac6](https://github.com/matter-labs/zksync-os-server/commit/50caac64e61eec2321378830b16a60a56508906d))
* Disable warning on connection retries ([#545](https://github.com/matter-labs/zksync-os-server/issues/545)) ([5ae9c80](https://github.com/matter-labs/zksync-os-server/commit/5ae9c80d629c251cb08fb81f8ad259fcb55a08f2))
* do not do migration to set `execute_sl_block_number` for old batches ([#976](https://github.com/matter-labs/zksync-os-server/issues/976)) ([18e060b](https://github.com/matter-labs/zksync-os-server/commit/18e060b302339d919d74bce4b54eccf6a7ce722d))
* don't require genesis_chain_id for ENs ([#734](https://github.com/matter-labs/zksync-os-server/issues/734)) ([459b97d](https://github.com/matter-labs/zksync-os-server/commit/459b97de00842a5423bdfd893bde0ee955edfb73))
* EN and handle errors more gracefully ([#247](https://github.com/matter-labs/zksync-os-server/issues/247)) ([842ec60](https://github.com/matter-labs/zksync-os-server/commit/842ec6002fa4a6395791579ca829fce8da58f29f))
* **en:** handle missing blocks on main node ([#677](https://github.com/matter-labs/zksync-os-server/issues/677)) ([702efa7](https://github.com/matter-labs/zksync-os-server/commit/702efa7dd3a544cf4a52aa2d3a45625b84ef853e))
* **eth-watch:** don't save batches with divergent hashes ([#871](https://github.com/matter-labs/zksync-os-server/issues/871)) ([a8f40eb](https://github.com/matter-labs/zksync-os-server/commit/a8f40eb54f9df3af5e8cbc64abdda074af691d06))
* fix batch storage in revert case ([#1081](https://github.com/matter-labs/zksync-os-server/issues/1081)) ([a9903d5](https://github.com/matter-labs/zksync-os-server/commit/a9903d53de3c74858c1ba0a86f7114fd8f659dc3))
* fix calculation of da fields for validium v4 ([#636](https://github.com/matter-labs/zksync-os-server/issues/636)) ([64e9f2f](https://github.com/matter-labs/zksync-os-server/commit/64e9f2fafcb92a2d9b57eebe18e3a5ea5a4e668c))
* fix legacy batch processing in persist batch watcher ([#975](https://github.com/matter-labs/zksync-os-server/issues/975)) ([5e65f8a](https://github.com/matter-labs/zksync-os-server/commit/5e65f8a0b0f8924621705e4b7523a275e4d339b7))
* gas field calculation in tx receipt ([#361](https://github.com/matter-labs/zksync-os-server/issues/361)) ([39965e5](https://github.com/matter-labs/zksync-os-server/commit/39965e57ae4ad97ef3f99f1b30f912969d289b85))
* get rid of broadcast in mempool ([#910](https://github.com/matter-labs/zksync-os-server/issues/910)) ([c53b3b6](https://github.com/matter-labs/zksync-os-server/commit/c53b3b67cd44634ac673dd500f223d30f755327c))
* get rid of default debug logs ([#939](https://github.com/matter-labs/zksync-os-server/issues/939)) ([9e7b378](https://github.com/matter-labs/zksync-os-server/commit/9e7b3784688bbcb5c230d94554a521c3a0cc757c))
* hack to allow forcing null bridgehub in config ([#435](https://github.com/matter-labs/zksync-os-server/issues/435)) ([3bd14bb](https://github.com/matter-labs/zksync-os-server/commit/3bd14bb4ea678727d11754806e0e6af2e803e1d5))
* increase default value for `estimate_gas_pubdata_price_factor` ([#831](https://github.com/matter-labs/zksync-os-server/issues/831)) ([de6e964](https://github.com/matter-labs/zksync-os-server/commit/de6e96432953cad2a5e6a6a41fadec37d9ecdeb4))
* keep `StoredBatchInfo::last_block_timestamp` ([#977](https://github.com/matter-labs/zksync-os-server/issues/977)) ([746eafa](https://github.com/matter-labs/zksync-os-server/commit/746eafaff924eb5bbe1b0aef4e98fbb85c8d8fef))
* **l1_sender:** fix bug in `parallel_transactions` metric ([#996](https://github.com/matter-labs/zksync-os-server/issues/996)) ([b5a9c1a](https://github.com/matter-labs/zksync-os-server/commit/b5a9c1a94d4b6efde07f30cbd180a760252d4ec4))
* **l1-sender:** allow non-empty buffer for rescheduling ([#511](https://github.com/matter-labs/zksync-os-server/issues/511)) ([d5d9f05](https://github.com/matter-labs/zksync-os-server/commit/d5d9f05bdceb825a18dc7a21977439be6b5c3191))
* **l1-watcher:** handle L1 reverts during state recovery ([#692](https://github.com/matter-labs/zksync-os-server/issues/692)) ([f9e7723](https://github.com/matter-labs/zksync-os-server/commit/f9e77239a9a4ae957e09d7fea15c11a2dbd1b1d9))
* **l1-watcher:** pick the most recent upgrade cut ([#742](https://github.com/matter-labs/zksync-os-server/issues/742)) ([3313326](https://github.com/matter-labs/zksync-os-server/commit/33133267a09f016689fd9de322313aa06661ad65))
* **l1-watcher:** skip persisting legacy batches ([#860](https://github.com/matter-labs/zksync-os-server/issues/860)) ([182f56a](https://github.com/matter-labs/zksync-os-server/commit/182f56ad62a08cce5862d992c4fe798e89939c7e))
* **l1-watcher:** update batch finality ([#506](https://github.com/matter-labs/zksync-os-server/issues/506)) ([b66dd89](https://github.com/matter-labs/zksync-os-server/commit/b66dd8925b530bddb45dd4a4d194eaed9833f8b7))
* **l1:** various `alloy::Provider` improvements ([#272](https://github.com/matter-labs/zksync-os-server/issues/272)) ([97f49ed](https://github.com/matter-labs/zksync-os-server/commit/97f49ed6d86889d15af4f9f5789c46615b934b6f))
* local chain config file is required to start the node ([#771](https://github.com/matter-labs/zksync-os-server/issues/771)) ([231ebcb](https://github.com/matter-labs/zksync-os-server/commit/231ebcb246bd3d031c4d4e81c4bdfd42bbb41873))
* make get_transaction_receipt fallible ([#279](https://github.com/matter-labs/zksync-os-server/issues/279)) ([53b48a2](https://github.com/matter-labs/zksync-os-server/commit/53b48a2164c0a435880a14bd80b5ff55f95422ba))
* mempool pending fee refresh ([#955](https://github.com/matter-labs/zksync-os-server/issues/955)) ([432d8b8](https://github.com/matter-labs/zksync-os-server/commit/432d8b89576f65059a0dca4aa88b822944640a0e))
* missing unwrap_or in submit_proof ([#418](https://github.com/matter-labs/zksync-os-server/issues/418)) ([7a63491](https://github.com/matter-labs/zksync-os-server/commit/7a634919324471ace54e6b397d0bf05278f802bd))
* move BlacklistedSigner error to different enum ([#605](https://github.com/matter-labs/zksync-os-server/issues/605)) ([b7bf8e1](https://github.com/matter-labs/zksync-os-server/commit/b7bf8e16c57ed5c6b5cb2f143d3e6bff6f296674))
* multivm app path caching across tempdirs ([#948](https://github.com/matter-labs/zksync-os-server/issues/948)) ([d79c1f0](https://github.com/matter-labs/zksync-os-server/commit/d79c1f095d82ec4552f3fc10094c52ad37582a34))
* **multivm:** use correct directories and default version ([#490](https://github.com/matter-labs/zksync-os-server/issues/490)) ([8be9216](https://github.com/matter-labs/zksync-os-server/commit/8be9216169ed7aa4adca6d2a95c151a068a74540))
* Persisting some info about the failed batch ([#532](https://github.com/matter-labs/zksync-os-server/issues/532)) ([3abc4fe](https://github.com/matter-labs/zksync-os-server/commit/3abc4fec4727b2be8dc59aae6e9db8ca2ba3cbe3))
* **pipeline:** simplify task spawning ([#519](https://github.com/matter-labs/zksync-os-server/issues/519)) ([5a8151d](https://github.com/matter-labs/zksync-os-server/commit/5a8151d8fc550dc3a9919a97605f30962d1ed406))
* prevent "subtract with overflow" error on EN startup  ([#802](https://github.com/matter-labs/zksync-os-server/issues/802)) ([9d74b7c](https://github.com/matter-labs/zksync-os-server/commit/9d74b7c31f6f854d9e4b33511b5bb6873b018534))
* priority tree caching ([#399](https://github.com/matter-labs/zksync-os-server/issues/399)) ([607261b](https://github.com/matter-labs/zksync-os-server/commit/607261b78d4751022c728b3df45b110893b9902b))
* priority tree trim ([#397](https://github.com/matter-labs/zksync-os-server/issues/397)) ([4ab2219](https://github.com/matter-labs/zksync-os-server/commit/4ab22191a35601de3488e86c28177166d717376e))
* **priority-tree:** run initialization in background to avoid shutdown bug ([#1067](https://github.com/matter-labs/zksync-os-server/issues/1067)) ([b4b5ff4](https://github.com/matter-labs/zksync-os-server/commit/b4b5ff47d76dfcdeb723cb3579d05a9761b2c2c3))
* proving empty blocks - fix division by zero error in metrics tracking ([#584](https://github.com/matter-labs/zksync-os-server/issues/584)) ([be71db0](https://github.com/matter-labs/zksync-os-server/commit/be71db075e42555549198b41bdc449a4ba1db207))
* rebuild_from_block assert for EN ([#864](https://github.com/matter-labs/zksync-os-server/issues/864)) ([51e719f](https://github.com/matter-labs/zksync-os-server/commit/51e719f5cd19c93c2151f17fa9451bac2b2f4475))
* Reduced tracing level for debug functions ([#531](https://github.com/matter-labs/zksync-os-server/issues/531)) ([10347d0](https://github.com/matter-labs/zksync-os-server/commit/10347d01a8dbd835038d7f9457464427c383bd8c))
* refactor local-chains structure and update with anvil 1.5.1 ([#776](https://github.com/matter-labs/zksync-os-server/issues/776)) ([ef6d49b](https://github.com/matter-labs/zksync-os-server/commit/ef6d49b9b4c0b13f393d911b163c3983173dd402))
* register misc mempool metrics ([#599](https://github.com/matter-labs/zksync-os-server/issues/599)) ([3cdbf79](https://github.com/matter-labs/zksync-os-server/commit/3cdbf793aa9d4bfc9444f06481d8d13288957456))
* remove transaction r and s paddings ([#890](https://github.com/matter-labs/zksync-os-server/issues/890)) ([39a1ac0](https://github.com/matter-labs/zksync-os-server/commit/39a1ac0719ced957467b5984690271eb163915b8))
* Remove unnecessary configs for EN ([#986](https://github.com/matter-labs/zksync-os-server/issues/986)) ([b877dc0](https://github.com/matter-labs/zksync-os-server/commit/b877dc014580c5f3f8c172479585033548b32407))
* rename aggregated root to multichain root ([#924](https://github.com/matter-labs/zksync-os-server/issues/924)) ([5d3f12e](https://github.com/matter-labs/zksync-os-server/commit/5d3f12ec4143de74e6a40a1960ef203f44026be5))
* rename sandbox to ephemeral ([#778](https://github.com/matter-labs/zksync-os-server/issues/778)) ([13c3dfc](https://github.com/matter-labs/zksync-os-server/commit/13c3dfc6ec588f3e05485f251250535023af3567))
* Replace DashMap with RwLock and HashMap ([#722](https://github.com/matter-labs/zksync-os-server/issues/722)) ([72ad5e1](https://github.com/matter-labs/zksync-os-server/commit/72ad5e126fc890f31646312207c639354a9b45a8))
* report error on reverting `eth_call` ([#449](https://github.com/matter-labs/zksync-os-server/issues/449)) ([f9ecea8](https://github.com/matter-labs/zksync-os-server/commit/f9ecea85f5d8d044c724d9f9971834dfbce79a30))
* retry on pending commit tx in L1 watcher instead of panicking ([#952](https://github.com/matter-labs/zksync-os-server/issues/952)) ([df290af](https://github.com/matter-labs/zksync-os-server/commit/df290af4b1d021b11e4e3c14358c62250cfacae7))
* revm-consistency-checker legacy pre-eip155 transactions ([#740](https://github.com/matter-labs/zksync-os-server/issues/740)) ([4464a9e](https://github.com/matter-labs/zksync-os-server/commit/4464a9eed16cee34a18b5b6cc707e1a522372dd8))
* **rpc:** adjust latency histogram bucket range (1µs-32s) ([#990](https://github.com/matter-labs/zksync-os-server/issues/990)) ([c8c1da8](https://github.com/matter-labs/zksync-os-server/commit/c8c1da861b2cbf937dd2dfdcdee7e142b901728a))
* **rpc:** camelCase `batchNumber` is L2-&gt;L1 log proof ([#923](https://github.com/matter-labs/zksync-os-server/issues/923)) ([d1323b1](https://github.com/matter-labs/zksync-os-server/commit/d1323b10486b7f78f6aa94892f9e3e4d80725959))
* **rpc:** Fix `zks_getProof` ([#1032](https://github.com/matter-labs/zksync-os-server/issues/1032)) ([d7a7ad3](https://github.com/matter-labs/zksync-os-server/commit/d7a7ad31c27dfe62bf1ca5668d40bbde92d9ef4e))
* **rpc:** lower eth_getLogs default limits to match industry standard ([#992](https://github.com/matter-labs/zksync-os-server/issues/992)) ([8ba949b](https://github.com/matter-labs/zksync-os-server/commit/8ba949bd2dc5fee6e9bcd901b910aa7b8ecf7d5f))
* **rpc:** make `eth_estimateGas` work when sender has no balance ([#807](https://github.com/matter-labs/zksync-os-server/issues/807)) ([7f8adcf](https://github.com/matter-labs/zksync-os-server/commit/7f8adcf9be9a24200db89308e798842add90c9df))
* **rpc:** move executed block check earlier in `zks_getL2ToL1LogProof` ([#704](https://github.com/matter-labs/zksync-os-server/issues/704)) ([6eab908](https://github.com/matter-labs/zksync-os-server/commit/6eab90844d77c6420173247565e155eff10bf036))
* **rpc:** respect 0 gas price during gas estimation ([#865](https://github.com/matter-labs/zksync-os-server/issues/865)) ([67faea2](https://github.com/matter-labs/zksync-os-server/commit/67faea260f7a37fda50c5b67b38df08269495856))
* **rpc:** return hex-encoded subscription ids ([#877](https://github.com/matter-labs/zksync-os-server/issues/877)) ([d1dd247](https://github.com/matter-labs/zksync-os-server/commit/d1dd24767cfcb8bcfd75ef20b5d5a8c0a1054f0f))
* **rpc:** revert "make `eth_estimateGas` work when sender has no balance ([#807](https://github.com/matter-labs/zksync-os-server/issues/807))" ([#826](https://github.com/matter-labs/zksync-os-server/issues/826)) ([450272c](https://github.com/matter-labs/zksync-os-server/commit/450272c67fed34141fb7287b7c463070777af026))
* run RPC/status components later in the flow ([#817](https://github.com/matter-labs/zksync-os-server/issues/817)) ([dd23e67](https://github.com/matter-labs/zksync-os-server/commit/dd23e67a5742c12edac4879ecdd23425f179e326))
* Sealing empty blocks ([#653](https://github.com/matter-labs/zksync-os-server/issues/653)) ([d793d0f](https://github.com/matter-labs/zksync-os-server/commit/d793d0f6247eb5b240bbbe9cbcdd54f9d05fc9a4))
* **sequencer:** handle low-fee L2 transactions without stalling block production ([#927](https://github.com/matter-labs/zksync-os-server/issues/927)) ([066988e](https://github.com/matter-labs/zksync-os-server/commit/066988ecef20b15bbd025d7298e2251e8a524040))
* **sequencer:** save replay record first ([#556](https://github.com/matter-labs/zksync-os-server/issues/556)) ([e4e13b2](https://github.com/matter-labs/zksync-os-server/commit/e4e13b2167722912cb9162dab77357492d0fb270))
* set WORKDIR to /app ([#573](https://github.com/matter-labs/zksync-os-server/issues/573)) ([d0888a2](https://github.com/matter-labs/zksync-os-server/commit/d0888a27ee8f0aed6bbac0a2d49fd0b30afad9c9))
* skip already committed blocks before main batcher loop ([#286](https://github.com/matter-labs/zksync-os-server/issues/286)) ([3aa57ee](https://github.com/matter-labs/zksync-os-server/commit/3aa57ee7ff8ec615cd04cf95ba58ccd222f8e6fc))
* state recovery edge case ([#299](https://github.com/matter-labs/zksync-os-server/issues/299)) ([29b16a5](https://github.com/matter-labs/zksync-os-server/commit/29b16a577ec00e3589c723bbffca6161f6540b3b))
* state tracking for sequencer ([#715](https://github.com/matter-labs/zksync-os-server/issues/715)) ([9d573d7](https://github.com/matter-labs/zksync-os-server/commit/9d573d77014346510c397b688309e068ecbd0f35))
* **state:** do not overwrite full diffs ([#386](https://github.com/matter-labs/zksync-os-server/issues/386)) ([bb1e41d](https://github.com/matter-labs/zksync-os-server/commit/bb1e41dd8171374d5722ae0d7078f5496333f787))
* **storage:** read replay record atomically ([#521](https://github.com/matter-labs/zksync-os-server/issues/521)) ([36a3890](https://github.com/matter-labs/zksync-os-server/commit/36a3890935f6acb54c537aeb111853ff1f6b0c1b))
* support 0x-prefixed hex in all config fields ([#931](https://github.com/matter-labs/zksync-os-server/issues/931)) ([61dfc0a](https://github.com/matter-labs/zksync-os-server/commit/61dfc0af1320b3358bb65d5ae45bfdab35a18bb5))
* **tests:** decompress L1 state in build.rs instead of in-process cache ([#966](https://github.com/matter-labs/zksync-os-server/issues/966)) ([8974392](https://github.com/matter-labs/zksync-os-server/commit/897439270c66c5667fd95c884ab915687ace6e63))
* **tracer:** Fix call tracer behavior for 'empty' transactions ([#718](https://github.com/matter-labs/zksync-os-server/issues/718)) ([e560bb4](https://github.com/matter-labs/zksync-os-server/commit/e560bb447bd960d5cd619effe05d6e9b28d2477e))
* **tracer:** Fix handling of errors in subcalls ([#719](https://github.com/matter-labs/zksync-os-server/issues/719)) ([be979c2](https://github.com/matter-labs/zksync-os-server/commit/be979c29682829411042e9e175ff9a64a6627f57))
* **tracer:** map CREATE and CREATE2 correctly ([#1060](https://github.com/matter-labs/zksync-os-server/issues/1060)) ([a8bead2](https://github.com/matter-labs/zksync-os-server/commit/a8bead27551851c2ddb8a2dd96bf3bbbf41b3211))
* track timeout seal criteria in batcher ([4f863a3](https://github.com/matter-labs/zksync-os-server/commit/4f863a394a139a0d7fd4cecdd3a1ec46af25017b))
* **tree:** report backpressure ([#520](https://github.com/matter-labs/zksync-os-server/issues/520)) ([06aaf22](https://github.com/matter-labs/zksync-os-server/commit/06aaf221173c60aadefc67c848d2296ad10d711c))
* unwrap_or in pick_real_job  ([#416](https://github.com/matter-labs/zksync-os-server/issues/416)) ([8690551](https://github.com/matter-labs/zksync-os-server/commit/8690551c198ab9dccd9d1e9c6184f8661b7aa56f))
* Update revm to v0.0.2 ([#732](https://github.com/matter-labs/zksync-os-server/issues/732)) ([c427962](https://github.com/matter-labs/zksync-os-server/commit/c4279621b78bb924a1d0543f6e9d256a7524e2e7))
* Update time crate to 0.3.47 to address security vulnerability ([#870](https://github.com/matter-labs/zksync-os-server/issues/870)) ([3232213](https://github.com/matter-labs/zksync-os-server/commit/32322137a194442e0d2193cb68b4c5a4fabd27ef))
* Update ZKsync REVM deps ([#648](https://github.com/matter-labs/zksync-os-server/issues/648)) ([2165a03](https://github.com/matter-labs/zksync-os-server/commit/2165a03f2e7a4982edb515b8e6ce96517ea58148))
* upgrade issues ([#638](https://github.com/matter-labs/zksync-os-server/issues/638)) ([a3b983e](https://github.com/matter-labs/zksync-os-server/commit/a3b983ed249d347221ac77166e02a1de08b05126))
* upgrade issues in block context provider ([#666](https://github.com/matter-labs/zksync-os-server/issues/666)) ([d6c4d27](https://github.com/matter-labs/zksync-os-server/commit/d6c4d27a23709984a4327ff1c525c699f7f24676))
* upgrade issues second part ([#639](https://github.com/matter-labs/zksync-os-server/issues/639)) ([34dd4e4](https://github.com/matter-labs/zksync-os-server/commit/34dd4e444d2fd93a6930b1187cea0fedda33d969))
* upgrade lz4_flex to 0.12.1 to address RUSTSEC-2026-0041 ([#1024](https://github.com/matter-labs/zksync-os-server/issues/1024)) ([a9a2c7b](https://github.com/matter-labs/zksync-os-server/commit/a9a2c7b98f641e2e086596c3c6d8362aeb310f78))
* use correct previous_block_timestamp on server restart ([#384](https://github.com/matter-labs/zksync-os-server/issues/384)) ([8ee49fa](https://github.com/matter-labs/zksync-os-server/commit/8ee49fa96accabb9b5e488ae2e033eeeb2229961))
* use validium-rollup setting from L1 - not config; fix integration tests ([#255](https://github.com/matter-labs/zksync-os-server/issues/255)) ([da0f291](https://github.com/matter-labs/zksync-os-server/commit/da0f29132cdfba6f8d06a597f4d9cab45222ef57))
* Use warn for server disconnects ([#998](https://github.com/matter-labs/zksync-os-server/issues/998)) ([cae94ad](https://github.com/matter-labs/zksync-os-server/commit/cae94aded7ef1569dc7a19976ab225722c3881cb))
* Warn on batch verification threshold mismatch ([#984](https://github.com/matter-labs/zksync-os-server/issues/984)) ([f309ae0](https://github.com/matter-labs/zksync-os-server/commit/f309ae024abcf1b0007670445b4871c08a86c42c))


### Performance Improvements

* speed up priority tree init for EN ([#824](https://github.com/matter-labs/zksync-os-server/issues/824)) ([6e1901c](https://github.com/matter-labs/zksync-os-server/commit/6e1901c444f66b931d2bcc7df39a258adb3a4c65))


### Reverts

* feat: set gas per pubdata to `1` ([#431](https://github.com/matter-labs/zksync-os-server/issues/431)) ([ba8e72f](https://github.com/matter-labs/zksync-os-server/commit/ba8e72f1845ba2f734c94ad397e426737fd77c36))

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
