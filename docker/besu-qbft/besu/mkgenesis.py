#!/usr/bin/env python3
"""Derive the Besu genesis file from qbftConfigFile.json + the committed
validator extraData.

Runs as the `genesis-init` compose service on every `docker compose up`, so
edits to qbftConfigFile.json (forks, alloc, chainId, gas limit, QBFT params)
take effect with a plain `docker compose down && docker compose up` — no
manual regeneration. extraData encodes only the validator set; it must be
regenerated (see README) solely when the validator keys/count change.
"""

import json
import sys


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit(f"usage: {sys.argv[0]} <qbftConfigFile.json> <extraData.txt> <out-genesis.json>")

    config_path, extra_path, out_path = sys.argv[1:4]

    with open(config_path) as f:
        genesis = json.load(f)["genesis"]
    with open(extra_path) as f:
        genesis["extraData"] = f.read().strip()

    with open(out_path, "w") as f:
        json.dump(genesis, f, indent=2)
        f.write("\n")

    cfg = genesis["config"]
    print(
        f"genesis written to {out_path}: "
        f"chainId={cfg.get('chainId')} zeroBaseFee={cfg.get('zeroBaseFee')} "
        f"osakaTime={cfg.get('osakaTime')} validatorsExtraData={genesis['extraData'][:26]}..."
    )


if __name__ == "__main__":
    main()
