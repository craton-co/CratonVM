#!/usr/bin/env python3
"""Fresh-process HotSpot worker-count, refusal, and allocation parity gate.

Compile G1ParallelGcThreadsProbe.java into --classes first. All commands and
complete output are retained in --output, with no build or environment changes.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--vm', required=True)
    p.add_argument('--java', default='java')
    p.add_argument('--classes', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--collectors', nargs='+', default=['G1', 'Z', 'Serial'])
    p.add_argument('--modes', nargs='+', default=['jdk-only', 'compatible'])
    a = p.parse_args()
    out = Path(a.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    records = []

    def run(label, exe, flags, extra_env=None):
        command = [exe, *flags]
        env = os.environ.copy()
        env.update(extra_env or {})
        r = subprocess.run(command, capture_output=True, timeout=240, env=env)
        (out / (label + '.stdout')).write_bytes(r.stdout)
        (out / (label + '.stderr')).write_bytes(r.stderr)
        records.append(dict(label=label, command=command, exit=r.returncode, environment=extra_env or {}))
        (out / 'results.json').write_text(json.dumps(records, indent=2))
        return r

    (out / 'environment.json').write_text(json.dumps({k: v for k, v in os.environ.items()
        if k.startswith(('JAVA_', 'CRATONVM_', 'RUST_'))}, indent=2))
    pairs = 0
    for mode_name in a.modes:
        mode = '--' + mode_name
        for gc in a.collectors:
            common = [f'-XX:+Use{gc}GC', '-Xms64m', '-Xmx64m']
            # Include repeated flags, hex, uppercase hex, and suffix syntax.
            for name, values, want in [
                ('one', ['1'], 1), ('two', ['2'], 2), ('four', ['4'], 4),
                ('hex', ['0x2'], 2), ('hex-upper', ['0X2'], 2),
                ('zero-then-two', ['0', '2'], 2), ('last', ['4', '2'], 2),
                ('over-cpus', [str((os.cpu_count() or 1) + 2)], (os.cpu_count() or 1) + 2),
            ]:
                flags = common + [f'-XX:ParallelGCThreads={v}' for v in values]
                label = f'{mode[2:]}-{gc}-{name}'
                hs = run(label + '-hs', a.java, flags + ['-Xlog:gc+init', '-version'])
                vm = run(label + '-vm', a.vm, [mode, *flags, '-Xlog:gc+init', '-version'])
                assert hs.returncode == vm.returncode == 0, label
                assert b'Warning: ignoring' not in vm.stderr, label
                if gc in ('G1', 'Z'):
                    for r in (hs, vm):
                        pattern = rb'Parallel Workers: (\d+)' if gc == 'G1' else rb'Runtime Workers: (\d+)'
                        workers = re.search(pattern, r.stdout + r.stderr)
                        assert workers and int(workers[1]) == want, label
                pairs += 1
            for name, values in [
                ('zero', ['0']), ('hex-zero', ['0x0']), ('suffix-zero', ['0k']),
                ('last-zero', ['2', '0']), ('bad', ['abc']), ('negative', ['-1']),
                ('plus', ['+2']), ('empty', ['']), ('overflow', ['4294967296']),
                ('bad-then-good', ['abc', '2']),
            ]:
                flags = common + [f'-XX:ParallelGCThreads={v}' for v in values]
                for target, tail in [('version', ['-version']), ('main', ['-cp', a.classes, 'G1ParallelGcThreadsProbe'])]:
                    label = f'{mode[2:]}-{gc}-{name}-{target}'
                    hs = run(label + '-hs', a.java, flags + tail)
                    vm = run(label + '-vm', a.vm, [mode, *flags, *tail])
                    assert hs.returncode == vm.returncode, label
                    if hs.returncode:
                        assert (hs.stdout, hs.stderr) == (vm.stdout, vm.stderr), label
                    elif target == 'main':
                        assert hs.stdout == vm.stdout and hs.stderr == vm.stderr, label
                    else:
                        assert b'Warning: ignoring' not in vm.stderr, label
                    pairs += 1
            for n in (1, 2, 4):
                flags = common + [f'-XX:ParallelGCThreads={n}', '-Xlog:gc+init',
                    '-cp', a.classes, 'G1ParallelGcThreadsProbe']
                label = f'{mode[2:]}-{gc}-allocation-{n}'
                hs = run(label + '-hs', a.java, flags)
                vm = run(label + '-vm', a.vm, [mode, *flags])
                assert hs.returncode == vm.returncode == 0, label
                checksum = lambda r: re.findall(rb'G1-THREADS-OK checksum=-?\d+', r.stdout)
                assert checksum(hs) and checksum(hs) == checksum(vm), label
                if gc == 'G1':
                    assert f'Parallel Workers: {n}'.encode() in vm.stdout + vm.stderr, label
                pairs += 1
        # Prove the requested count reaches a real evacuation, not just a log.
        flags = ['-XX:+UseG1GC', '-XX:ParallelGCThreads=2', '-Xms64m', '-Xmx64m',
                 '-cp', a.classes, 'G1ParallelGcThreadsProbe']
        label = f'{mode[2:]}-live-workers'
        hs = run(label + '-hs', a.java, flags)
        vm = run(label + '-vm', a.vm, [mode, *flags],
                 {'CRATONVM_GC_STATS': '1', 'CRATONVM_G1_EVAC_WORKERS_BY_POOL': '0'})
        assert hs.returncode == vm.returncode == 0 and hs.stdout == vm.stdout, label
        assert re.search(rb'g1 evac-workers: rows=2 .*helpers_dispatched=1', vm.stderr), label
        pairs += 1
        # Preserve the launcher's legacy spellings and mixed-spelling ordering.
        for alias_index, flags in enumerate([
            ['--XX:ParallelGCThreads', '2'],
            ['-XX:ParallelGCThreads=4', '--XX:ParallelGCThreads=2'],
            ['--XX:ParallelGCThreads=4', '-XX:ParallelGCThreads=2'],
            ['-XX:ParallelGCThreads=0', '--XX:ParallelGCThreads=2'],
        ]):
            label = f'{mode[2:]}-alias-{alias_index}'
            r = run(label, a.vm, [mode, '-XX:+UseG1GC', '-Xmx64m', *flags,
                '-Xlog:gc+init', '-version'])
            assert r.returncode == 0 and b'Parallel Workers: 2' in r.stdout + r.stderr, label
        # No collector: keep HotSpot's default-G1 zero refusal verbatim.
        hs = run(f'{mode[2:]}-default-zero-hs', a.java, ['-XX:ParallelGCThreads=0', '-version'])
        vm = run(f'{mode[2:]}-default-zero-vm', a.vm, [mode, '-XX:ParallelGCThreads=0', '-version'])
        assert (hs.returncode, hs.stdout, hs.stderr) == (vm.returncode, vm.stdout, vm.stderr)
        pairs += 1
    print(f'G1_PARALLEL_THREADS_GATE_OK pairs={pairs} processes={len(records)}')


if __name__ == '__main__':
    main()
