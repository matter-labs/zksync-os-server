RPC_URL=http://localhost:8545
PRIVKEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
find . -type f -name 'wallets.yaml' | while read -r file; do
  echo "Processing $file …"

  # extract all addresses (strips leading spaces and the "address:" prefix)
  grep -E '^[[:space:]]*address:' "$file" \
    | sed -E 's/^[[:space:]]*address:[[:space:]]*//' \
    | while read -r addr; do

      if [[ $addr =~ ^0x[0-9a-fA-F]{40}$ ]]; then
        echo "→ Sending 10 ETH to $addr"
        cast send "$addr" \
          --value 10ether \
          --private-key "$PRIVKEY" \
          --rpc-url "$RPC_URL"
      else
        echo "⚠️  Skipping invalid address: '$addr'" >&2
      fi

    done
done
