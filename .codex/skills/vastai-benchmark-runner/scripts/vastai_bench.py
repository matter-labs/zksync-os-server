#!/usr/bin/env python3
"""Small helpers for Vast.ai benchmark workflows."""

from __future__ import annotations

import argparse
import json
import shlex
import subprocess
import sys
from typing import Any


CPU_KEYS = (
    "cpu_cores_effective",
    "cpu_cores",
    "cpu_threads",
)


def number(value: Any, default: float = 0.0) -> float:
    if value is None:
        return default
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def text(value: Any) -> str:
    return "" if value is None else str(value)


def load_offers(path: str | None, query: str | None) -> list[dict[str, Any]]:
    if path:
        raw = open(path, "r", encoding="utf-8").read()
    else:
        if not query:
            raise SystemExit("provide --query or --offers-json")
        cmd = ["vastai", "search", "offers", query, "--raw"]
        raw = subprocess.check_output(cmd, text=True)
    data = json.loads(raw)
    if isinstance(data, dict):
        for key in ("offers", "results", "machines"):
            if isinstance(data.get(key), list):
                return data[key]
    if isinstance(data, list):
        return data
    raise SystemExit("could not find offer list in raw Vast.ai output")


def get_cpu_cores(offer: dict[str, Any]) -> float:
    return max(number(offer.get(key)) for key in CPU_KEYS)


def get_price(offer: dict[str, Any]) -> float:
    for key in ("dph_total", "dph_base", "min_bid", "price", "rentable_price"):
        value = number(offer.get(key), -1)
        if value >= 0:
            return value
    return 999999.0


def is_whole_machine(offer: dict[str, Any]) -> bool:
    num_gpus = number(offer.get("num_gpus"), -1)
    total_gpus = number(offer.get("total_gpus"), -2)
    if num_gpus >= 0 and total_gpus >= 0 and num_gpus != total_gpus:
        return False
    rented = number(offer.get("rented"), 0)
    if rented > 0:
        return False
    return True


def score_offer(offer: dict[str, Any]) -> float:
    cores = get_cpu_cores(offer)
    ram_gb = number(offer.get("cpu_ram"), number(offer.get("ram"), 0)) / 1024.0
    reliability = number(offer.get("reliability2"), number(offer.get("reliability"), 0.9))
    disk_bw = number(offer.get("disk_bw"))
    inet = number(offer.get("inet_down")) + number(offer.get("inet_up"))
    price = max(get_price(offer), 0.05)
    whole_bonus = 1.20 if is_whole_machine(offer) else 1.0
    return whole_bonus * (cores * 8 + ram_gb * 0.7 + disk_bw * 0.015 + inet * 0.01) * reliability / price


def offer_id(offer: dict[str, Any]) -> Any:
    for key in ("id", "ask_contract_id", "machine_id", "bundle_id"):
        if offer.get(key) is not None:
            return offer[key]
    return "unknown"


def cmd_search(args: argparse.Namespace) -> None:
    offers = load_offers(args.offers_json, args.query)
    ranked = sorted(offers, key=score_offer, reverse=True)[: args.top]
    for idx, offer in enumerate(ranked, 1):
        price = get_price(offer)
        print(
            f"{idx:>2}. offer={offer_id(offer)} score={score_offer(offer):.1f} "
            f"${price:.3f}/h whole={is_whole_machine(offer)} "
            f"cpu={text(offer.get('cpu_name') or offer.get('cpu_model'))} "
            f"cores={get_cpu_cores(offer):.0f} "
            f"ram_gb={number(offer.get('cpu_ram'), number(offer.get('ram'), 0)) / 1024.0:.0f} "
            f"gpus={text(offer.get('num_gpus'))}/{text(offer.get('total_gpus'))} "
            f"gpu={text(offer.get('gpu_name'))} "
            f"reliability={number(offer.get('reliability2'), number(offer.get('reliability'), 0)):.3f}"
        )


def create_command(args: argparse.Namespace) -> list[str]:
    cmd = [
        "vastai",
        "create",
        "instance",
        str(args.offer_id),
        "--image",
        args.image,
        "--disk",
        str(args.disk),
        "--ssh",
    ]
    if args.env:
        for item in args.env:
            cmd.extend(["--env", item])
    if args.onstart:
        cmd.extend(["--onstart-cmd", args.onstart])
    return cmd


def cmd_create_command(args: argparse.Namespace) -> None:
    print(" ".join(shlex.quote(part) for part in create_command(args)))


def cmd_create(args: argparse.Namespace) -> None:
    cmd = create_command(args)
    print(" ".join(shlex.quote(part) for part in cmd))
    if args.execute:
        subprocess.run(cmd, check=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(required=True)

    search = sub.add_parser("search", help="rank Vast.ai offers")
    search.add_argument("--query")
    search.add_argument("--offers-json")
    search.add_argument("--top", type=int, default=8)
    search.set_defaults(func=cmd_search)

    for name, execute in (("create-command", False), ("create", True)):
        create = sub.add_parser(name, help="generate or run a create command")
        create.add_argument("--offer-id", required=True)
        create.add_argument("--image", default="nvidia/cuda:12.4.1-devel-ubuntu22.04")
        create.add_argument("--disk", type=int, default=200)
        create.add_argument("--env", action="append")
        create.add_argument("--onstart")
        create.add_argument("--execute", action="store_true", default=False)
        create.set_defaults(func=cmd_create if execute else cmd_create_command)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
