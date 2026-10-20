#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Five interleaved off/default/direct-lazy rounds in both modes and all collectors."""
import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
from gate_diagnostics import clean_diagnostics

parser = argparse.ArgumentParser(description=__doc__)
for name in ['binary', 'jdk', 'classes', 'logs']:
    parser.add_argument(name, type=Path)
args = parser.parse_args()
binary, jdk, classes, logs = (getattr(args, n).resolve() for n in ['binary', 'jdk', 'classes', 'logs'])
for path in [binary, jdk, classes]:
    if not path.exists():
        parser.error(f'missing input: {path}')
logs.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, JAVA_HOME=str(jdk))
env['PATH'] = str(jdk / 'bin') + os.pathsep + env['PATH']
for key in ['CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY', 'CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY']:
    env.pop(key, None)
arms = [('off', {'CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY': '0'}), ('default', {}),
        ('direct-lazy', {'CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY': '1'})]
rows = defaultdict(list)
with (logs / 'runs.txt').open('w') as summary:
    for mode in ['jdk-only', 'compatible']:
        for collector in ['ZGC', 'G1GC', 'GenerationalGC']:
            for probe in ['R16Ffm3Stw', 'R16FfmDowncallCost', 'R17FfmsegCost']:
                for round_number in range(1, 6):
                    for arm, settings in arms:
                        base = logs / f'{mode}-{collector}-{probe}-{round_number}-{arm}'
                        command = [str(binary), '--' + mode, '-XX:+Use' + collector, '-Xmx128m',
                                   '--enable-native-access=ALL-UNNAMED', '-cp', str(classes), probe]
                        with base.with_suffix('.out').open('w') as out, base.with_suffix('.err').open('w') as err:
                            try:
                                rc = subprocess.run(command, env=dict(env, **settings), stdout=out,
                                                    stderr=err, timeout=90).returncode
                            except subprocess.TimeoutExpired:
                                rc = 124
                        output = base.with_suffix('.out').read_text(errors='replace')
                        errors = base.with_suffix('.err').read_text(errors='replace')
                        if probe == 'R16Ffm3Stw':
                            values = dict(re.findall(r'^(cost-llabs(?:-critical)?-ns) (\d+)$', output, re.M))
                            correct = 'worker sleep: ok' in output and ('PASS' in output.splitlines() or
                                       (arm == 'off' and 'FAIL stall' in output.splitlines()))
                        elif probe == 'R16FfmDowncallCost':
                            values = dict(re.findall(r'^\[r16-ffm-cost2\] (\S+) median=(\d+) ns/call$', errors, re.M))
                            values.pop('mh-java', None)
                            correct = 'PASS' in output.splitlines()
                        else:
                            values = dict(re.findall(r'^(\S+) (\d+) ns/op$', output, re.M))
                            correct = 'PASS' in output.splitlines()
                        if (rc or not correct or not values or not clean_diagnostics(output, errors)
                                or any(int(v) <= 0 for v in values.values())):
                            print(f'FAIL {base.name} rc={rc}', flush=True)
                            sys.exit(1)
                        for label, value in values.items():
                            rows[(mode, collector, probe, label, arm)].append(int(value))
                        summary.write(f'PASS {base.name} rc=0\n')
                        summary.flush()
                        print(f'PASS {base.name}', flush=True)
medians = {key: statistics.median(values) for key, values in rows.items()}
failed = False
records = []
for key, off in medians.items():
    if key[-1] != 'off':
        continue
    prefix = key[:-1]
    default = medians.get(prefix + ('default',))
    direct = medians.get(prefix + ('direct-lazy',))
    complete = all(len(rows.get(prefix + (arm,), [])) == 5 for arm, _ in arms)
    # The five arms are deliberately interleaved. Compare corresponding
    # rounds so a host-frequency or scheduling shift between rounds cannot
    # manufacture a ratio by dividing two independent medians.
    off_samples = rows.get(prefix + ('off',), [])
    default_samples = rows.get(prefix + ('default',), [])
    paired_ratios = [on / baseline for baseline, on in zip(off_samples, default_samples)
                     if baseline > 0]
    ratio = statistics.median(paired_ratios) if paired_ratios else None
    ok = complete and ratio <= 1.5
    failed |= not ok
    record = dict(zip(['mode', 'collector', 'probe', 'metric'], prefix))
    record.update(off=off, default=default, direct_lazy=direct,
                  paired_ratios=paired_ratios, ratio=ratio, passed=ok)
    records.append(record)
    print(f'{"PASS" if ok else "FAIL"} {" ".join(prefix)} off={off} default={default} '
          f'direct={direct} paired_ratio={ratio}', flush=True)
(logs / 'medians.json').write_text(json.dumps(records, indent=2) + '\n')
if failed:
    sys.exit(1)
print('FFM_COST_GATE_OK runs=270', flush=True)
