#!/usr/bin/env python3
import argparse
import json
import re
from pathlib import Path


PROBE_EVENT = "pubdata_probe_tx"
MEASUREMENT_MARKER = "Executed transaction pubdata measurement"
BREAKDOWN_MARKER = "Block pubdata component measurement"
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def load_probe_events(output_dir: Path):
    events = {}
    with (output_dir / "events.jsonl").open(errors="ignore") as f:
        for line in f:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("event") != PROBE_EVENT:
                continue
            tx_hash = str(event["source_tx_hash"])
            events[tx_hash] = event
    return events


def load_pubdata_measurements(log_dir: Path):
    measurements = {}
    breakdowns = {}
    for path in log_dir.glob("chain_656*.log"):
        chain_id = 6565 if "chain_6565" in path.name else 6566
        with path.open(errors="ignore") as f:
            for line in f:
                line = ANSI_RE.sub("", line)
                if MEASUREMENT_MARKER in line:
                    tx_hash = find_field(line, "tx_hash")
                    if tx_hash is None:
                        continue
                    measurements[tx_hash] = {
                        "log_chain_id": chain_id,
                        "block_number": find_int_field(line, "block_number"),
                        "tx_type": find_field(line, "tx_type"),
                        "system_tx_type": find_field(line, "system_tx_type"),
                        "gas_used": find_int_field(line, "gas_used"),
                        "pubdata_used": find_int_field(line, "pubdata_used"),
                    }
                elif BREAKDOWN_MARKER in line:
                    block_number = find_int_field(line, "block_number")
                    if block_number is None:
                        continue
                    breakdowns[(chain_id, block_number)] = {
                        "header_bytes": find_int_field(line, "header_bytes"),
                        "storage_bytes": find_int_field(line, "storage_bytes"),
                        "storage_diffs": find_int_field(line, "storage_diffs"),
                        "account_diff_bytes": find_int_field(line, "account_diff_bytes"),
                        "account_diffs": find_int_field(line, "account_diffs"),
                        "value_diff_bytes": find_int_field(line, "value_diff_bytes"),
                        "value_diffs": find_int_field(line, "value_diffs"),
                        "logs_bytes": find_int_field(line, "logs_bytes"),
                        "logs_count": find_int_field(line, "logs_count"),
                        "message_payload_bytes": find_int_field(line, "message_payload_bytes"),
                        "messages_count": find_int_field(line, "messages_count"),
                        "total_pubdata_bytes": find_int_field(line, "total_pubdata_bytes"),
                    }
    return measurements, breakdowns


def find_field(line: str, name: str):
    match = re.search(rf"{name}=([^ ]+)", line)
    return match.group(1) if match else None


def find_int_field(line: str, name: str):
    value = find_field(line, name)
    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def main():
    parser = argparse.ArgumentParser(
        description="Join interop-load --pubdata-probe events with source-chain pubdata logs."
    )
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--logs-dir", required=True, type=Path)
    args = parser.parse_args()

    events = load_probe_events(args.output_dir)
    measurements, breakdowns = load_pubdata_measurements(args.logs_dir)

    print(
        "label,shape,source_chain_id,source_block,source_gas_used,tx_hash,log_chain_id,log_block_number,tx_type,system_tx_type,gas_used,pubdata_used,header_bytes,storage_bytes,storage_diffs,account_diff_bytes,account_diffs,value_diff_bytes,value_diffs,logs_bytes,logs_count,message_payload_bytes,messages_count,total_pubdata_bytes"
    )
    for tx_hash, event in events.items():
        measurement = measurements.get(tx_hash, {})
        breakdown = breakdowns.get(
            (measurement.get("log_chain_id"), measurement.get("block_number")), {}
        )
        print(
            ",".join(
                str(value if value is not None else "")
                for value in [
                    event.get("label"),
                    event.get("shape"),
                    event.get("source_chain_id"),
                    event.get("source_block"),
                    event.get("source_gas_used"),
                    tx_hash,
                    measurement.get("log_chain_id"),
                    measurement.get("block_number"),
                    measurement.get("tx_type"),
                    measurement.get("system_tx_type"),
                    measurement.get("gas_used"),
                    measurement.get("pubdata_used"),
                    breakdown.get("header_bytes"),
                    breakdown.get("storage_bytes"),
                    breakdown.get("storage_diffs"),
                    breakdown.get("account_diff_bytes"),
                    breakdown.get("account_diffs"),
                    breakdown.get("value_diff_bytes"),
                    breakdown.get("value_diffs"),
                    breakdown.get("logs_bytes"),
                    breakdown.get("logs_count"),
                    breakdown.get("message_payload_bytes"),
                    breakdown.get("messages_count"),
                    breakdown.get("total_pubdata_bytes"),
                ]
            )
        )


if __name__ == "__main__":
    main()
