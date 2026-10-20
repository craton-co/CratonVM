#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Compare complete hidden/reflect/plain frame records against a fresh JDK 25.
CVM=/unique/binary JAVA_HOME=/jdk25 python3 tools/scripts/i61-hidden-reflection-gate.py /evidence
Includes warmed linker customization and repeated explicit collections.
"""
import difflib, os, pathlib, subprocess, sys
root = pathlib.Path(__file__).resolve().parents[2]
out = pathlib.Path(sys.argv[1]).resolve(); out.mkdir(parents=True, exist_ok=True)
boot = out/'boot'; boot.mkdir(exist_ok=True)
classes = out/'classes'; classes.mkdir(exist_ok=True)
jdk = pathlib.Path(os.environ['JAVA_HOME'])/'bin'
subprocess.run([str(jdk/'javac'), '-d', str(classes),
    str(root/'tools/probes/interp/L3/I61HiddenReflectionFrames.java'),
    str(root/'tools/probes/interp/L3/I61LinkerLoader.java'),
    str(root/'tools/probes/interp/L3/I61LinkerPeerGc.java'),
    str(root/'tools/probes/interp/L4/L4W79BootSiteType.java'),
    str(root/'tools/probes/interp/L4/L4W81SiteTypeMissingClass.java'),
    str(root/'tools/probes/interp/L3/I61SoftCacheRetention.java'),
    str(root/'tools/probes/interp/L3/I61LambdaSerialization.java'),
    str(root/'tools/probes/interp/L3/i61-loader/Box.java'),
    str(root/'tools/probes/interp/L3/i61-loader/Caller.java')], check=True)
(classes/'L4W79BootSiteTypeBootSide.class').replace(boot/'L4W79BootSiteTypeBootSide.class')
def run(command, label, probe="I61HiddenReflectionFrames"):
    result = subprocess.run(command+['-cp',str(classes),probe], capture_output=True, timeout=90)
    (out/(label+'.out')).write_bytes(result.stdout)
    (out/(label+'.err')).write_bytes(result.stderr)
    assert result.returncode == 0, (label,result.returncode,result.stderr.decode(errors='replace'))
    return result.stdout
oracle = run([str(jdk/'java')], 'hotspot')
assert len(oracle.splitlines()) == 23
loader_oracle = run([str(jdk/'java')], 'hotspot-loader', 'I61LinkerLoader')
assert len(loader_oracle.splitlines()) == 2
soft_oracle = run([str(jdk/'java')], 'hotspot-soft', 'I61SoftCacheRetention')
lambda_oracle = run([str(jdk/'java')], 'hotspot-lambda', 'I61LambdaSerialization')
peer_oracle = run([str(jdk/'java')], 'hotspot-peer', 'I61LinkerPeerGc')
boot_oracle = run([str(jdk/'java'), '-Xbootclasspath/a:'+str(boot)], 'hotspot-boot', 'L4W79BootSiteType')
missing_oracle = run([str(jdk/'java')], 'hotspot-missing', 'L4W81SiteTypeMissingClass')
for mode in ['--jdk-only','--compatible']:
    for tier in ['jit','int']:
        for collector in ['ZGC','G1GC','GenerationalGC']:
            label = mode[2:]+'-'+tier+'-'+collector
            answer = run([os.environ['CVM'],mode,'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label)
            if oracle != answer:
                diff=''.join(difflib.unified_diff(oracle.decode().splitlines(True),answer.decode(errors='replace').splitlines(True)))
                (out/(label+'.diff')).write_text(diff)
                raise AssertionError(label+' differs from HotSpot\n'+diff)
            loader = run([os.environ['CVM'],mode,'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label+'-loader','I61LinkerLoader')
            assert loader == loader_oracle, (label, loader.decode(), loader_oracle.decode())
            soft = run([os.environ['CVM'],mode,'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label+'-soft','I61SoftCacheRetention')
            assert soft == soft_oracle, (label, soft.decode(), soft_oracle.decode())
            serialized = run([os.environ['CVM'],mode,'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label+'-lambda','I61LambdaSerialization')
            assert serialized == lambda_oracle, (label, serialized.decode(), lambda_oracle.decode())
            peer = run([os.environ['CVM'],mode,'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label+'-peer','I61LinkerPeerGc')
            assert peer == peer_oracle, (label, peer.decode(), peer_oracle.decode())
            site_boot = run([os.environ['CVM'],mode,'-Xbootclasspath/a:'+str(boot),'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label+'-boot','L4W79BootSiteType')
            assert site_boot == boot_oracle, (label,site_boot.decode(),boot_oracle.decode())
            missing = run([os.environ['CVM'],mode,'-XX:+Use'+collector]+(['--nojit'] if tier=='int' else []),label+'-missing','L4W81SiteTypeMissingClass')
            assert missing == missing_oracle, (label,missing.decode(),missing_oracle.decode())
            print('PASS',label,'23 frame records, loader casts, soft caches and serialization',flush=True)
print('I61_HIDDEN_REFLECTION_GATE_OK',flush=True)
