# CLI tool for the server

This tool allows you to see the details of the server database, and manipuate some of them in case of emergency.


## Features:

**Info**
Shows basic information from the database:

```shell
cargo run -- --db-path ../../db/node1/ info
```

**Block**

Shows detailed information about a given block:

```shell
cargo run -- --db-path ../../db/node1/ block 6
```

**Tx**

Shows detailed information about the transaction:

```shell
cargo run -- --db-path ../../db/node1/ tx 8aaa3134a4e3ec915fb5f00a1400a8212a3a0a331cd8cf36f672197b8f8fcc11
```

**All rows**
Prints all the rows from the sub database (please select correct dir)

```shell
cargo run -- --db-path ../../db/node1/preimages all-rows
```


## TODO:

[] - allow selecting tx based off the block + index
[] - show basic calldata about transactions
[] - show info about preimages

[] - support showing info about legacy TX too.

[] - add option to remove uncommitted block
[] - option to remove the proof
[] - what about the tree?