# RPC

* All standard `eth_` methods are supported (except those specific to EIP-2930, EIP-4844 and EIP-7702). Block tags have
  a special meaning:
    * `earliest` - not supported yet (will return genesis or first uncompressed block)
    * `pending` - the latest produced block
    * `latest` - same as `pending` (consider taking consensus into account here)
    * `safe` - the latest block that has been committed to L1
    * `finalized` - not supported yet (will return the latest block that has been executed on L1)
* `zks_` namespace is kept to the minimum right now to avoid legacy from Era. Only following methods are supported:
    * `zks_getBridgehubContract`
* `ots_` namespace is used for Otterscan integration (meant for local development only)
