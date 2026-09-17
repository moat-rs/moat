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

"""Fill authorized devices with mixed records, then measure parallel recovery."""

import argparse
import fcntl
import shutil
import json
from pathlib import Path
import socket
import subprocess
import time
import traceback

parser = argparse.ArgumentParser(description='Destructive mixed-record fill and parallel V2 recovery')
parser.add_argument('--config', type=Path, required=True)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--overwrite-entire-devices', action='store_true')
parser.add_argument('--smoke-files', action='store_true')
args = parser.parse_args()
cfg = json.loads(args.config.read_text())
assert args.overwrite_entire_devices or args.smoke_files, 'explicit destructive scope required'
assert not args.output.exists(), 'output already exists'
root = args.output.resolve()
root.mkdir(parents=True)
cfg_path = root / 'config.json'
cfg['smoke'] = args.smoke_files
cfg_path.write_text(json.dumps(cfg, indent=2) + '\n')
shutil.copy2(args.binary, root / 'recovery')
children = []
locks = []
status = dict(phase='preflight', started_at=time.time(), rounds=[])


def state(**changes):
    status.update(changes)
    tmp = root / 'status.tmp'
    tmp.write_text(json.dumps(status, indent=2) + '\n')
    tmp.replace(root / 'status.json')


def stats():
    return [list(map(int, (Path('/sys/class/block') / Path(d['path']).name / 'stat').read_text().split()))
            if (Path('/sys/class/block') / Path(d['path']).name / 'stat').exists() else [0]*17
            for d in cfg['disks']]


def preflight():
    assert socket.gethostname() == cfg['host']
    assert len(cfg['disks']) == len(cfg['io_cpus'])
    assert len({Path(d['path']).resolve() for d in cfg['disks']}) == len(cfg['disks'])
    assert len(cfg['disks']) > 0
    for d in cfg['disks']:
        p = Path(d['path']).resolve()
        if not p.is_block_device():
            assert cfg.get('smoke', False) and p.is_file() and p.stat().st_size == d['expected_capacity']
            continue
        b = Path('/sys/class/block') / p.name
        assert not args.smoke_files, 'smoke mode refuses block devices'
        assert cfg['forbidden_serials'] and d['serial'] not in cfg['forbidden_serials']
        lock = open('/tmp/moat-bench-' + p.name + '.lock', 'a')
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        locks.append(lock)
        assert (b / 'device/serial').read_text().strip() == d['serial']
        assert int((b / 'size').read_text()) * 512 == d['expected_capacity']
        assert not list((b / 'holders').iterdir())
        assert not (b / 'partition').exists()
        assert not any((p / 'partition').exists() for p in b.iterdir())
        assert not any(l.split()[2] == (b / 'dev').read_text().strip() for l in Path('/proc/self/mountinfo').read_text().splitlines())
        assert subprocess.run(['fuser', str(p)], capture_output=True).returncode == 1
        assert not json.loads(subprocess.check_output(['wipefs', '--no-act', '--json', str(p)]))['signatures']
    before = stats()
    time.sleep(2)
    after = stats()
    assert all(a[:8] == b[:8] and a[8] == 0 for a, b in zip(after, before)), 'devices not idle'
    (root / 'preflight.json').write_text(json.dumps(dict(devices=len(cfg['disks']), idle=True,
        authorized_identities_match=True, before=before, after=after, mdstat=Path('/proc/mdstat').read_text()), indent=2))


def records(log, prefix):
    return [json.loads(s[len(prefix)+1:]) for s in log.read_text().splitlines() if s.startswith(prefix+' ')]


def run(mode, label):
    global children
    go = root / (label + '.go')
    assert not go.exists()
    files = []
    paths = []
    children = []
    ready_start = time.monotonic()
    for i in range(len(cfg['disks'])):
        path = root / f'{label}-{i:02}.log'
        assert not path.exists()
        f = path.open('w')
        files.append(f)
        paths.append(path)
        children.append(subprocess.Popen([str(root/'recovery'), str(cfg_path), str(i), mode, str(go)], stdout=f, stderr=subprocess.STDOUT))
    state(phase=label+'_setup', pids=[c.pid for c in children])
    while not all(records(p, 'READY') for p in paths):
        assert all(c.poll() is None for c in children), 'worker exited during setup'
        assert time.monotonic()-ready_start < 600, 'setup timeout'
        time.sleep(0.2)
    before = stats()
    scheduled = time.monotonic() + 1
    tmp = go.with_suffix('.tmp')
    tmp.write_text(str(scheduled))
    tmp.replace(go)
    state(phase=label, go=scheduled, setup_seconds=time.monotonic()-ready_start)
    timeout = 28800 if mode == 'fill' else 1800
    prefix = 'FILLED' if mode == 'fill' else 'RECOVERED'
    next_progress = 0
    while not all(records(p, prefix) for p in paths):
        assert time.monotonic()-scheduled < timeout, 'phase timeout'
        assert all(c.poll() in (None, 0) for c in children), 'worker failed'
        assert all(c.poll() is None or records(p, prefix) for c,p in zip(children,paths)), 'worker exited before result'
        if time.monotonic() >= next_progress:
            progress = [(records(p,'FILLED') or records(p,'PROGRESS') or [None])[-1] for p in paths]
            state(elapsed=max(0,time.monotonic()-scheduled), progress=progress)
            next_progress = time.monotonic()+30
        time.sleep(0.1)
    after = stats()
    result = [records(p,prefix)[-1] for p in paths]
    if mode == 'recover':
        (root/(label+'.go.verify')).touch()
    for child in children:
        assert child.wait(timeout=600) == 0
    for f in files:
        f.close()
    if mode == 'recover':
        assert all(records(p,'VERIFIED')[-1]['samples'] == 505 for p in paths)
    summary = dict(label=label, measurements=result, diskstats_before=before, diskstats_after=after)
    if mode == 'recover':
        summary.update(all_ready_seconds=max(r['end'] for r in result)-scheduled,
                       active_span_seconds=max(r['end'] for r in result)-min(r['start'] for r in result),
                       verified_samples=505*len(result))
    (root/(label+'.json')).write_text(json.dumps(summary,indent=2)+'\n')
    children = []
    return summary


try:
    state()
    preflight()
    filled = run('fill', 'fill')
    cfg['records'] = [v['records'] for v in filled['measurements']]
    cfg_path.write_text(json.dumps(cfg, indent=2)+'\n')
    for n in range(1,4):
        result=run('recover', f'recovery-{n}')
        state(rounds=status['rounds']+[dict(round=n,all_ready_seconds=result['all_ready_seconds'])])
    state(phase='complete',finished_at=time.time(),pids=[])
except BaseException:
    for child in children:
        if child.poll() is None:
            child.terminate()
    for child in children:
        try: child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    state(phase='failed', error=traceback.format_exc(),pids=[])
    raise
finally:
    for lock in locks:
        lock.close()
