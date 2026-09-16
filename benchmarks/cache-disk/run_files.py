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

"""Compare disk-cache APIs using new disposable files on Linux."""

import argparse
import json
import os
import socket
import subprocess
import time
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--output", type=Path)
parser.add_argument("--key-bytes", type=int, default=16)
parser.add_argument("--value-bytes", type=int, default=4096)
parser.add_argument("--seconds", type=int, default=5)
parser.add_argument("--engines", nargs="+", default=["foyer", "v1", "v2"])
parser.add_argument("--repeats", type=int, default=3)
parser.add_argument("--records", type=int, default=256)
parser.add_argument("--disks", type=int, default=1)
parser.add_argument("--verify-reads", action="store_true")
parser.add_argument("--engine-preassembled-input", action="store_true")
parser.add_argument("--v2-batch-large-records", action="store_true")
parser.add_argument("--engine-in-place-input", action="store_true")
args = parser.parse_args()
cpus = sorted(os.sched_getaffinity(0))
if args.disks < 1 or len(cpus) < args.disks + 2:
    parser.error("one I/O CPU per disk and two runtime CPUs are required")
root = args.output or Path(__file__).resolve().parent / "local" / str(time.time_ns())
root.mkdir(parents=True, exist_ok=False)
binary = args.binary.resolve(strict=True)
for engine in args.engines:
    disks = []
    for index in range(args.disks):
        disk = root / f"{engine}-{index}.img"
        with disk.open("xb") as output:
            output.truncate(1 << 30)
        disks.append({"path": str(disk.resolve()), "serial": ""})
    config = {
        "host": socket.gethostname(),
        "engine": engine,
        "disks": disks,
        "bytes_per_disk": 1 << 30,
        "records_per_disk": args.records,
        "key_bytes": args.key_bytes,
        "value_bytes": args.value_bytes,
        "clients": max(128, args.disks),
        "client_levels": [max(n, args.disks) for n in (8, 32, 128)],
        "runtime_cpus": cpus[args.disks:args.disks+2],
        "io_cpus": cpus[:args.disks],
        "seconds": args.seconds,
        "repeats": args.repeats,
        "pool_bytes_per_disk": 64 << 20,
        "prefill_batch": max(args.disks, min(256, (64 << 20) // (args.key_bytes + args.value_bytes) // 4)),
        "moat_verify_reads": args.verify_reads,
        "engine_segment_bytes": 16 << 20,
        "engine_preassembled_input": args.engine_preassembled_input and engine in ("v1", "v2"),
        "v2_batch_large_records": args.v2_batch_large_records and engine == "v2",
        "engine_in_place_input": args.engine_in_place_input and engine in ("v1", "v2"),
    }
    name = f"{engine}-d{args.disks}-k{args.key_bytes}-v{args.value_bytes}"
    path = root / (name + ".json")
    path.write_text(json.dumps(config, indent=2) + "\n")
    log_path = root / (name + ".log")
    with log_path.open("w") as log:
        subprocess.run(
            [str(binary), str(path)], stdout=log, stderr=subprocess.STDOUT, check=True
        )
    lines = log_path.read_text().splitlines()
    assert f"VERIFIED {args.records * args.disks}" in lines
    assert sum(line.startswith("RESULT ") for line in lines) == args.repeats * 3
print(root)
