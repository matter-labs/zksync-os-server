# Besu Network

ZKsync OS can settle to a [Besu](https://besu.hyperledger.org/) L1 (for example a private QBFT
network) instead of Anvil. Two conditions must hold for the node to run against it.

## Archive node

Besu must run as an **archive node** (`--data-storage-format=FOREST`). On startup the server
discovers each L1 contract's deployment block by binary-searching historical `eth_getCode` calls,
which a pruned node (the default `BONSAI` format) cannot answer.

## Forks

Enable all forks **up to and including Cancun**. On a Cancun chain the server posts pubdata as
**EIP-4844 blobs**.

**Fusaka (Osaka) is optional.** If you also enable it, the server switches to the newer **EIP-7594**
blob transaction format. The node detects the chain's active fork through `eth_config` at startup
and picks the matching blob format automatically — no configuration needed.

### Genesis up to Cancun

A complete Besu QBFT genesis with forks enabled through Cancun, ready to copy into a
`qbftConfigFile.json`:

```json
{
    "genesis": {
        "config": {
            "chainId": 31337,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "constantinopleFixBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "shanghaiTime": 0,
            "cancunTime": 0,
            "zeroBaseFee": true,
            "qbft": {
                "blockperiodseconds": 2,
                "epochlength": 30000,
                "requesttimeoutseconds": 4
            }
        },
        "nonce": "0x0",
        "timestamp": "0x58ee40ba",
        "gasLimit": "0x1c9c380",
        "baseFeePerGas": "0x0",
        "blobGasUsed": "0x0",
        "excessBlobGas": "0x0",
        "parentBeaconBlockRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "difficulty": "0x1",
        "mixHash": "0x63746963616c2062797a616e74696e65206661756c7420746f6c6572616e6365",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": {
            "fe3b557e8fb62b89f4916b721be55ceb828dbd73": {
                "privateKey": "8f2a55949038a9610f50fb23b5883af3b4ecb3c3bb792cbcefbd1542c692be63",
                "comment": "Testing only. Besu ignores this privateKey field in alloc.",
                "balance": "0xad78ebc5ac6200000"
            },
            "627306090abaB3A6e1400e9345bC60c78a8BEf57": {
                "privateKey": "c87509a1c067bbde78beb793e6fa76530b6382a4c0241e5e4a9ec0a0f44dc0d3",
                "comment": "Testing only. Besu ignores this privateKey field in alloc.",
                "balance": "90000000000000000000000"
            },
            "f17f52151EbEF6C7334FAD080c5704D77216b732": {
                "privateKey": "ae6ae8e5ccbfb04590405997ee2d52d2b330726137b875053c36d94e974d162f",
                "comment": "Testing only. Besu ignores this privateKey field in alloc.",
                "balance": "90000000000000000000000"
            },
            "a61464658afeaf65cccaafd3a512b69a83b77618": {
                "privateKey": "ac1e735be8536c6534bb4f17f06f6afc73b2b5ba84ac2cfb12f7461b20c0bbe3",
                "comment": "zksync-os-scripts rich account. Besu ignores this privateKey field in alloc.",
                "balance": "90000000000000000000000"
            },
            "36615cf349d7f6344891b1e7ca7c72883f5dc049": {
                "privateKey": "7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
                "comment": "zksync-os-scripts rich account. Besu ignores this privateKey field in alloc.",
                "balance": "90000000000000000000000"
            },
            "4e59b44847b379578588920cA78FbF26c0B4956C": {
                "comment": "Foundry deterministic deployment proxy (CREATE2 factory)",
                "balance": "0x0",
                "code": "0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3"
            }
        }
    },
    "blockchain": {
        "nodes": {
            "generate": true,
            "count": 4
        }
    }
}
```

The `alloc` predeploys the CREATE2 factory at `0x4e59b44847b379578588920cA78FbF26c0B4956C`, which the
deployer relies on. The `privateKey` fields are ignored by Besu and are only there for test tooling.

### Enabling Fusaka

To move the chain to Fusaka, add Prague and Osaka activation plus a blob schedule to `config`:

```json
{
  "config": {
    "...": "... forks up to cancunTime as above ...",
    "cancunTime": 0,
    "pragueTime": 0,
    "osakaTime": 0,
    "blobSchedule": {
      "osaka": {
        "target": 6,
        "max": 9,
        "baseFeeUpdateFraction": 5007716
      }
    }
  }
}
```

With Osaka active, the server sends EIP-7594 blob transactions instead of EIP-4844.
