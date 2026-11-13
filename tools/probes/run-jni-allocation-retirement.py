#!/usr/bin/env python3
"""Fresh-process JNI allocation parity against HotSpot 25.

Compile the three Java/C fixtures first (C build commands are in their headers).
Use task-specific output and library paths; no shared binary is discovered.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--vm')
    p.add_argument('--java', default='java')
    p.add_argument('--classes', required=True)
    p.add_argument('--libraries', required=True)
    p.add_argument('--out', required=True)
    p.add_argument('--oracles-only', action='store_true')
    p.add_argument('--quick', action='store_true')
    a = p.parse_args()
    out = Path(a.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    libraries = json.loads(Path(a.libraries).read_text())
    source_root = Path(__file__).resolve().parents[2]
    def source_fingerprint():
        paths = subprocess.check_output(['git', 'ls-files', '*.rs', '*.toml', 'Cargo.lock', '*.c', '*.h'],
                                        cwd=source_root).decode().splitlines()
        digest = hashlib.sha256()
        for name in sorted(paths):
            digest.update(name.encode() + b'\0')
            digest.update((source_root / name).read_bytes().replace(b'\r\n', b'\n'))
        return digest.hexdigest()
    frozen_source = source_fingerprint()
    source_commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source_root).decode().strip()
    rows = []
    env = {k: v for k, v in os.environ.items() if not k.startswith('CRATONVM_')}

    def run(name, exe, gc, cls, lib, args=(), mode=None, switches=None, nojit=False, expected_nonzero=False, extra=()):
        cmd = [str(exe)]
        if mode:
            cmd += [mode]
        cmd += [f'-XX:+Use{gc}GC', '-Xms32m', '-Xmx64m',
                '--enable-native-access=ALL-UNNAMED', '-cp', a.classes]
        if nojit:
            cmd += ['--nojit']
        cmd += [*extra, cls, libraries[lib], *map(str, args)]
        current_env = env | (switches or {})
        try:
            r = subprocess.run(cmd, env=current_env, capture_output=True, timeout=180)
            stdout, stderr, rc = r.stdout, r.stderr, r.returncode
        except subprocess.TimeoutExpired as e:
            stdout, stderr, rc = e.stdout or b'', e.stderr or b'', 'timeout'
        (out / (name + '.stdout')).write_bytes(stdout)
        (out / (name + '.stderr')).write_bytes(stderr)
        record = dict(name=name, cmd=cmd, switches=switches or {}, rc=rc,
                      stdout=stdout.decode(errors='replace').replace('\r\n', '\n'))
        rows.append(record)
        (out / 'records.json').write_text(json.dumps(rows, indent=2))
        print(f'{name}: rc={rc} {record["stdout"].strip()[:160]}', flush=True)
        if (rc != 0) != expected_nonzero:
            raise AssertionError(f'{name} failed: {stderr.decode(errors="replace")[-2000:]}')
        return record['stdout']

    oracles = {}
    string_cases = [(0, -1), (0, 0), (3, -1), (3, 0), (0, 16), (1, 16),
                    (0, (1 << 30) - 2), (0, (1 << 30) - 1),
                    (0, 1 << 30), (0, (1 << 31) - 1),
                    (1, (1 << 31) - 3), (1, (1 << 31) - 2),
                    (2, (1 << 30) - 2), (2, (1 << 30) - 1), (2, 1 << 30)]
    if a.quick:
        string_cases = [(0, 1 << 30)]
    for gc in ['Serial', 'G1', 'Z']:
        oracles[(gc, 'array')] = run(f'hotspot-{gc}-array', a.java, gc,
            'Gcj1JnArrayLimitProbe', 'array')
        for case in string_cases:
            key = 'string-%s-%s' % case
            oracles[(gc, key)] = run(f'hotspot-{gc}-{key}', a.java, gc,
                'GcjJniStringLimitProbe', 'string', case)
        for shape in ['drop', 'ring', 'frames']:
            oracles[(gc, shape)] = run(f'hotspot-{gc}-{shape}', a.java, gc,
                'Gcj1JnForeignAllocProbe', 'foreign', [shape, 1024, 256])
    (out / 'oracles.json').write_text(json.dumps({f'{k[0]}-{k[1]}': v for k, v in oracles.items()}, indent=2))
    if a.oracles_only:
        return
    assert a.vm, '--vm is required'
    for gc, oracle_gc in [('Generational', 'Serial'), ('G1', 'G1'), ('Z', 'Z')]:
        for mode in ['--real-jdk', '--compatible']:
            for nojit in ([False] if a.quick else [False, True]):
                prefix = f'{gc}-{mode[2:]}-jit{int(not nojit)}'
                actual = run(prefix + '-array', a.vm, gc, 'Gcj1JnArrayLimitProbe', 'array', mode=mode, nojit=nojit)
                assert actual == oracles[(oracle_gc, 'array')], prefix + ' array DIFF'
                for case in string_cases:
                    key = 'string-%s-%s' % case
                    actual = run(prefix + '-' + key, a.vm, gc, 'GcjJniStringLimitProbe', 'string', case, mode, nojit=nojit)
                    assert actual == oracles[(oracle_gc, key)], prefix + ' ' + key + ' DIFF'
                handles = [('indirect', {}), ('raw', {'CRATONVM_JNI_INDIRECT_LOCALS': '0'}),
                           ('no-foreign-transitions', {'CRATONVM_JNI_FOREIGN_TRANSITIONS': '0'}),
                           ('no-native-transitions', {'CRATONVM_JNI_NATIVE_TRANSITIONS': '0'})]
                for handle, switches in handles:
                    for shape in ['drop', 'ring', 'frames']:
                        switches = switches | {'CRATONVM_GC_STATS': '1'}
                        wait = 'poll' if handle in ['raw', 'no-native-transitions'] else 'native'
                        actual = run(prefix + '-' + handle + '-' + shape, a.vm, gc,
                            'Gcj1JnForeignAllocProbe', 'foreign', [shape, 1024, 256, wait], mode,
                            switches, nojit)
                        assert actual == oracles[(oracle_gc, shape)], prefix + ' foreign DIFF'
        # Raw handles must survive collections under allocation stress too.
        actual = run(gc + '-raw-stress', a.vm, gc, 'Gcj1JnForeignAllocProbe', 'foreign',
                     ['frames', 1024, 256, 'poll'], '--real-jdk',
                     {'CRATONVM_JNI_INDIRECT_LOCALS': '0', 'CRATONVM_DBG_GC_STRESS': '65536', 'CRATONVM_GC_STATS': '1'})
        assert actual == oracles[(oracle_gc, 'frames')]
        stats = (out / (gc + '-raw-stress.stderr')).read_text(errors='replace')
        assert re.search(r'jni_enter=[1-9][0-9]*', stats), gc + ' raw ladder did not engage'
    # Disable the preallocated-message pool: error construction must still
    # perform no collection for a VM-limit refusal, even with raw handles.
    for gc, oracle_gc in [('Generational', 'Serial'), ('G1', 'G1'), ('Z', 'Z')]:
        actual = run(gc + '-array-unpooled-raw', a.vm, gc, 'Gcj1JnArrayLimitProbe', 'array',
                     mode='--real-jdk', switches={'CRATONVM_GC_PREALLOCATED_OOME_KINDS': '0',
                                                  'CRATONVM_JNI_INDIRECT_LOCALS': '0'})
        assert actual == oracles[(oracle_gc, 'array')]
    actual = run('Generational-raw-compaction', a.vm, 'Generational',
                 'Gcj1JnForeignAllocProbe', 'foreign', ['frames', 2048, 256, 'poll'],
                 '--real-jdk', {'CRATONVM_JNI_INDIRECT_LOCALS': '0',
                 'CRATONVM_OLDGEN_COMPACT': '1', 'CRATONVM_GC_MOVING_MAJOR_JIT_GUARD': '0',
                 'CRATONVM_GC_STATS': '1', 'CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS': '0',
                 'CRATONVM_GC_FULL_GC_TRUE_ROOTS': '0'}, nojit=True)
    assert actual == 'foreign-alloc frames: PASS arrays=2048 bad=0\n'
    stats = (out / 'Generational-raw-compaction.stderr').read_text(errors='replace')
    assert re.search(r'oldsz_moving_major_vetoes=[1-9][0-9]*', stats), 'raw compaction veto did not engage'
    assert re.search(r'jni_enter=[1-9][0-9]*', stats), 'raw compaction ladder did not engage'
    # VM-limit OOMEs still go through the configured fatal OOME action.
    for gc, oracle_gc in [('Generational', 'Serial'), ('G1', 'G1'), ('Z', 'Z')]:
        for exe, selector, mode, label in [(a.java, oracle_gc, None, 'hotspot'),
                                            (a.vm, gc, '--real-jdk', 'candidate')]:
            run(f'{gc}-{label}-exit-oome', exe, selector, 'GcjJniStringLimitProbe', 'string',
                [0, (1 << 30) - 1], mode, expected_nonzero=True,
                extra=['-XX:+ExitOnOutOfMemoryError'])
            assert rows[-1]['rc'] == 3, rows[-1]
    for length in [-1, 0]:
        actual = run('string-edge-kill-' + str(length), a.vm, 'Generational',
                     'GcjJniStringLimitProbe', 'string', [3, length], '--real-jdk',
                     {'CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES': '0'})
        assert actual == f'string kind=3 len={length}: null gcs=0\n'
    # The diagnostic kills must expose the historical defects (positive controls).
    # A ring retained in raw locals forces the foreign requester to do the work.
    kill = run('foreign-kill', a.vm, 'Generational', 'Gcj1JnForeignAllocProbe', 'foreign',
               ['drop', 1024, 256], '--real-jdk', {'CRATONVM_JNI_FOREIGN_ALLOC_LADDER': '0'}, expected_nonzero=True)
    assert 'FAIL oome-at=' in kill, 'foreign kill did not discriminate'
    kill = run('array-kill', a.vm, 'Generational', 'Gcj1JnArrayLimitProbe', 'array',
               mode='--real-jdk', switches={'CRATONVM_JNI_ARRAY_VM_LIMIT': '0'})
    assert 'byte[MAX]: java.lang.OutOfMemoryError: Java heap space gcs=>0' in kill
    assert source_fingerprint() == frozen_source, 'production source changed during the gate'
    manifest = dict(runs=len(rows), source_commit=source_commit, source_sha256=frozen_source, vm_sha256=hashlib.sha256(Path(a.vm).read_bytes()).hexdigest())
    (out / 'PASS.json').write_text(json.dumps(manifest, indent=2))
    print('JNI_ALLOCATION_RETIREMENT_OK', manifest, flush=True)


if __name__ == '__main__':
    main()
