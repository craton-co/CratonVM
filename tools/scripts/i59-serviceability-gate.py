#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Linux external JDK attach/discovery and termination parity; no switch opt-ins.
CVM=/unique/binary JAVA_HOME=/jdk25 python3 tools/scripts/i59-serviceability-gate.py /evidence
"""
import os, pathlib, pwd, select, signal, subprocess, sys, time
root = pathlib.Path(__file__).resolve().parents[2]
out = pathlib.Path(sys.argv[1]).resolve(); out.mkdir(parents=True, exist_ok=True)
classes = out / 'classes'; classes.mkdir(exist_ok=True)
jdk = pathlib.Path(os.environ['JAVA_HOME']) / 'bin'
subprocess.run([str(jdk/'javac'), '-d', str(classes), str(root/'tools/probes/interp/L1/I59Serviceability.java'),
    str(root/'tools/probes/interp/L1/L1W81PerfDataFile.java')], check=True)
perfdir = pathlib.Path('/tmp') / ('hsperfdata_' + pwd.getpwuid(os.geteuid()).pw_name)
expected = 'com.sun.tools.attach.AttachNotSupportedException: The VM does not support the attach mechanism'
def client(*args):
    return subprocess.check_output([str(jdk/'java'), '-cp', str(classes), 'I59Serviceability', *args], text=True, timeout=5).strip()
def line(p):
    deadline = time.monotonic() + 30
    data = bytearray()
    while True:
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([p.stdout], [], [], remaining)[0], 'target did not respond'
        byte = os.read(p.stdout.fileno(), 1)
        assert byte, 'target exited before responding'
        if byte == b'\n': return data.decode().strip()
        data.extend(byte)

# Refresh the original measured byte walker, including ordinary main return
# and System.exit teardown. Each parent creates two children of its own VM.
def byte_walk(command, label):
    answer = subprocess.run(command+['-cp',str(classes),'L1W81PerfDataFile'],capture_output=True,timeout=90)
    (out/(label+'-bytes.out')).write_bytes(answer.stdout)
    (out/(label+'-bytes.err')).write_bytes(answer.stderr)
    assert answer.returncode == 0, (label,answer.stderr.decode(errors='replace'))
    return answer.stdout
byte_oracle = byte_walk([str(jdk/'java')], 'hotspot')
for mode in ['--jdk-only','--compatible']:
    for tier in ['jit','int']:
        answer = byte_walk([os.environ['CVM'],mode]+(['--nojit'] if tier=='int' else []),mode[2:]+'-'+tier)
        assert answer == byte_oracle, (mode,tier,answer.decode(),byte_oracle.decode())
        print('PASS',mode,tier,'original byte walk and normal exit',flush=True)

for binary, mode in [(str(jdk/'java'), []), (os.environ['CVM'], ['--jdk-only']), (os.environ['CVM'], ['--compatible'])]:
    label = mode[0][2:] if mode else 'hotspot'
    for flags in [[], ['-XX:+DisableAttachMechanism'], ['-XX:-UsePerfData'], ['-XX:+PerfDisableSharedMem']]:
        name = label + '-' + (flags[0].replace(':','').replace('+','').replace('-','') if flags else 'default')
        err = (out/(name+'.err')).open('w')
        p = subprocess.Popen([binary, *mode, *flags, '-cp', str(classes), 'I59Serviceability'], preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL), stdout=subprocess.PIPE, stderr=err, text=True)
        f = perfdir / str(p.pid)
        try:
            assert line(p) == 'ready', name
            published = flags not in [['-XX:-UsePerfData'], ['-XX:+PerfDisableSharedMem']]
            assert f.exists() == published, (name, 'publication', f.exists())
            if published:
                listed = [s for s in subprocess.check_output([str(jdk/'jps'), '-l'], text=True, timeout=5).splitlines() if s.startswith(str(p.pid)+' ')]
                assert listed and 'I59Serviceability' in listed[0], (name, listed)
                disabled = '+DisableAttachMechanism' in ''.join(flags)
                assert bool(client('list', str(p.pid))) != disabled, (name, 'VirtualMachine.list')
                begin = time.monotonic(); answer = client('attach', str(p.pid)); elapsed = time.monotonic()-begin
                assert answer == (expected if disabled else 'attach ok'), (name, answer)
                assert elapsed < 3, (name, 'attach delay', elapsed)
            p.terminate(); output = p.communicate(timeout=10)[0]
            assert 'hook' in output and p.returncode == 143 and not f.exists(), (name, 'termination', p.returncode, output, f.exists())
            print('PASS', name, flush=True)
        finally:
            if p.poll() is None: p.kill(); p.wait()
            err.close()
    registration = subprocess.check_output([binary, *mode, '-Xrs', '-cp', str(classes), 'I59Serviceability', 'register'], text=True, timeout=30)
    (out/(label+'-xrs-register.out')).write_text(registration)
    if label == 'hotspot': registration_oracle = registration
    else: assert registration == registration_oracle, (label, registration, registration_oracle)
    print('PASS', label, '-Xrs signal registration', flush=True)
    # -Xrs leaves catchable OS termination to the inherited disposition;
    # HotSpot then skips hooks and leaves its perf file for the next sweep.
    with (out/(label+'-xrs.err')).open('w') as err:
        p = subprocess.Popen([binary, *mode, '-Xrs', '-cp', str(classes), 'I59Serviceability'],
            stdout=subprocess.PIPE, stderr=err, text=True)
        xrs_file = perfdir / str(p.pid)
        try:
            assert line(p) == 'ready' and xrs_file.exists()
            p.terminate(); output = p.communicate(timeout=10)[0]
            assert p.returncode == -signal.SIGTERM and 'hook' not in output and xrs_file.exists(), (label, '-Xrs', p.returncode, output)
            print('PASS', label, '-Xrs inherited termination', flush=True)
        finally:
            if p.poll() is None: p.kill(); p.wait()
    for sig in [signal.SIGHUP, signal.SIGINT]:
        with (out/(label+'-signal'+str(sig)+'.err')).open('w') as err:
            p = subprocess.Popen([binary, *mode, '-cp', str(classes), 'I59Serviceability'], preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL), stdout=subprocess.PIPE, stderr=err, text=True)
            f = perfdir / str(p.pid)
            try:
                assert line(p) == 'ready' and not xrs_file.exists(); p.send_signal(sig); output = p.communicate(timeout=10)[0]
                assert 'hook' in output and p.returncode == 128+sig and not f.exists(), (label, sig, output, p.returncode)
                print('PASS', label, 'signal', sig, flush=True)
            finally:
                if p.poll() is None: p.kill(); p.wait()
    with (out/(label+'-handler.err')).open('w') as err:
        p = subprocess.Popen([binary, *mode, '-cp', str(classes), 'I59Serviceability', 'handle'], preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL), stdout=subprocess.PIPE, stderr=err, text=True)
        f = perfdir / str(p.pid)
        try:
            assert line(p) == 'ready'; p.terminate(); assert line(p) == 'handled'
            for _ in range(3): p.terminate(); time.sleep(0.005)
            for _ in range(3): assert line(p) == 'handled'
            assert p.poll() is None and f.exists(), (label, 'custom handler')
            p.kill(); p.wait(); assert f.exists(), (label, 'SIGKILL')
            print('PASS', label, 'custom handler and SIGKILL', flush=True)
        finally:
            if p.poll() is None: p.kill(); p.wait()
    stale_disposition_files = []
    for disposition in ['ignore', 'default', 'restore']:
        with (out/(label+'-'+disposition+'.err')).open('w') as err:
            r = subprocess.Popen([binary, *mode, '-cp', str(classes), 'I59Serviceability', disposition],
                preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL),
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
            rf = perfdir / str(r.pid)
            try:
                assert line(r) == 'ready' and rf.exists(), (label, disposition)
                r.terminate()
                if disposition == 'default':
                    output = r.communicate(timeout=10)[0]
                    assert r.returncode == -signal.SIGTERM and 'hook' not in output and rf.exists(), (label, disposition, r.returncode, output)
                    stale_disposition_files.append(rf)
                else:
                    time.sleep(0.1)
                    assert r.poll() is None and rf.exists(), (label, disposition, 'SIG_IGN killed target')
                    if disposition == 'restore':
                        r.stdin.write('r'); r.stdin.flush(); assert line(r) == 'restored'
                        r.terminate(); time.sleep(0.1)
                        assert r.poll() is None, (label, disposition, 'HotSpot keeps ignored shutdown signals ignored')
                        r.send_signal(signal.SIGINT); output = r.communicate(timeout=10)[0]
                        assert r.returncode == 130 and 'hook' in output and not rf.exists(), (label, disposition, r.returncode, output)
                    else:
                        r.kill(); r.wait(); stale_disposition_files.append(rf)
                print('PASS', label, 'Java signal disposition', disposition, flush=True)
            finally:
                if r.poll() is None: r.kill(); r.wait()
    # The next launch sweeps that killed process's stale numeric publication.
    q = subprocess.Popen([binary, *mode, '-cp', str(classes), 'I59Serviceability'], preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    try:
        assert line(q) == 'ready'; assert not f.exists(), (label, 'stale file')
        assert not any(path.exists() for path in stale_disposition_files), (label, 'disposition stale files')
        q.terminate(); q.wait(timeout=10)
        print('PASS', label, 'stale file sweep', flush=True)
    finally:
        if q.poll() is None: q.kill(); q.wait()
print('I59_SERVICEABILITY_GATE_OK', flush=True)
