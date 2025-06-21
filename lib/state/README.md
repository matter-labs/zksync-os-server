## State Storage implementation for zksync-os sequencer

`State` is the full set of user and smart contract data required for VM execution. It consists of:
* `Storage` - key value map containing all storage slots ever written by any smart contract (`Bytes32 => Bytes32`)
* `Preimages` - key value map containing hash values with their respective preimages (`Bytes32 => Vec<u8>`). zksync-os uses it for:
  * Bytecodes of all deployed contracts 
  * Account Properties of all addresses/accounts ever interacted with

During VM execution, server provides the state to the VM via the following traits: 
```rust
pub trait ReadStorage: 'static {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32>;
}
```

```rust
pub trait PreimageSource: 'static {
    fn get_preimage(&mut self, hash: Bytes32) -> Option<Vec<u8>>;
}
```

Note that `Storage` is always accessed in a context of certain block. 
That is, if a key was written to in multiple blocks, its expected value will differ depending on the block at which context it is requested.
To facilitate this, `StorageView` abstraction is introduced.

This doesn't apply to preimages - currently newer preimage can be returned for an older block.
This is confirmed to be acceptable from a protocol standpoint - but we may still reconsider for consistency


Note that there is other data that node needs to store like:
* Block recipes
* Tx recipes
* Account data per block
* etc

This data is required for RPC, but not needed for VM execution. 