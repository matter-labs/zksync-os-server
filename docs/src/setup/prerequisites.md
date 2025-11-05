## Prerequisites

This project requires:

* The **Foundry nightly toolchain**
* The **Rust toolchain**

### Install Foundry (v1.3.4)

Install [Foundry](https://getfoundry.sh/) v1.3.4 (newer **stable** versions are likely to work too but not guaranteed):

```bash
# Download the Foundry installer
curl -L https://foundry.paradigm.xyz | bash

# Install forge, cast, anvil, chisel
# Ensure you are using the 1.3.4 stable release
foundryup -i 1.3.4
```

Verify your installation:

```bash
anvil --version
```

The output should include a `anvil Version: 1.3.4-v1.3.4`.

### Install Rust

Install [Rust](https://www.rust-lang.org/tools/install) using `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

After installation, ensure Rust is available:

```bash
rustc --version
```

### Linux packages

```bash
# essentials
sudo apt-get install -y build-essential pkg-config cmake clang lldb lld libssl-dev apt-transport-https ca-certificates curl software-properties-common git    
```
