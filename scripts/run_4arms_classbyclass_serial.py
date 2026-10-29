import subprocess
import time
import os
import sys

JDK_HOME = '/data/toolchain/jdk-25'
CV_BIN = '/data/cvm/target/release/cratonvm'
RUNNER_DIR = '/data/cvm/apps/hibernate-reactive-suite-runner'
COMMON_ARGS = os.path.join(RUNNER_DIR, 'common.args')
TESTLIST_PATH = os.path.join(RUNNER_DIR, 'testlist-nohang.txt')
OUT_DIR = os.path.join(RUNNER_DIR, 'results_4arms_classbyclass_serial')
TIMEOUT_SEC = 180

os.makedirs(OUT_DIR, exist_ok=True)

with open(TESTLIST_PATH, 'r') as f:
    classes = [line.strip() for line in f if line.strip() and not line.startswith('#')]

print('Loaded ' + str(len(classes)) + ' classes from ' + TESTLIST_PATH, flush=True)
print('Using binary: ' + CV_BIN, flush=True)

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
print('Loaded ' + str(len(overrides)) + ' class overrides.', flush=True)

arms = [
    {
        'name': 'default-default',
        'desc': 'Arm 1: Default VM + Default C2 (Stock compat, Adaptive C1+C2)',
        'base_flags': ['--java-home', JDK_HOME, '-Xmx', '1500m'],
        'env': {}
    },
    {
        'name': 'default-c1',
        'desc': 'Arm 2: Default VM + C1-Only (Stock compat, Pinned C1)',
        'base_flags': ['--java-home', JDK_HOME, '-Xmx', '1500m'],
        'env': {'CRATONVM_C2_SUPERSEDE': '0'}
    },
    {
        'name': 'jdkonly-default',
        'desc': 'Arm 3: --jdk-only + Default C2 (JDK-only, Adaptive C1+C2)',
        'base_flags': ['--jdk-only', '--java-home', JDK_HOME, '-Xmx', '1500m'],
        'env': {}
    },
    {
        'name': 'jdkonly-c1',
        'desc': 'Arm 4: --jdk-only + C1-Only (JDK-only, Pinned C1)',
        'base_flags': ['--jdk-only', '--java-home', JDK_HOME, '-Xmx', '1500m'],
        'env': {'CRATONVM_C2_SUPERSEDE': '0'}
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
    print('Starting ' + arm['desc'] + ' [' + str(arm_idx) + '/4]', flush=True)
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
                rf.write('=== [' + str(idx) + '/' + str(len(classes)) + '] ' + cls + ' (rc=' + str(proc.returncode) + ') ===\n' + out + '\n')
                
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
                rf.write('=== [' + str(idx) + '/' + str(len(classes)) + '] ' + cls + ' (TIMEOUT) ===\n')
            
            if status == 'PASS': pass_cnt += 1
            elif status == 'FAIL': fail_cnt += 1
            elif status == 'HANG': hang_cnt += 1
            elif status == 'CRASH': crash_cnt += 1
            elif status == 'NOTESTS': notests_cnt += 1
            
            tf.write(str(idx) + '\t' + cls + '\t' + status + '\t' + str(found) + '\t' + str(ok) + '\t' + str(failed) + '\t' + str(aborted) + '\t' + str(skipped) + '\t' + str(elapsed_ms) + '\t' + sig + '\n')
            tf.flush()
            rf.flush()
            
            if idx % 10 == 0 or idx == len(classes) or status != 'PASS':
                print('[' + arm_name + '] [' + str(idx) + '/' + str(len(classes)) + '] ' + cls + ' -> ' + status + ' (' + str(elapsed_ms) + 'ms)', flush=True)

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
    print('[' + arm_name + '] COMPLETED in ' + str(round(wall_s, 1)) + 's: PASS=' + str(pass_cnt) + ' FAIL=' + str(fail_cnt) + ' HANG=' + str(hang_cnt) + ' CRASH=' + str(crash_cnt) + ' NOTESTS=' + str(notests_cnt), flush=True)

summary_file = os.path.join(OUT_DIR, 'SUMMARY.txt')
with open(summary_file, 'w') as sf:
    sf.write('========================================================================================\n')
    sf.write('  HIBERNATE REACTIVE: 4 ARMS SERIAL CLASS-BY-CLASS BENCHMARK\n')
    sf.write('========================================================================================\n')
    sf.write('{:<18} {:<6} {:<6} {:<6} {:<6} {:<8} {:<6} {:<10} {:<12}\n'.format('Arm', 'PASS', 'FAIL', 'HANG', 'CRASH', 'NOTESTS', 'Total', 'Wall(s)', 'SumClassMs'))
    sf.write('-' * 88 + '\n')
    for arm in arms:
        m = arm_metrics.get(arm['name'], {})
        sf.write('{:<18} {:<6} {:<6} {:<6} {:<6} {:<8} {:<6} {:<10.1f} {:<12}\n'.format(
            arm['name'], m.get('pass', 0), m.get('fail', 0), m.get('hang', 0), m.get('crash', 0), m.get('notests', 0), m.get('total', 0), m.get('wall_s', 0.0), m.get('sum_class_ms', 0)
        ))

print('\n' + open(summary_file).read(), flush=True)