#!/usr/bin/env python3
"""Interleaved A/B of a probe's `time <row> <ns>` lines, one fresh process per arm per round.

  ab-rows.py --vm cvm --classes dir --probe J27DecRows --rounds 5 \
      --arm base= --arm sized=CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1,CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1 \
      [--flags --compatible] [--hotspot java]

An arm is NAME=ENV=V,ENV=V (empty env list = the shipped defaults). Arms rotate their start
position every round so a co-tenant burst does not always hit the same arm. The non-`time`
lines (the probe's facts) must be identical in every arm and in the HotSpot control, or the
run reports FACTS-DIFFER. Medians are printed in ns/op per row.
"""
import argparse
import os
import statistics
import subprocess


def run(cmd, env):
    r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=900)
    lines = r.stdout.splitlines()
    rows = {}
    for l in lines:
        if l.startswith('time '):
            _, name, v = l.split()[:3]
            rows[name] = float(v)
    return r.returncode, [l for l in lines if not l.startswith('time ')], rows


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--vm', required=True)
    p.add_argument('--classes', required=True)
    p.add_argument('--probe', default='J27DecRows')
    p.add_argument('--rounds', type=int, default=5)
    p.add_argument('--arm', action='append', required=True)
    p.add_argument('--flags', nargs='*', default=[])
    p.add_argument('--args', nargs='*', default=[])
    p.add_argument('--hotspot')
    a = p.parse_args()
    base_env = {k: v for k, v in os.environ.items() if not k.startswith('CRATONVM')}
    arms = []
    for s in a.arm:
        name, _, envs = s.partition('=')
        arms.append((name, dict(kv.split('=', 1) for kv in envs.split(',') if kv)))
    facts_ref, samples = None, {n: {} for n, _ in arms}
    if a.hotspot:
        rc, facts_ref, rows = run([a.hotspot, '-Dj27.timing=true', '-cp', a.classes, a.probe, *a.args], base_env)
        samples['hotspot'] = {k: [v] for k, v in rows.items()}
    bad = []
    for r in range(a.rounds):
        for n, env in arms[r % len(arms):] + arms[:r % len(arms)]:
            rc, facts, rows = run([a.vm, *a.flags, '-Dj27.timing=true', '-cp', a.classes, a.probe, *a.args],
                                  {**base_env, **env})
            if rc or (facts_ref is not None and facts != facts_ref):
                bad.append(f'{n} round {r}: rc={rc} FACTS-DIFFER' if rc == 0 else f'{n} round {r}: rc={rc}')
            facts_ref = facts_ref if facts_ref is not None else facts
            for k, v in rows.items():
                samples[n].setdefault(k, []).append(v)
    names = list(samples)
    keys = sorted({k for s in samples.values() for k in s})
    print('row'.ljust(14) + ''.join(n.rjust(12) for n in names))
    for k in keys:
        print(k.ljust(14) + ''.join(
            (f'{statistics.median(samples[n][k]):.1f}' if samples[n].get(k) else '-').rjust(12) for n in names))
    print('BAD' if bad else 'facts identical in every run', *bad, sep='\n' if bad else ' ')


main()
