## Prerequisites

This project requires:

* **Git LFS** (this repo uses Git Large File Storage for chain state files)
* The **Foundry nightly toolchain**
* The **Rust toolchain**

### Install Git LFS

This repository uses [Git LFS](https://git-lfs.com) to store large chain state files (`*.json.gz`, `*.tar.gz`).
You **must** install and initialize Git LFS before cloning, otherwise you'll get small pointer files instead of the actual data.

```bash
# macOS
brew install git-lfs

# Ubuntu/Debian
sudo apt-get install -y git-lfs
```

Then run the one-time global setup:

```bash
git lfs install
```

After this, `git clone` will automatically fetch LFS files. If you already cloned without LFS, run:

```bash
git lfs pull
```

### Install Foundry (v1.5.1)

Install [Foundry](https://getfoundry.sh/) v1.5.1:

```bash
# Download the Foundry installer
curl -L https://foundry.paradigm.xyz | bash

# Install forge, cast, anvil, chisel
# Ensure you are using the 1.5.1 stable release
foundryup -i 1.5.1
```

Verify your installation:

```bash
anvil --version
```

The output should include a `anvil Version: 1.5.1`.

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
