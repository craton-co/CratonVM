#!/bin/bash
set -e

echo 'Waiting for cargo build to finish...'
while pgrep -f 'rustc.*cratonvm' > /dev/null || pgrep -f 'cargo build --release' > /dev/null; do
    sleep 3
done

echo 'Cargo build finished. Checking binary timestamp...'
ls -l /data/cvm/target/release/cratonvm

echo 'Launching 4-arm serial class-by-class benchmark...'
python3 -u /data/cvm/apps/hibernate-reactive-suite-runner/run_4arms_classbyclass_serial.py > /data/cvm/apps/hibernate-reactive-suite-runner/run_4arms_serial.log 2>&1 &
RUN_PID=$!
echo 'Started benchmark with PID:' $RUN_PID
echo $RUN_PID > /data/cvm/apps/hibernate-reactive-suite-runner/run_4arms_serial.pid