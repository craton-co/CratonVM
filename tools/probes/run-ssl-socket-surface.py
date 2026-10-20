#!/usr/bin/env python3
"""Compare SSL socket/SNI probes in fresh processes against the selected HotSpot JDK."""
import argparse, difflib, os, pathlib, subprocess, sys
p = argparse.ArgumentParser()
p.add_argument('--cvm', required=True)
p.add_argument('--java-home', required=True)
p.add_argument('--keystore', required=True)
p.add_argument('--out', required=True)
p.add_argument('--repeats', type=int, default=1)
p.add_argument('--nojit', action='store_true')
p.add_argument('--probe', action='append', choices=['Nio21SocketImpl', 'Nio21SessionImpl', 'SslSocketSurface', 'SslCarrierResiduals'])
a = p.parse_args()
out = pathlib.Path(a.out).resolve(); out.mkdir(parents=True, exist_ok=True)
classes = out / 'classes'; classes.mkdir(exist_ok=True)
source = pathlib.Path(__file__).resolve().parent
probes = a.probe or ['Nio21SocketImpl', 'Nio21SessionImpl', 'SslSocketSurface', 'SslCarrierResiduals']
subprocess.run([str(pathlib.Path(a.java_home) / 'bin/javac'), '-d', str(classes)] + [str(source / (s + '.java')) for s in probes], check=True)
env = dict(os.environ, JAVA_HOME=a.java_home, RUST_LOG='error')
refs = {}
failures = 0
def run(label, cmd):
    (out / (label + '.command')).write_text(' '.join(cmd) + '\n')
    try:
        r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180, env=env)
    except subprocess.TimeoutExpired as e:
        (out / (label + '.stderr')).write_bytes(e.stderr or b'')
        (out / (label + '.stdout')).write_bytes(e.stdout or b'')
        raise RuntimeError(label + ' timed out')
    (out / (label + '.stdout')).write_bytes(r.stdout)
    (out / (label + '.stderr')).write_bytes(r.stderr)
    (out / (label + '.rc')).write_text(str(r.returncode) + '\n')
    if r.returncode != 0:
        raise RuntimeError(label + ' failed with ' + str(r.returncode))
    return r.stdout.decode().splitlines(keepends=True)
for probe in probes:
    tail = ['-Dnio19.ks=' + a.keystore, '-cp', str(classes), probe]
    refs[probe] = run('hotspot-' + probe, [str(pathlib.Path(a.java_home) / 'bin/java')] + tail)
    if not refs[probe] or not refs[probe][-1].strip() in ('done', 'SSL_SOCKET_SURFACE_OK'):
        raise RuntimeError(probe + ' reference did not finish')
    for rep in range(a.repeats):
        for mode in ('compatible', 'jdk-only'):
            label = mode + '-' + probe + '-' + str(rep)
            try:
                got = run(label, [a.cvm, '--java-home', a.java_home, '--' + mode] + (['--nojit'] if a.nojit else []) + tail)
            except RuntimeError as e:
                print(str(e), flush=True); failures += 1; continue
            diff = ''.join(difflib.unified_diff(refs[probe], got, fromfile='HotSpot', tofile=label))
            (out / (label + '.diff')).write_text(diff)
            print(label + (' DIFF' if diff else ' PASS'), flush=True)
            if diff:
                print(diff)
                failures += 1
if failures:
    raise SystemExit('SSL_SURFACE_GATE_FAILED failures=' + str(failures))
print('SSL_SURFACE_GATE_OK runs=' + str(2 * len(probes) * a.repeats), flush=True)
