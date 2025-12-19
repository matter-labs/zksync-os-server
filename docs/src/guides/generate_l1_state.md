# Generate new genesis and L1 state

THIS GUIDE IS A DRAFT VERSION FOR THE NEW TOOLING AND WILL BE SIGNIFICANTLY UPDATED SOON.

## Prerequisites

Make sure you have the following tools installed:
* `anvil` v1.3.4 -> incredibly important to have exactly this `anvil` version, otherwise the state file won't load
* `forge` and `cast` v0.0.4 -> incredibly important to have exactly these versions, otherwise contracts won't deploy correctly
* `git`
* `cargo`
* `yarn`
* `python3`

Make sure port 8545 is free on your machine.

## Prepare environment

Make sure you have cloned the following repositories:
* `zksync-os-server`: `main` or your custom branch you're working on.
* `zkstack_cli` tool: `git clone https://github.com/matter-labs/zksync-era/ --branch zkstack-for-zksync-os --recurse-submodules`
* `era-contracts`: `git clone https://github.com/matter-labs/era-contracts/ --branch zkos-v0.30.2 --recurse-submodules`

Clone repository with the scripts:
```shell
git clone https://github.com/matter-labs/zksync-os-workflows.git
```

Take the latest execution version from:
https://github.com/matter-labs/zksync-os-server/blob/main/lib/types/src/protocol/execution_version.rs#L10
As of today (17.12.2025), it should be 5.

## Prepare python environment
Create python virtual environment and install dependencies:
```shell
cd zksync-os-workflows
python3 -m venv venv
source venv/bin/activate
pip install -r ./scripts_python/requirements.txt
```

If pyyaml installation fails due to cython, run:

```shell
pip install setuptools
pip install "cython<3"
pip install --no-build-isolation pyyaml==6.0
pip install -r ./scripts_python/requirements.txt
```

## Re-generate L1 state and genesis

Inside `zksync-os-workflows` repo, run:
```shell
WORKSPACE=${PWD} \
  REPO_DIR=../zksync-os-server \
  ERA_CONTRACTS_PATH=../era-contracts \
  ZKSYNC_ERA_PATH=../zksync-era \
  ZKSYNC_OS_EXECUTION_VERSION=5 \
    ./scripts_python/update_server.py
```

**Forge/cast requirements:**
Currently the script requires concrete version of forge/cast (0.0.4) - this is due to the current version of contracts (v29/v30) still using
old compiler, and new compiler resulting in different hashes.

If you run locally, and don't care about contract hash matches, you can comment it out in the script, and use any version of forge/cast.
