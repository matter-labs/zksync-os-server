# FAQ

**Failed to read L1 state: contract call to `getAllZKChainChainIDs` returned no data ("0x"); the called address might
not be a contract**

Something went wrong with L1 - check that you're really running the anvil with the proper state on the right port.

**Failed to deserialize context**

If you hit this error when starting, check if you don't have some 'old' rocksDB data in db/node1 directory.
