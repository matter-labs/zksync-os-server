# ZKSync OS Fee model

ZKSync OS fee model is designed in a way to ensure that it describes well L2-specific costs (pubdata costs, ZK proving costs),
while trying to keep it both simple and similar to Ethereum model. Internally VM keeps track of three resources: 
gas (similar to EVM), native (resource that reflects proving costs), pubdata (number of bytes to be posted on L1).

There are three parameters in block context that define fees:
- `native_price` -- price for one unit of native
- `eip1559_basefee` -- price for one unit of gas
- `pubdata_price` -- price for one byte of pubdata
All three are specified in base token units, e.g. wei for ETH-based chains.

VM uses the following reasoning when calculating `gas_used`. Firstly, it calculates EVM gas used and effective gas price.
`evm_gas_used * effective_gas_price` gives it a number of base token units to be charged in EVM case. Secondly, it calculates native and pubdata costs:
`native_price * native_used + pubdata_price * pubdata_used`. Finally, it takes the maximum of two values and uses it as total fee and returns 
`gas_used` such that `gas_used * effective_gas_price` equals total fee.

## Fee configuration and calculation

### Native price

Since native reflects ZK proving costs, it should be calculated based on 2 things:
- prover machine cost
- prover performance (how many native units it processes per second)

Native price is configured with `FeeConfig::native_price_usd` (`fee_native_price_usd`).
Node converts config parameter to base token units and uses the result as `native_price`.

### Base fee

The idea behind calculating base fee is to choose it in a way such that in most cases 
- the resulting gas used should be equal to evm gas used
- total fee should not be much higher than what operator spends in reality

Luckily, for the most opcodes the ratio between evm gas cost and native cost does not differ a lot.
However, for some precompiles, e.g. modexp, the ratio native:gas is higher than for regular opcodes.

So, base fee is calculated as `eip1559_basefee = native_price * native_per_gas`, 
where `native_per_gas` can be configured via `FeeConfig::native_per_gas` (`fee_native_per_gas`).
The default value is chosen such that the two properties above hold in most cases, 
that is if a transaction doesn't use many precompiles that are expensive in terms of native and does not require publishing a lot of pubdata.

### Pubdata price

Pubdata price depends on what DA chain uses. If chain is a validium then price is set to 0.
For rollups that settle to L1:
- if blobs are used, then L1 blob price is used for calculation
- if calldata is used, then L1 gas price is used for calculation
If rollup settles to Gateway, then gateway pubdate price is used.

Pricing for blobs case is special because calculation of blob commitments is proven so it results in additional proving costs,
thus pubdata price also depends on native price.
Also, if chain settles frequently and posts blob that is not full then operator still needs to pay for the full blob.
Node calculates a statistic for a fill ratio of submitted blobs and uses it to adjust pubdata price accordingly.

Blob price is not stable on Ethereum testnets, and can grow a lot sometimes. 
At some point it can be the case that pubdata price is high enough so that gas costs for small transactions are higher than block gas limit.
To circumvent this issue, node has a configuration parameter `FeeConfig::pubdata_price_cap` (`fee_pubdata_price_cap`).
If it's set, then pubdata price is capped by the value, allowing the node to operate normally even if blob price is very high.
For ETH-based testnet chains we recommend to set `10_000_000_000_000`. If base token price is different from ETH, then the value should be adjusted accordingly.

### Config overrides

Config allows to set constant overrides for base_fee, native_price, and pubdata_price. 
Config variable are `fee_base_fee_override`, `fee_native_price_override`, and `fee_pubdata_price_override` respectively.
If set, node uses the override values instead of calculating the parameters as described above.
It can be used if operator prefers to not have a dynamic fee model, or for testing purposes.

### EIP-1559

EIP-1559 rules for base fee calculation does not make much sense for ZKSync OS, 
because the operator costs doesn't depend on block gas usage and sequencer supports big max TPS that should be enough for most situations.
However, a small part of EIP-1559 is still applied:
- `native_price` and `base_fee` can change between subsequent blocks max by 12.5%
- `pubdata_price` can increase between subsequent blocks max by 50%
It's needed for smooth transitions in case of sudden changes in parameters that affect fee calculation,
otherwise it may lead to poor UX, e.g. txs can stuck in mempool, fail with out of gas, etc.
