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

"""Run a destructive comparison in the first 4 GiB of a verified scratch NVMe."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", type=Path, required=True)
    parser.add_argument("--serial", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cpu", type=int, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--seconds", type=int, default=10)
    parser.add_argument("--payload-mib", type=int, default=512)
    parser.add_argument("--overwrite-first-4g", action="store_true", required=True)
    args = parser.parse_args()
    if args.repeats < 1 or args.seconds < 1 or not 1 <= args.payload_mib <= 512:
        parser.error("repeats/seconds must be positive; payload must be 1..512 MiB")
    device = args.device.resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    sys = Path("/sys/class/block") / device.name
    require(stat.S_ISBLK(device.stat().st_mode), "expected a block device")
    require((sys / "device/serial").read_text().strip() == args.serial, "wrong device identity")
    require(not (sys / "partition").exists(), "expected a whole scratch device")
    require(not list((sys / "holders").iterdir()), "device has holders")
    info = json.loads(subprocess.check_output(["lsblk", "-J", "-o", "NAME,TYPE,FSTYPE,MOUNTPOINTS", str(device)]))["blockdevices"]
    require(len(info) == 1 and not info[0].get("children"), "device has partitions")
    require(not info[0]["fstype"] and not any(info[0]["mountpoints"]), "device has a filesystem or mount")
    require(subprocess.check_output(["wipefs", "--no-act", str(device)]) == b"", "device has signatures")
    require(args.cpu in os.sched_getaffinity(0), "CPU is outside allowed affinity")
    args.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    # Coordinate other runs of this harness; this cannot exclude unrelated users.
    with (Path("/tmp") / f"moat-bench-{device.name}.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(subprocess.run(["fuser", str(device)], capture_output=True).returncode == 1, "device has users or fuser failed")
        before = (sys / "stat").read_text()
        time.sleep(1)
        require(before == (sys / "stat").read_text(), "device has active I/O")
        for repeat in range(args.repeats):
            for size in ["100", "1024", "4096", "65536", "4194304", "mixed"]:
                engines = ["legacy", "v2"] if repeat % 2 == 0 else ["v2", "legacy"]
                for engine in engines:
                    name = f"engine-{repeat}-{size}-{engine}"
                    output = args.output / f"{name}.jsonl"
                    errors = args.output / f"{name}.stderr"
                    qds = "1,16,64" if size == "4194304" else "1,64"
                    command = ["taskset", "-c", str(args.cpu), str(binary), str(device), engine, size,
                               str(args.payload_mib), str(args.seconds), qds, "--overwrite-first-4g"]
                    print("START", name, flush=True)
                    # Never silently overwrite an earlier measurement.
                    with output.open("x") as out, errors.open("x") as err:
                        subprocess.run(command, stdout=out, stderr=err, check=True,
                                       timeout=120 + len(qds.split(",")) * (args.seconds + 5))
                    for line in output.read_text().splitlines():
                        row = json.loads(line)
                        print("DONE", name, row["phase"], row["qd"],
                              round(row["gib_s"], 3), "GiB/s", round(row["ops_s"]), "ops/s", flush=True)


if __name__ == "__main__":
    main()
