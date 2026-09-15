#!/usr/bin/env python3
# Copyright 2026- Moat Project Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Aggregate numeric engine JSONL samples without copying operational logs."""

import argparse
from collections import defaultdict
import csv
import json
from pathlib import Path
from statistics import median


def write_csv(path, rows):
    with path.open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    rows = []
    groups = defaultdict(list)
    for path in sorted(args.input.glob("engine-*-*.jsonl")):
        repeat = int(path.stem.split("-")[1])
        for line in path.read_text().splitlines():
            row = {"repeat": repeat, **json.loads(line)}
            if row["operations"] <= 0 or not row["durable_flush"]:
                raise ValueError(f"invalid sample: {path.name}")
            row.setdefault("read_range", "full")
            row.setdefault("huge_pages", "Disabled")
            memory = row.pop("memory", {})
            row.update({f"memory_{key}": value for key, value in memory.items()})
            row["device_read_bytes_per_op"] = row["device_read_bytes"] / row["operations"]
            row["device_write_amplification"] = row["device_write_bytes"] / row["payload_bytes"]
            row["cpu_percent"] = (row["cpu_user_s"] + row["cpu_system_s"]) / row["seconds"] * 100
            rows.append(row)
            groups[(row["workload"], row["phase"], row["qd"], row["engine"], row["verified_reads"], row["read_range"], row["huge_pages"])].append(row)
    if not rows:
        raise ValueError("no engine samples")
    summaries = []
    for (workload, phase, qd, engine, verify, read_range, huge_pages), samples in sorted(groups.items()):
        if len({row["repeat"] for row in samples}) != len(samples):
            raise ValueError("duplicate repetition")
        summary = {"workload": workload, "phase": phase, "qd": qd, "engine": engine, "samples": len(samples), "verified_reads": verify, "read_range": read_range, "huge_pages": huge_pages}
        for column in ["gib_s", "ops_s", "p50_us", "p99_us", "p999_us", "device_read_bytes_per_op", "device_write_amplification", "cpu_percent"]:
            values = [row[column] for row in samples if row[column] is not None]
            summary[column] = median(values) if values else None
        summary["min_ops_s"] = min(row["ops_s"] for row in samples)
        summary["max_ops_s"] = max(row["ops_s"] for row in samples)
        summaries.append(summary)
    args.output.mkdir(parents=True, exist_ok=True)
    write_csv(args.output / "samples.csv", rows)
    write_csv(args.output / "summary.csv", summaries)
    print(f"Wrote {len(rows)} samples and {len(summaries)} configurations")


if __name__ == "__main__":
    main()
