#!/usr/bin/env python3
"""Fresh-process Reference.get acceptance against matching HotSpot collectors.

Build the three Java sources in this directory into --classes first. Every
stdout, stderr, command, environment override and binary hash is retained.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--vm', required=True)
    p.add_argument('--classes', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--java', default='java')
    p.add_argument('--timeout', type=int, default=240)
    p.add_argument('--quick', action='store_true')
    a = p.parse_args()
    out = Path(a.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    inherited = {k: v for k, v in env.items() if k.startswith('CRATONVM')}
    (out / 'metadata.json').write_text(json.dumps(dict(
        vm=str(Path(a.vm).resolve()), sha256=hashlib.sha256(Path(a.vm).read_bytes()).hexdigest(),
        inherited=inherited), indent=2))
    records = []
    def run(label, exe, flags, probe, overrides=None):
        command = [exe, *flags, '-Xmx128m', '-cp', str(Path(a.classes).resolve()), probe]
        start = time.monotonic()
        try:
            r = subprocess.run(command, env={**env, **(overrides or {})}, capture_output=True, timeout=a.timeout)
            rc, stdout, stderr = r.returncode, r.stdout, r.stderr
        except subprocess.TimeoutExpired as e:
            rc, stdout, stderr = -999, e.stdout or b'', e.stderr or b''
        (out / (label + '.stdout')).write_bytes(stdout)
        (out / (label + '.stderr')).write_bytes(stderr)
        records.append(dict(label=label, command=command, overrides=overrides or {}, exit=rc,
                            seconds=round(time.monotonic() - start, 3)))
        (out / 'results.json').write_text(json.dumps(records, indent=2))
        if rc:
            raise RuntimeError(f'{label}: exit={rc}')
        return stdout
    probes = ['J27WeakRefSpliceRow', 'J27SameTargetReferenceGet', 'J27RefsGcChurn']
    profiles = [('zgc', 'Z', 'Z', {}), ('g1', 'G1', 'G1', {}),
                ('gen', 'Generational', 'Serial', {})]
    if not a.quick:
        profiles += [('zgc-concurrent', 'Z', 'Z', {'CRATONVM_ZGC_CONCURRENT': '1'}),
                     ('zgc-color', 'Z', 'Z', {'CRATONVM_ZGC_COLOR_FLIP': '1'})]
    comparisons = 0
    for profile, gc, oracle, extra in profiles:
        for probe in probes:
            expected = run(f'{profile}-{probe}-hotspot', a.java, [f'-XX:+Use{oracle}GC'], probe)
            for mode in ('jdk-only', 'compatible'):
                for arm in ('0', '1', 'default'):
                    label = f'{profile}-{probe}-{mode}-row{arm}'
                    actual = run(label, a.vm, ['--' + mode, f'-XX:+Use{gc}GC'], probe,
                                 {**extra, **({'CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW': arm}
                                            if arm != 'default' else {})})
                    if actual != expected:
                        raise AssertionError(f'{label}: stdout differs from fresh HotSpot')
                    comparisons += 1
                    print(f'PASS {label}', flush=True)
    # A correct fact result alone does not prove that C2 installed the splice.
    for probe, kernel in [('J27WeakRefSpliceRow', 'getKernel'),
                          ('J27SameTargetReferenceGet', 'shared')]:
        for mode in ('jdk-only', 'compatible'):
            label = f'engagement-{probe}-{mode}'
            run(label, a.vm, ['--' + mode, '-XX:+UseZGC'], probe,
                {'CRATONVM_DBG_JITC': '1', 'CRATONVM_DBG_IR_COMPILES': '1',
                 'CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW': '1'})
            diagnostic = (out / (label + '.stderr')).read_text()
            method = probe + '.' + kernel
            lines = diagnostic.splitlines()
            assert any('ir-same-target-splice ' in line and method in line for line in lines), label
            assert any('inline-plan ' + method in line and '1 spliced body' in line for line in lines), label
            assert 'optimizing backend produced a body for ' + method in diagnostic, label
            assert not any('schedule rejected ' + method in line for line in lines), label
    print(f'SAME_TARGET_REFERENCE_OK comparisons={comparisons} engagement=4 processes={len(records)}', flush=True)


if __name__ == '__main__':
    main()
