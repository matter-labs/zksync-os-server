# Chaos workload contracts

A [Foundry](https://book.getfoundry.sh/) project with the contracts the chaos
rig's load workloads drive (`chaos load`, see `tools/chaos/src/load/`). One
contract per workload theme; each file's header comment names its driver.

Built automatically by `tools/chaos/build.rs` when `forge` is on the path
(artifacts land in `./out/`, gitignored). The rig binary reads deployment
bytecode from the artifacts at runtime, so building the workspace does not
require forge — running contract workloads does.

Manual build:

```shell
$ forge build
```
