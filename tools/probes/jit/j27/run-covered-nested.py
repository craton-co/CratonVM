#!/usr/bin/env python3
"""Fresh-process covered-nested acceptance; retain commands, hashes and full output.

Compile J27CoveredCallRows, J27T1L1CoveredNestedRows, J27DecRows and R21CfChain
into --classes. The semantic gate compares complete stdout with HotSpot 25.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import time

FLAGS = ['CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN',
         'CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED',
         'CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH',
         'CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER',
         'CRATONVM_JIT_IR_STATIC_FINAL_BOOT_RECEIVER']
PROBES = ['J27T1L1CoveredNestedRows', 'J27DecRows', 'R21CfChain', 'J27CoveredCallRows', 'J27CoveredAllocationFailure']


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--vm', required=True)
    p.add_argument('--classes', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--java', default='java')
    p.add_argument('--diagnostic', action='store_true')
    p.add_argument('--quick', action='store_true')
    a = p.parse_args()
    out = Path(a.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    vm = str(Path(a.vm).resolve())
    env = {k: v for k, v in os.environ.items() if not k.startswith('CRATONVM')}
    records = []
    (out / 'metadata.json').write_text(json.dumps(dict(vm=vm,
        sha256=hashlib.sha256(Path(vm).read_bytes()).hexdigest(),
        source=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()), indent=2))

    def run(label, exe, flags, probe, overrides):
        properties = ['-Dr21.factsOnly=true'] if probe == 'R21CfChain' else []
        heap = '-Xmx32m' if probe == 'J27CoveredAllocationFailure' else '-Xmx256m'
        cmd = [exe, *flags, heap, *properties, '-cp', str(Path(a.classes).resolve()), probe]
        started = time.monotonic()
        try:
            r = subprocess.run(cmd, env={**env, **overrides}, capture_output=True, timeout=240)
            rc, stdout, stderr = r.returncode, r.stdout, r.stderr
        except subprocess.TimeoutExpired as e:
            rc, stdout, stderr = -999, e.stdout or b'', e.stderr or b''
        (out / (label + '.stdout')).write_bytes(stdout)
        (out / (label + '.stderr')).write_bytes(stderr)
        records.append(dict(label=label, command=cmd, overrides=overrides,
                            exit=rc, seconds=round(time.monotonic()-started, 3)))
        (out / 'results.json').write_text(json.dumps(records, indent=2))
        if rc:
            raise RuntimeError(f'{label}: exit={rc}')
        return b"\n".join(line for line in stdout.splitlines() if not line.startswith(b"time "))

    arms = [('default', {})]
    if not a.quick:
        arms += [(letter, {f: '1' if i < count else '0' for i, f in enumerate(FLAGS)})
                 for letter, count in [('A', 0), ('B', 1), ('C', 2), ('D', len(FLAGS))]]
    comparisons = 0
    expected_by_probe = {}
    for gc, oracle in [('Z', 'Z'), ('G1', 'G1'), ('Generational', 'Serial')]:
        if a.quick and gc != 'Z':
            continue
        for probe in PROBES:
            expected = run(f'{gc}-{probe}-hotspot', a.java, [f'-XX:+Use{oracle}GC'], probe, {})
            expected_by_probe[probe] = expected
            for mode in ['jdk-only', 'compatible']:
                for arm, overrides in arms:
                    label = f'{gc}-{probe}-{mode}-{arm}'
                    actual = run(label, vm, ['--'+mode, f'-XX:+Use{gc}GC'], probe, overrides)
                    if actual != expected:
                        raise AssertionError(f'{label}: stdout differs from HotSpot')
                    comparisons += 1
                    print('PASS', label, flush=True)
                if not a.quick:
                    label = f'{gc}-{probe}-{mode}-nojit'
                    actual = run(label, vm, ['--'+mode, '--nojit', f'-XX:+Use{gc}GC'], probe, {})
                    if actual != expected:
                        raise AssertionError(f'{label}: interpreter stdout differs from HotSpot')
                    comparisons += 1
                    print('PASS', label, flush=True)
    if a.diagnostic:
        for mode in ['jdk-only', 'compatible']:
            for probe in PROBES:
                label = f'diagnostic-{mode}-{probe}'
                actual = run(label, vm, ['--'+mode], probe,
                    {**{f: '1' for f in FLAGS}, 'CRATONVM_DBG_JITC': '1',
                     'CRATONVM_DBG_IR_COMPILES': '1', 'CRATONVM_DBG_IR_BAIL': '1'})
                if actual != expected_by_probe[probe]:
                    raise AssertionError(f'{label}: diagnostic arm differs from HotSpot')
                comparisons += 1
        for mode in ['jdk-only', 'compatible']:
            diagnostic = (out / f'diagnostic-{mode}-J27DecRows.stderr').read_text()
            lines = diagnostic.splitlines()
            assert any('ir-splice-bytes java/nio/ByteBuffer.wrap' in line
                       and 'in J27DecRows.wrap(I)J' in line for line in lines), mode
            assert 'optimizing backend produced a body for J27DecRows.wrap(I)J' in diagnostic, mode
            assert 'optimizing backend produced a body for J27DecRows.dec3' in diagnostic, mode
            assert 'ir-splice-handler REFUSED' not in diagnostic, mode
            assert not any('ir-splice-callee-handler-covered-nested-body' in line
                           and 'in J27DecRows.wrap(I)J' in line for line in lines), mode
            assert 'nest-static-final jdk/internal/access/JavaLangAccess.uncheckedInflateBytesToChars' in diagnostic, mode
            decoder_classes = set(re.findall(
                r'nest-profiled java/nio/charset/CharsetDecoder\.decodeLoop.*?guard on class (\d+)',
                diagnostic))
            built_classes = set(re.findall(r'ir-nested-guarded-splice.*?class id (\d+)', diagnostic))
            assert decoder_classes & built_classes, (mode, decoder_classes, built_classes)
            oom = (out / f'diagnostic-{mode}-J27CoveredAllocationFailure.stderr').read_text()
            assert 'optimizing backend produced a body for J27CoveredAllocationFailure.fill(I)V' in oom, mode
            assert any('ir-splice-bytes J27CoveredAllocationFailure.make' in line
                       and 'in J27CoveredAllocationFailure.fill(I)V' in line
                       for line in oom.splitlines()), mode
        print('COVERED_NESTED_ENGAGEMENT_OK wrap=2 decoder=2 bootstrap-receiver=2 allocation-failure=2', flush=True)
    print(f'COVERED_NESTED_OK comparisons={comparisons} processes={len(records)}', flush=True)


if __name__ == '__main__':
    main()
