import os
import sys
import time
import subprocess
import shutil

JDK_HOME = os.environ.get('JDK_HOME', '/data/toolchain/jdk-25')
CV_BIN = os.environ.get('CV_BIN', '/data/CVM/target/release/cratonvm')
RUNNER_DIR = '/data/CVM/apps/hibernate-reactive-suite-runner'
COMMON_ARGS = os.path.join(RUNNER_DIR, 'common-args.txt')
CLASSES_FILE = os.path.join(RUNNER_DIR, 'classes.list')
OUT_DIR = os.environ.get('OUT_DIR', os.path.join(RUNNER_DIR, 'results_4shards_' + str(int(time.time()))))

TIMEOUT_SEC = 120

if not os.path.exists(CV_BIN):
    print(f"Error: binary not found at {CV_BIN}", flush=True)
    sys.exit(1)

with open(CLASSES_FILE, 'r') as f:
    classes = [line.strip() for line in f if line.strip() and not line.startswith('#')]

print(f"Loaded {len(classes)} classes from {CLASSES_FILE}", flush=True)
print(f"Using binary: {CV_BIN}", flush=True)
print(f"Output directory: {OUT_DIR}", flush=True)

# Verify local-postgres is running
res = subprocess.run(['docker', 'inspect', '-f', '{{.State.Running}}', 'local-postgres'], capture_output=True, text=True)
if res.stdout.strip() != 'true':
    print('Starting local-postgres...', flush=True)
    subprocess.run(['docker', 'start', 'local-postgres'], check=True)
    time.sleep(2)

overrides = {}
overrides_path = os.path.join(RUNNER_DIR, 'class-overrides.tsv')
if os.path.exists(overrides_path):
    with open(overrides_path, 'r') as of:
        for line in of:
            line = line.strip()
            if not line or line.startswith('#'):
                continue
            parts = line.split('\t')
            if len(parts) >= 3:
                cls_n, to_s, fl_s = parts[0], parts[1], parts[2]
                overrides[cls_n] = {
                    'timeout': int(to_s) if to_s != '-' and to_s.isdigit() else TIMEOUT_SEC,
                    'flags': fl_s.split() if fl_s != '-' else []
                }
print(f"Loaded {len(overrides)} class overrides.", flush=True)

arms = [
    {
        'name': 'default',
        'desc': 'Arm 1: default (Default GC, normal execution)',
        'base_flags': ['--java-home', JDK_HOME, '-Xmx', '1500m'],
        'env': {}
    },
    {
        'name': 'g1',
        'desc': 'Arm 2: g1 (UseG1GC)',
        'base_flags': ['--java-home', JDK_HOME, '-Xmx', '1500m', '-XX:+UseG1GC'],
        'env': {}
    },
    {
        'name': 'gen-gc',
        'desc': 'Arm 3: gen gc (UseGenerationalGC / Generational GC)',
        'base_flags': ['--java-home', JDK_HOME, '-Xmx', '1500m', '-XX:+UseGenerationalGC'],
        'env': {}
    },
    {
        'name': 'nojit-zgc',
        'desc': 'Arm 4: no-jit + zgc (-Xint, UseZGC)',
        'base_flags': ['--java-home', JDK_HOME, '-Xmx', '1500m', '-Xint', '-XX:+UseZGC'],
        'env': {}
    }
]

arm_metrics = {}

for arm_idx, arm in enumerate(arms, 1):
    arm_name = arm['name']
    arm_dir = os.path.join(OUT_DIR, arm_name)
    os.makedirs(arm_dir, exist_ok=True)

    tsv_file = os.path.join(arm_dir, arm_name + '.tsv')
    raw_log = os.path.join(arm_dir, arm_name + '.raw.log')

    print('\n=======================================================', flush=True)
    print(f"Starting {arm['desc']} [{arm_idx}/4]", flush=True)
    print('=======================================================', flush=True)

    t0_arm = time.time()
    pass_cnt = 0
    fail_cnt = 0
    hang_cnt = 0
    crash_cnt = 0
    notests_cnt = 0
    sum_class_ms = 0

    with open(tsv_file, 'w') as tf, open(raw_log, 'w') as rf:
        tf.write('idx\tclass\tstatus\tfound\tok\tfailed\taborted\tskipped\tms\tsig\n')

        for idx, cls in enumerate(classes, 1):
            override_info = overrides.get(cls, {})
            timeout_val = override_info.get('timeout', TIMEOUT_SEC)
            extra_flags = override_info.get('flags', [])

            cmd = [CV_BIN] + arm['base_flags'] + extra_flags + ['@' + COMMON_ARGS, 'CratonRunner', cls]
            env = os.environ.copy()
            env['CRATONVM_DISABLE_DEFAULT_WATCHDOG'] = '1'
            env.update(arm['env'])

            t0 = time.time()
            try:
                proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, cwd=RUNNER_DIR, timeout=timeout_val, text=True, errors='replace')
                elapsed_ms = int((time.time() - t0) * 1000)
                sum_class_ms += elapsed_ms
                out = proc.stdout
                rf.write(f"=== [{idx}/{len(classes)}] {cls} (rc={proc.returncode}) ===\n{out}\n")

                status = 'PASS'
                found = ok = failed = aborted = skipped = 0
                sig = ''

                for line in out.splitlines():
                    if line.startswith('@@RESULT '):
                        parts = line.strip().split()
                        for p in parts[2:]:
                            if '=' in p:
                                k, v = p.split('=', 1)
                                if v.isdigit():
                                    if k == 'found': found = int(v)
                                    elif k == 'ok': ok = int(v)
                                    elif k == 'failed': failed = int(v)
                                    elif k == 'aborted': aborted = int(v)
                                    elif k == 'skipped': skipped = int(v)
                    elif line.startswith('@@TESTFAIL '):
                        if not sig: sig = line.strip()

                if proc.returncode != 0 and status == 'PASS' and failed == 0 and found == 0:
                    status = 'CRASH'
                    sig = 'exit code ' + str(proc.returncode)
                elif failed > 0:
                    status = 'FAIL'
                elif found == 0:
                    status = 'NOTESTS'
                else:
                    status = 'PASS'

            except subprocess.TimeoutExpired as e:
                elapsed_ms = int((time.time() - t0) * 1000)
                sum_class_ms += elapsed_ms
                status = 'HANG'
                found = ok = failed = aborted = skipped = 0
                sig = 'timeout after ' + str(timeout_val) + 's'
                rf.write(f"=== [{idx}/{len(classes)}] {cls} (TIMEOUT) ===\n")

            if status == 'PASS': pass_cnt += 1
            elif status == 'FAIL': fail_cnt += 1
            elif status == 'HANG': hang_cnt += 1
            elif status == 'CRASH': crash_cnt += 1
            elif status == 'NOTESTS': notests_cnt += 1

            tf.write(f"{idx}\t{cls}\t{status}\t{found}\t{ok}\t{failed}\t{aborted}\t{skipped}\t{elapsed_ms}\t{sig}\n")
            tf.flush()
            rf.flush()

            if idx % 10 == 0 or idx == len(classes) or status != 'PASS':
                print(f"[{arm_name}] [{idx}/{len(classes)}] {cls} -> {status} ({elapsed_ms}ms)", flush=True)

    t1_arm = time.time()
    wall_s = t1_arm - t0_arm
    arm_metrics[arm_name] = {
        'pass': pass_cnt,
        'fail': fail_cnt,
        'hang': hang_cnt,
        'crash': crash_cnt,
        'notests': notests_cnt,
        'total': len(classes),
        'wall_s': wall_s,
        'sum_class_ms': sum_class_ms
    }
    print(f"[{arm_name}] COMPLETED in {wall_s:.1f}s: PASS={pass_cnt} FAIL={fail_cnt} HANG={hang_cnt} CRASH={crash_cnt} NOTESTS={notests_cnt}", flush=True)

summary_file = os.path.join(OUT_DIR, 'SUMMARY.txt')
with open(summary_file, 'w') as sf:
    sf.write('========================================================================================\n')
    sf.write('  HIBERNATE REACTIVE: 4 SERIAL SHARDS BENCHMARK SUMMARY\n')
    sf.write('========================================================================================\n')
    sf.write('{:<18} {:<6} {:<6} {:<6} {:<6} {:<8} {:<6} {:<10} {:<12}\n'.format('Arm', 'PASS', 'FAIL', 'HANG', 'CRASH', 'NOTESTS', 'Total', 'Wall(s)', 'SumClassMs'))
    sf.write('-' * 88 + '\n')
    for arm in arms:
        m = arm_metrics.get(arm['name'], {})
        sf.write('{:<18} {:<6} {:<6} {:<6} {:<6} {:<8} {:<6} {:<10.1f} {:<12}\n'.format(
            arm['name'], m.get('pass', 0), m.get('fail', 0), m.get('hang', 0), m.get('crash', 0), m.get('notests', 0), m.get('total', 0), m.get('wall_s', 0.0), m.get('sum_class_ms', 0)
        ))

print('\n' + open(summary_file).read(), flush=True)
