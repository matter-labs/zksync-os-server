# Updating local chains with new genesis and L1 state

This guide describes how to update [local chains](https://github.com/matter-labs/zksync-os-server/tree/main/local-chains)
setup for a particular protocol version with the new genesis, L1 state, and all required dependencies including
chain configs, wallets and contracts YAML files.

There are two ways to update the local setup:
1. [using GitHub Actions (recommended way for most users)](#updating-with-github-actions)
2. [running the update scripts locally (on self-hosted setups, next protocol version development, or when experimenting with custom contracts)](#updating-locally)

```admonish info
**Use automated update through GitHub Actions if:**
* you're iterating on the next protocol version and need to update the server setup with the new version of contracts
* you need to regenerate local chains setup due to the tooling changes, e.g. anvil updates, new scripts, etc.

**Run scripts locally if:**
* you are experimenting with custom contracts locally and want to update your local setup
* you are self-hosted user wanting to update your server setup with the custom version of contracts
```

Follow the appropriate section below depending on your use case.

---

## Updating with GitHub Actions

For the general guidelines about the GitHub Actions workflow to perform the local chains update, please refer to the
**[General GitHub Actions guidelines](https://matter-labs.github.io/zksync-os-scripts/latest/github-actions.html)**.
Especially important are the sections about:
* [how to run the workflows](https://matter-labs.github.io/zksync-os-scripts/latest/github-actions.html#how-to-run-the-workflows)
* [outputs and artifacts](https://matter-labs.github.io/zksync-os-scripts/latest/github-actions.html#outputs-and-artifacts)

Then, follow the instructions in the **[Server Update GitHub workflow guide](https://matter-labs.github.io/zksync-os-scripts/latest/scripts/update_server.html#github-actions)**
to perform the update of the local chains setup through GitHub Actions.

As the result of the workflow execution, you will get the updated local chains setup available as:
* git patch published as a workflow artifact that you can download and apply locally using `git apply` command
* new commit on your custom development branch
* new pull request with the update

```admonish tip
If you are planning to merge your local chains update into the `main` branch,
please prefer automated update through GitHub Actions
as it is more reliable, tested in CI and less error-prone than the manual update.
```

---

## Updating locally

To perform a local update of the local chains setup, you can run the update scripts locally on your machine.

### Compatibility between protocol versions

Before performing the update, please make sure to check the
**[protocol compatibility](https://matter-labs.github.io/zksync-os-scripts/latest/protocol-compatibility.html)**
tables to make sure that you are using compatible versions of the protocol (contracts), server, and tooling.

```admonish warning
**Using incompatible versions of the protocol, server, or tooling may lead to unexpected errors during the update process.**
Carefully check the compatibility tables and make sure to use compatible versions of all components before proceeding with the update.

These tables are **the source of truth** for the compatibility of different versions of the protocol, server, and tooling.
If you notice that the tables are outdated, please report it to the team, and/or contribute to updating the documentation with the correct information.
```

### Prerequisites and environment

The scripts are located in the [matter-labs/zksync-os-scripts](https://github.com/matter-labs/zksync-os-scripts) repository.

Go through the [Prerequisites](https://matter-labs.github.io/zksync-os-scripts/latest/prerequisites.html) guide to set up your environment.

```admonish info
There is a [Quick install](https://matter-labs.github.io/zksync-os-scripts/latest/prerequisites.html#quick-install-recommended) section
that goes through all required dependencies setup for Unix-like system, and it is recommended to use it as a reference.
```

### Re-generate local state

It is recommended to go through the full **[Server Update guide](https://matter-labs.github.io/zksync-os-scripts/latest/scripts/update_server.html)** first.

```admonish tip
You can directly jump to the **[Local Use](https://matter-labs.github.io/zksync-os-scripts/latest/scripts/update_server.html#local-use)**
section of the guide if you already know what the script does.
```

As the result, you will get the updated local chains setup available in the `local-chains/` directory of your server repository.

---
