## Test Contracts

This directory contains a [Foundry](https://book.getfoundry.sh/) project with contracts that are used by integration tests.

### Build

```shell
$ forge build
```

Artifacts end up in `./out/<contract-name>.sol/<contract-name>.json` and are used by `zksync_os_integration_tests` via `alloy::sol!` macro.
