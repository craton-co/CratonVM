#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Validate the actual cross-compiled Windows publisher with Windows JDK clients.
Run on Linux with CVM, JAVA_HOME, WINDOWS_JAVA_HOME, WINDOWS_PERF_PROBE and
WINDOWS_RUNNER (e.g. 'wine64') set. First argument is an evidence directory.
The fixture is tools/probes/interp/L1/I59WindowsPerfPublication.rs; compile it
with rustc --target x86_64-pc-windows-gnu and a MinGW linker. This tests the
publisher against real Windows jvmstat, not a complete Windows VM launch.
"""
import os, pathlib, pwd, select, shlex, subprocess, sys, time
root = pathlib.Path(__file__).resolve().parents[2]
out = pathlib.Path(sys.argv[1]).resolve(); out.mkdir(parents=True, exist_ok=True)
classes = out / 'classes'; classes.mkdir(exist_ok=True)
subprocess.run([os.environ['JAVA_HOME']+'/bin/javac', '-d', str(classes),
    str(root/'tools/probes/interp/L1/I59Serviceability.java')], check=True)
runner = shlex.split(os.environ['WINDOWS_RUNNER'])
jdk = pathlib.Path(os.environ['WINDOWS_JAVA_HOME']) / 'bin'
def win(path): return 'Z:' + str(path).replace('/', '\\')
# Capture the VM's actual bytes, already verified by the Linux jvmstat gate.
with (out/'capture.err').open('w') as err:
    p = subprocess.Popen([os.environ['CVM'], '--jdk-only', '-XX:+DisableAttachMechanism',
        '-cp', str(classes), 'I59Serviceability'], stdout=subprocess.PIPE, stderr=err, text=True)
    try:
        assert select.select([p.stdout], [], [], 30)[0] and p.stdout.readline().strip() == 'ready'
        data = (pathlib.Path('/tmp')/('hsperfdata_'+pwd.getpwuid(os.geteuid()).pw_name)/str(p.pid)).read_bytes()
        assert len(data) == 32768
        (out/'disabled.bin').write_bytes(data)
        at = data.index(b'01'+b'0'*62)
        (out/'enabled.bin').write_bytes(data[:at]+b'1'+data[at+1:])
    finally:
        if p.poll() is None: p.terminate()
        p.wait(timeout=10)
for disabled in [True, False]:
    label = 'disabled' if disabled else 'enabled'
    with (out/(label+'.err')).open('w') as err:
        p = subprocess.Popen(runner+[os.environ['WINDOWS_PERF_PROBE'], win(out/(label+'.bin'))],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
        try:
            ready = p.stdout.readline().strip()
            assert ready.startswith('READY '), ready
            _, pid, path = ready.split(' ', 2)
            def client(exe, *args):
                flags = ['-J-XX:ActiveProcessorCount=2','-J-Xms16m','-J-Xmx128m'] if exe=='jps' else ['-XX:ActiveProcessorCount=2','-Xms16m','-Xmx128m']
                result = subprocess.run(runner+[str(jdk/(exe+'.exe')), *flags, *args], capture_output=True, text=True, timeout=45)
                (out/(label+'-'+exe+'.out')).write_text(result.stdout)
                (out/(label+'-'+exe+'.err')).write_text(result.stderr)
                assert result.returncode == 0, (exe, result.returncode, result.stderr)
                return result.stdout.strip()
            rows = client('jps', '-l').splitlines()
            assert any(row.startswith(pid+' ') and 'I59Serviceability' in row for row in rows), rows
            listed = client('java', '-cp', win(classes), 'I59Serviceability', 'list', pid)
            assert bool(listed) != disabled, (label, listed)
            if disabled:
                before = time.monotonic()
                answer = client('java', '-cp', win(classes), 'I59Serviceability', 'attach', pid)
                assert answer == 'com.sun.tools.attach.AttachNotSupportedException: The VM does not support the attach mechanism', answer
                print('PASS windows disabled attach %.3fs' % (time.monotonic()-before), flush=True)
            remaining = p.communicate(input='done\n', timeout=10)[0]
            # The fixture asserts exclusivity and verifies deletion itself.
            assert p.returncode == 0 and 'REMOVED' in remaining, (p.returncode, remaining)
            print('PASS windows', label, 'discovery, duplicate lease, teardown', flush=True)
        finally:
            if p.poll() is None: p.kill(); p.wait()
# Delete-on-close also survives forced termination (no Rust Drop runs).
with (out/'forced.err').open('w') as err:
    p = subprocess.Popen(runner+[os.environ['WINDOWS_PERF_PROBE'], win(out/'enabled.bin')],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
    try:
        ready = p.stdout.readline().strip()
        assert ready.startswith('READY '), ready
        _, pid, path = ready.split(' ', 2)
        killed = subprocess.run(runner+[str(jdk/'java.exe'),'-XX:ActiveProcessorCount=2',
            '-cp',win(classes),'I59Serviceability','kill',pid],capture_output=True,text=True,timeout=45)
        assert killed.returncode == 0 and killed.stdout.strip() == 'true', killed
        p.wait(timeout=10)
        removed = subprocess.run(runner+[os.environ['WINDOWS_PERF_PROBE'],'absent',path],
            capture_output=True,text=True,timeout=15)
        assert removed.returncode == 0 and removed.stdout.strip() == 'ABSENT', removed
        print('PASS windows forced termination cleanup',flush=True)
    finally:
        if p.poll() is None: p.kill(); p.wait()
print('WINDOWS_PERF_GATE_OK', flush=True)
