# Base token price updater

Base token price updater is a service that periodically fetches USD prices of tokens that are required to properly
calculate fee parameters. There are 3 options for token price source: CoinGecko, CoinMarketCap (3rd party APIs),
or Forced  which instantiates a client that returns prices that are configured, by default price randomly fluctuate a 
little to simulate real world scenario (fluctuation can be disabled).

## Price source configuration

Source is configured in `ExternalPriceApiClientConfig`. For example, CoinGecko can be configured as follows:

```yaml
external_price_api_client:
  source: "Coingecko"
  coingecko_api_key: "<key>"
```

For forced config it's essential to provide prices for all required tokens:
- chain base token
- ETH

So for the chain that uses USDC as base token forced configuration can look like this:
```yaml
external_price_api_client:
  source: "Forced"
  forced_prices:
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": 1.0    # USDC
    "0x0000000000000000000000000000000000000001": 3000.0 # ETH
```

In simple case, when chain base token is ETH, only ETH price is required:
```yaml
external_price_api_client:
  source: "Forced"
  forced_prices:
    "0x0000000000000000000000000000000000000001": 3000.0 # ETH
```

## Token multiplier setter

For chains with base token different from ETH it's recommended to configure a token multiplier setter signer,
then the component will also periodically update "ETH:token" price ratio on L1. Component and node will
still work without it but there will be a warning in logs and ratio on L1 won't change meaning that price
for L1->L2 txs can eventually get outdated.

You can use a local private key, a GCP KMS key, or an Azure Key Vault key via the `token_multiplier_setter_sk` field:

```yaml
# Option 1: Local private key (plain hex string)
base_token_price_updater:
  token_multiplier_setter_sk: "<private_key_in_hex>"

# Option 2: GCP KMS key (structured object)
base_token_price_updater:
  token_multiplier_setter_sk:
    type: gcp_kms
    resource: "projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}"

# Option 3: Azure Key Vault / Managed HSM key (structured object; the key version is required)
base_token_price_updater:
  token_multiplier_setter_sk:
    type: azure_kms
    key_id: "https://{vault}.vault.azure.net/keys/{name}/{version}"
```

## Mainnet recommendation

For mainnet it's recommended to use one of 3rd party sources so that the fees are accurate 
and correspond to an up-to-date token price; and provide an API key to avoid getting rate-limited.

**Also, it's highly recommended to set `fallback_prices` configuration.**
It sets predefined fallback prices for tokens in case external API fetching fails on startup.
If it's missing and the price fetching fails on startup, then **block sequencing will be blocked**.

Configuration is similar to `forced_prices`. And should contain prices for all required tokens.
```yaml
base_token_price_updater:
  fallback_prices:
    "0x0000000000000000000000000000000000000001": 3000.0 # ETH
```

## Testnet recommendation

For testnets it's usually acceptable to use Forced source with reasonable prices configured.
In case you want fees to behave as on mainnet, you can still use 3rd party source and set config:
- `base_token_addr_override` - mainnet token address that source can provide price for; in case base token is ETH it can be omitted.
- `base_token_decimals_override` - token decimals (since token is on mainnet but node connects to testnet it cannot get the decimals from L1);
    in case base token is ETH or ZK it can be omitted.

Example configuration for a chain whose base token is USDC:
```yaml
base_token_price_updater:
  base_token_addr_override: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" # USDC
  base_token_decimals_override: 6 # USDC decimals
```
