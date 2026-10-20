#!/usr/bin/env python3
"""Fresh-process soft LRU parity, including concurrent ZGC reference processing.

Retain complete output and commands. Concurrent rows must certify a mark with
VM soft rows, so a stop-the-world fallback cannot silently pass the gate.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from gc_process_measure import run_measured


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--cvm', type=Path, required=True)
    ap.add_argument('--jdk', type=Path, required=True)
    ap.add_argument('--out', type=Path, required=True)
    ap.add_argument('--repeat', type=int, default=2)
    ap.add_argument('--concurrent-only', action='store_true')
    ap.add_argument('--collectors', nargs='+', choices=['Generational', 'G1', 'ZGC'])
    ap.add_argument('--stress', action='store_true', help='allocation LRU, weak/cleaner, and multithreaded churn')
    ap.add_argument('--regressions', action='store_true', help='soft-only class loader and young-reference survival')
    ap.add_argument('--skip', choices=['unset', '0', '1'], default='unset')
    ap.add_argument('--soft-skip', choices=['unset', '0', '1'], default='unset')
    ap.add_argument('--lru', choices=['unset', '0', '1'], default='unset')
    ap.add_argument('--env', action='append', default=[], metavar='NAME=VALUE', help='diagnostic controls; recorded verbatim')
    ap.add_argument('--cases', nargs='+', help='probe modes (or weak) to run')
    ap.add_argument('--engines', nargs='+', default=['jit', 'nojit'], choices=['jit', 'nojit'])
    ap.add_argument('--allow-failures', action='store_true')
    args = ap.parse_args()
    if args.repeat < 1 or (args.stress and args.regressions):
        ap.error('use a positive repeat count and choose stress or regressions')
    args.out.mkdir(parents=True, exist_ok=True)
    source = Path(__file__).resolve().parent
    classes = args.out / 'classes'
    classes.mkdir(exist_ok=True)
    names = ['GchAk1RefProbe', 'Zr1RfSoftLruProbe', 'Zr1RfConcWeakProbe', 'MtChurnProbe',
             'GcReferenceQueuePausedPoll', 'SoftLruClockAgeProbe', 'SoftLruInitialClockProbe']
    if args.regressions:
        names += ['GchH4g1SoftLoaderProbe', 'GenR4SoftRefLruProbe', 'GenR5W1SoftRefYoungProbe']
    sources = {n: source / (n + '.java') for n in names}
    sources = {n: p if p.exists() else source.parent / 'bench' / p.name for n, p in sources.items()}
    ext = '.exe' if os.name == 'nt' else ''
    subprocess.run([str(args.jdk / 'bin' / ('javac' + ext)), '-d', str(classes),
                    *[str(p) for p in sources.values()]], check=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith('CRATONVM_')}
    env['JAVA_HOME'] = str(args.jdk)
    if args.skip != 'unset':
        env['CRATONVM_ZGC_CONC_REF_SKIP'] = args.skip
    if args.soft_skip != 'unset':
        env['CRATONVM_ZGC_CONC_REF_SKIP_SOFT'] = args.soft_skip
    if args.lru != 'unset':
        env['CRATONVM_SOFTREF_HOTSPOT_LRU'] = args.lru
    for setting in args.env:
        key, value = setting.split('=', 1)
        if not key.startswith('CRATONVM_'):
            ap.error('--env accepts CRATONVM_ controls only')
        env[key] = value
    records, failures = [], []
    cases = [('GchAk1RefProbe', p) for p in ['softlru', 'softidle', 'softcache']]
    cases += [('Zr1RfSoftLruProbe', p) for p in ['gc', 'clock']]
    cases += [('GcReferenceQueuePausedPoll', '')]
    cases += [('SoftLruClockAgeProbe', '')]
    cases += [('SoftLruClockAgeProbe', 'contended')]
    cases += [('SoftLruInitialClockProbe', ''), ('SoftLruInitialClockProbe', 'unsafe')]
    # Serial/G1 can collect only young garbage here: the old idle object is
    # then outside the collection. This allocation-only oracle is ZGC-specific.
    concurrent_cases = cases + [('Zr1RfSoftLruProbe', 'alloc'), ('Zr1RfConcWeakProbe', '')]
    if args.stress:
        args.concurrent_only = True
        concurrent_cases = [('Zr1RfSoftLruProbe', 'alloc'), ('Zr1RfConcWeakProbe', ''),
                            ('MtChurnProbe', '4 20 8')]
    if args.regressions:
        cases = [('GchH4g1SoftLoaderProbe', '6'), ('GenR4SoftRefLruProbe', ''),
                 ('GenR5W1SoftRefYoungProbe', '')]
        concurrent_cases = cases
    if args.cases:
        def case_name(case):
            name, param = case
            return param or {'Zr1RfConcWeakProbe': 'weak',
                             'GcReferenceQueuePausedPoll': 'queue',
                             'SoftLruClockAgeProbe': 'clock-age',
                             'SoftLruInitialClockProbe': 'initial-clock'}.get(name, name)
        cases = [c for c in cases if case_name(c) in args.cases]
        concurrent_cases = [c for c in concurrent_cases if case_name(c) in args.cases]
        if not concurrent_cases:
            ap.error('--cases selected no probes')
    common = ['-Xmx512m' if args.regressions else '-Xmx64m',
              '-XX:SoftRefLRUPolicyMSPerMB=1000' if args.regressions else '-XX:SoftRefLRUPolicyMSPerMB=20',
              '--add-opens=java.base/java.lang.ref=ALL-UNNAMED',
              '--add-opens=java.base/jdk.internal.misc=ALL-UNNAMED', '-cp', str(classes)]

    def run(binary, opts, probe, param, label, extra=None):
        command = [str(binary), *opts, *common, probe, *param.split()]
        row = dict(label=label, command=command,
                   env={k: v for k, v in dict(env, **(extra or {})).items() if k.startswith('CRATONVM_')})
        row.update(run_measured(command, dict(env, **(extra or {})), 180))
        records.append(row)
        (args.out / 'records.json').write_text(json.dumps(records, indent=2))
        print(label, row['rc'], repr(row['stdout']), flush=True)
        return row

    collectors = args.collectors or (['ZGC'] if args.concurrent_only else ['Generational', 'G1', 'ZGC'])
    if args.concurrent_only and collectors != ['ZGC']:
        ap.error('concurrent-only requires ZGC')
    for gc in collectors:
        oracle = '-XX:+Use' + {'Generational': 'SerialGC', 'G1': 'G1GC', 'ZGC': 'ZGC'}[gc]
        selector = '-XX:+Use' + {'Generational': 'GenerationalGC', 'G1': 'G1GC', 'ZGC': 'ZGC'}[gc]
        selected = concurrent_cases if gc == 'ZGC' else cases
        expected = {}
        for probe, param in selected:
            row = run(args.jdk / 'bin' / ('java' + ext), [oracle], probe, param,
                      f'hotspot/{gc}/{probe}/{param}')
            if row['rc'] or row.get('timeout') or ('false' in row['stdout'] and param != 'softcache') or (param == 'softcache' and 'softcache oome: false' not in row['stdout']):
                raise SystemExit('HotSpot oracle failed: ' + row['label'])
            expected[(probe, param)] = row['stdout']
        variants = ['concurrent'] if args.concurrent_only else ['stw', 'concurrent'] if gc == 'ZGC' else ['stw']
        for variant in variants:
            for policy in ['--jdk-only', '--compatible']:
                for engine in args.engines:
                    for probe, param in selected:
                        for n in range(args.repeat):
                            label = f'{gc}/{variant}/{policy}/{engine}/{probe}/{param}/{n}'
                            extra = dict(CRATONVM_ZGC_CONC_START='auto') if variant == 'concurrent' else {}
                            row = run(args.cvm, [policy, selector, '--verbose:gc'] + (['--nojit'] if engine == 'nojit' else []),
                                      probe, param, label, extra)
                            # Allocation and pressure rows must reach a certified concurrent mark.
                            coverage = True
                            if variant == 'concurrent':
                                if param in ['alloc', 'softcache']:
                                    coverage = bool(re.search(r'\[GC\] zgc-markend:.*soft_rows=[1-9]', row['stderr']))
                                    if param == 'alloc':
                                        coverage = coverage and bool(re.search(r'\[GC\] zgc-markend:.*soft_rows=[1-9].*soft_roots=[1-9]', row['stderr']))
                                elif probe in ['Zr1RfConcWeakProbe', 'MtChurnProbe']:
                                    coverage = '[GC] zgc-markend:' in row['stderr']
                            row['coverage'] = coverage
                            row['certified_marks'] = row['stderr'].count('[GC] zgc-markend:')
                            row['filter_refusals'] = row['stderr'].count('declining to certify this mark set')
                            fatal = any(s in row['stderr'] for s in ['Stale pointer', 'FATAL:', '[jit-helper-panic]', 'unknown panic', 'corrupt object'])
                            def stable(output):
                                return re.sub(r'wallMs=\d+', 'wallMs=<elapsed>', output) if probe == 'MtChurnProbe' else output
                            unexpected_fallback = variant == 'concurrent' and row['filter_refusals'] > 0
                            if row['rc'] or row.get('timeout') or stable(row['stdout']) != stable(expected[(probe, param)]) or not coverage or fatal or unexpected_fallback:
                                failures.append(label)
    manifest = dict(binary=str(args.cvm.resolve()),
                    binary_sha256=digest(args.cvm),
                    source_head=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                    source_diff_sha256=hashlib.sha256(subprocess.check_output(['git', 'diff', '--binary', 'HEAD'])).hexdigest(),
                    runner_sha256=digest(Path(__file__)),
                    sources={n: digest(p) for n, p in sources.items()})
    (args.out / 'manifest.json').write_text(json.dumps(manifest, indent=2))
    (args.out / 'records.json').write_text(json.dumps(records, indent=2))
    summary = dict(runs=len(records), failures=failures, accepted=not failures)
    (args.out / 'summary.json').write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary), flush=True)
    return int(bool(failures) and not args.allow_failures)


if __name__ == '__main__':
    raise SystemExit(main())
