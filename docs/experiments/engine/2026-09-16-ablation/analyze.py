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

"""Reproduce ablation summaries from the adjacent, anonymized sample CSVs."""

import csv
import math
from pathlib import Path
import statistics

ROOT = Path(__file__).resolve().parent
CONTROLS = {"control_before", "control_after"}


def write_csv(name, rows):
    with (ROOT / name).open("w") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


groups = {}
paired_groups = {}
for phase, filename in [("read", "samples.csv"), ("write", "prefill.csv")]:
    with (ROOT / filename).open() as source:
        for row in csv.DictReader(source):
            key = (row["suite"], phase, int(row["value_bytes"]), int(row["clients"]))
            variant = "baseline" if row["variant"] in CONTROLS else row["variant"]
            groups.setdefault((*key, variant), []).append(row)
            paired_groups.setdefault((*key, int(row["round"])), {})[row["variant"]] = row

summary = []
for key, rows in sorted(groups.items()):
    suite, phase, size, clients, variant = key
    entry = dict(suite=suite, phase=phase, value_bytes=size, clients=clients,
                 variant=variant, samples=len(rows))
    for field in ["ops_per_second", "logical_gib_s", "physical_gib_s",
                  "cpu_cores", "cpu_us_per_op", "seconds", "p50_us", "p99_us", "p999_us"]:
        entry[field] = statistics.median(float(r[field]) for r in rows) if field in rows[0] else ""
    rates = [float(r["ops_per_second"]) for r in rows]
    entry.update(min_ops_per_second=min(rates), max_ops_per_second=max(rates))
    summary.append(entry)
write_csv("summary.csv", summary)

pairs = []
for key, rows in sorted(paired_groups.items()):
    if not CONTROLS.issubset(rows):
        continue
    suite, phase, size, clients, round_number = key
    before, after = rows["control_before"], rows["control_after"]
    control_rate = math.sqrt(float(before["ops_per_second"]) * float(after["ops_per_second"]))
    for variant, row in sorted(rows.items()):
        if variant in CONTROLS:
            continue
        entry = dict(suite=suite, phase=phase, value_bytes=size, clients=clients,
                     round=round_number, variant=variant, run=row["run"],
                     before_run=before["run"], after_run=after["run"],
                     ops_per_second=float(row["ops_per_second"]),
                     control_ops_per_second=control_rate,
                     throughput_ratio=float(row["ops_per_second"]) / control_rate,
                     control_after_before_ratio=float(after["ops_per_second"]) / float(before["ops_per_second"]))
        for field in ["p99_us", "cpu_us_per_op"]:
            entry[field + "_ratio"] = (
                float(row[field]) / math.sqrt(float(before[field]) * float(after[field]))
                if field in row else ""
            )
        pairs.append(entry)
write_csv("pairs.csv", pairs)

paired_summary = []
groups = {}
for row in pairs:
    key = tuple(row[k] for k in ["suite", "phase", "value_bytes", "clients", "variant"])
    groups.setdefault(key, []).append(row)
for key, rows in sorted(groups.items()):
    entry = dict(zip(["suite", "phase", "value_bytes", "clients", "variant"], key))
    entry["pairs"] = len(rows)
    for field in ["throughput_ratio", "p99_us_ratio", "cpu_us_per_op_ratio"]:
        values = [r[field] for r in rows if r[field] != ""]
        for label, reduce in [("median", statistics.median), ("min", min), ("max", max)]:
            entry[f"{label}_{field}"] = reduce(values) if values else ""
    paired_summary.append(entry)
write_csv("paired-summary.csv", paired_summary)
print(f"{len(summary)} absolute groups; {len(pairs)} pairs; {len(paired_summary)} paired groups")
