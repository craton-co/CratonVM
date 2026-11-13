#!/usr/bin/env python3
"""Fresh-process parity and interleaved pricing for the separate CPU variants."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('vm', type=Path)
p.add_argument('--work-dir', required=True, type=Path)
p.add_argument('--baseline-vm', type=Path)
p.add_argument('--cpu', default='2')
p.add_argument('--rounds', type=int, default=3)
p.add_argument('--entries', type=int, default=1_000_000)
a = p.parse_args()
if a.entries < 0 or a.rounds < 1:
    p.error('entries must be nonnegative and rounds positive')
if os.getloadavg()[0] > 1.0:
    p.error('host load exceeds 1.0; wait for builds and unrelated work to finish')
a.vm = a.vm.resolve()
a.work_dir.mkdir(parents=True, exist_ok=False)
repo = Path(__file__).resolve().parents[3]
classes = a.work_dir / 'classes'
classes.mkdir()
sources = [repo/'tools/bench'/n for n in ('CratonBench.java','HashMapOnly.java','HashMapSimd.java')]
sources += list(Path(__file__).parent.glob('*.java'))
subprocess.run(['javac','--add-modules','jdk.incubator.vector','-d',str(classes),*map(str,sources)],check=True)
base = ['--add-modules','jdk.incubator.vector','-Xmx2g','-XX:+UseG1GC','-cp',str(classes)]
runtimes = {'hotspot':['java'], 'jdk-only':[str(a.vm),'--real-jdk'], 'compatible':[str(a.vm),'--compatible']}
if a.baseline_vm:
    a.baseline_vm = a.baseline_vm.resolve()
    runtimes.update({'baseline-jdk-only':[str(a.baseline_vm),'--real-jdk'], 'baseline-compatible':[str(a.baseline_vm),'--compatible']})
rows = []
report = {'source':subprocess.check_output(['git','-C',str(repo),'rev-parse','HEAD'],text=True).strip(),
          'binary_sha256':hashlib.sha256(a.vm.read_bytes()).hexdigest(),
          'baseline_binary_sha256':hashlib.sha256(a.baseline_vm.read_bytes()).hexdigest() if a.baseline_vm else None,
          'java_version':subprocess.run(['java','-version'],capture_output=True,text=True,check=True).stderr.strip(), 'java_source_sha256':{str(s.relative_to(repo)):hashlib.sha256(s.read_bytes()).hexdigest() for s in sources},
          'dirty':bool(subprocess.check_output(['git','-C',str(repo),'status','--porcelain'],text=True)),
          'cpu':a.cpu, 'entries':a.entries, 'rounds':a.rounds, 'cpu_info':Path('/proc/cpuinfo').read_text().split('\n\n')[0], 'rows':rows}

def rust_build_active():
    return subprocess.run(['pgrep','-x','rustc'],stdout=subprocess.DEVNULL).returncode == 0

def run(runtime, name, args, round, expected_output=None, expected_checksum=None):
    attempt = 0
    while True:
        if rust_build_active() or attempt:
            print(name,runtime,'waiting for quiet host',flush=True)
            while rust_build_active() or os.getloadavg()[0] > 1.0:
                time.sleep(5)
        prefix = a.work_dir / f'{name}.{runtime}.{round}.{attempt}'
        started = time.monotonic()
        result = subprocess.run(['taskset','-c',a.cpu,*runtimes[runtime],*base,*args],capture_output=True,text=True,timeout=600)
        prefix.with_suffix(prefix.suffix+'.out').write_text(result.stdout)
        prefix.with_suffix(prefix.suffix+'.err').write_text(result.stderr)
        row = {'runtime':runtime,'case':name,'round':round,'attempt':attempt,'rc':result.returncode,
               'seconds':time.monotonic()-started,'load':os.getloadavg()[0]}
        bits = re.search(r'flat-map group bits: (\d+)',result.stderr)
        if bits: row['group_bits'] = int(bits.group(1))
        rows.append(row)
        (a.work_dir/'results.json').write_text(json.dumps(report,indent=2))
        if result.returncode or 'panicked' in result.stderr:
            raise RuntimeError(f'{name} {runtime}: exit={result.returncode}; see {prefix}')
        if expected_output is not None:
            assert result.stdout == expected_output, (name,runtime,'stdout mismatch')
        if expected_checksum is not None:
            checksums = re.findall(r'\[(-?\d+)\]',result.stdout)
            assert checksums and int(checksums[-1]) == expected_checksum, (name,runtime,result.stdout)
            row['checksum'] = int(checksums[-1])
            row['kernel_ms'] = int(re.search(r': (\d+) ms',result.stdout).group(1))
        row['accepted'] = not rust_build_active()
        (a.work_dir/'results.json').write_text(json.dumps(report,indent=2))
        if row['accepted']:
            print(name,runtime,round,'OK',flush=True)
            return result.stdout
        print(name,runtime,round,'compiler overlap: sample retained, retrying',flush=True)
        attempt += 1

for probe in ('MatrixTilingProbe','HashMapSimdProbe','VectorEqualityProbe'):
    reference = None
    for runtime in runtimes:
        output = run(runtime,probe,[probe],0,expected_output=reference)
        if reference is None: reference = output

n = str(a.entries)
cases = {'matrix':['CratonBench','matrix'], 'matrix-tiled':['CratonBench','matrix-tiled'],
         'hashmap':['HashMapOnly',n], 'hashmap-flat-scalar':['HashMapSimd',n,'scalar'],
         'hashmap-flat-simd':['HashMapSimd',n,'simd']}
# Java int overflow applies before each value is added to the long checksum.
expected_map = sum(((i*31+7+(1<<31)) % (1<<32))-(1<<31) for i in range(a.entries))
for round in range(a.rounds):
    for name,args in cases.items():
        for runtime in list(runtimes)[round%len(runtimes):]+list(runtimes)[:round%len(runtimes)]:
            run(runtime,name,args,round+1,expected_checksum=(173943680 if name.startswith('matrix') else expected_map))

for name in cases:
    for runtime in runtimes:
        print(name,runtime,'median_ms',statistics.median(r['kernel_ms'] for r in rows if r['case']==name and r['runtime']==runtime and r.get('accepted')))
report['status'] = 'passed'
(a.work_dir/'results.json').write_text(json.dumps(report,indent=2))
print('MATRIX_HASHMAP_GATE_OK')
