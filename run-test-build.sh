#!/usr/bin/env bash
cd /c/craton/CratonVM-ffmseg-20260822
cargo test -p cratonvm-native-builtins --release --lib the_craton_segment_class_mirrors_the_interface > test-mirror.log 2>&1
echo "TEST-EXIT=$?" >> test-mirror.log
cargo build --release --bin cratonvm > build-fix4.log 2>&1
echo "BUILD-EXIT=$?" >> build-fix4.log
