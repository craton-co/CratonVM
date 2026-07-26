# Hibernate UUidV6V7GeneratorTest SIGSEGV after longer timeout

## Status

Fixed on 2026-07-05. The hard crash was not the previously fixed
`jit-sigsegv-regression-20260704-FIXED.md`
`invokedynamic`/trap-stub regression. The reproducible fault was a GC
concurrent-mark candidate-filtering bug, with several follow-on hard-aborting
OOM allocation paths exposed once the SIGSEGV was removed.

The test class still times out functionally on CratonVM
(`testMonotonicityUuid7() timed out after 120 seconds`), but the VM no longer
SIGSEGVs or aborts in the verified default-JIT, `--nojit`, and
`CRATONVM_JIT_OSR=0` configurations. The timeout/slowness is tracked separately
as part of the Hibernate throughput wall cluster.

## Original symptom

`org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest` was previously
reported as a hang under a 600 second outer timeout. With the timeout raised to
1200 seconds on the Azure Linux real-JDK harness, the class ran long enough to
terminate with SIGSEGV (`rc=139`).

Harness shape:

```bash
cd /home/victor/hibpkg/runner
printf 'org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest\n' > /tmp/uuidcrash.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/uuidcrash.txt 0
```

## Root cause

The crash reproduced under `--nojit`, so it was not JIT-generated code
corruption. The release-with-debug core showed the fault in the concurrent old
generation marker:

```text
Program terminated with signal SIGSEGV
#0 atomic_load<u64>
#2 read_value_atomic() at types/src/value.rs:772
#3 scan_object() at gc/src/concurrent_mark.rs:777
#4 ConcurrentMarker::concurrent_mark() at gc/src/concurrent_mark.rs:481
```

`ConcurrentMarker` accepted root/SATB/outgoing-reference candidates by old-gen
range containment only. Under this test's allocation pressure, stale or interior
old-gen-looking words reached the marker. Some decoded as impossible object
headers such as `kind=Object` with `array_length=258`; another guard run showed
raw corrupt header tags. The marker then scanned fields from a non-object start
and faulted while loading a bogus `Value`.

Two details made the issue hard to see:

- The 600 second harness timeout often killed the class before the marker hit
  the bad candidate.
- After the marker was hardened, the same run progressed into separate hard
  aborts from infallible allocation helpers under real heap exhaustion.

Those follow-on aborts were:

```text
#13 alloc_ref_array() at native-collections/src/lib.rs:550
#14 native_tm_init() at native-collections/src/lib.rs:24350
```

and, after native object allocation was made fallible:

```text
Program terminated with signal SIGABRT
#7  alloc_java_string_object_from_units::{closure} at vm/src/vm/vm_object.rs:128
#9  alloc_java_string_object_from_units() at vm/src/vm/vm_object.rs:123
#11 create_java_string() at vm/src/vm/vm_object.rs:53
#12 prepare_class_shared() at vm/src/vm/vm_util.rs:1885
```

## Fix

The fix set does four things:

- `../../../gc/src/concurrent_mark.rs`: rejects corrupt object headers without formatting
  possibly invalid enum fields, and filters old-gen mark candidates to exact
  old-gen object starts instead of accepting any pointer inside old-gen.
- `../../../native-collections/src/lib.rs`: `TreeMap` backing-array construction now uses
  fallible native array allocation and returns a catchable `OutOfMemoryError`.
- `../../../vm/src/vm/vm_exec.rs`: `NativeContextImpl::new_object` and
  `new_object_initialized` now use fallible object allocation and return
  `OutOfMemoryError` instead of calling the fatal allocator.
- `../../../vm/src/vm/vm_object.rs` and `../../../vm/src/vm/vm_util.rs`: class-preparation
  `ConstantValue` string materialization now uses fallible Java string creation
  and propagates `OutOfMemoryError`.

## Verification

Local focused checks:

```powershell
cargo check -p cratonvm-vm
cargo test -p cratonvm-vm prepare_class_shared_applies_constant_values_for_all_supported_types
cargo test -p cratonvm-gc concurrent_mark
cargo test -p cratonvm-gc
```

Azure build:

```bash
cargo build --profile release-with-debug -p cratonvm-cli --bin cratonvm
cp target/release-with-debug/cratonvm ./cvm-uuidv6v7-conststr-oom-20260705
```

Final Azure matrix, worktree
`/data/wt/wt-uuidv6v7-native-array-oom-20260705`, binary
`cvm-uuidv6v7-conststr-oom-20260705`:

| Mode | Result |
| --- | --- |
| HotSpot JDK 25 | `rc=0`, `found=2 started=2 ok=2 failed=0`, `ms=2649` |
| CratonVM default JIT | `rc=0`, no core, `ok=1 failed=1`, residual JUnit timeout, `ms=233323` |
| CratonVM `--nojit` | `rc=0`, no core, `ok=1 failed=1`, residual JUnit timeout, `ms=237760` |
| CratonVM `CRATONVM_JIT_OSR=0` | `rc=0`, no core, `ok=1 failed=1`, residual JUnit timeout, `ms=239386` |

Final logs:

- `uuid-hotspot-conststr.log`
- `uuid-craton-jit-conststr.log`
- `uuid-craton-nojit-conststr.log`
- `uuid-craton-osr0-conststr.log`
- `uuid-conststr-matrix-conststr-20260705-135553.log`

## Residuals

The crash is fixed. The CratonVM runs still emit stale-local diagnostics and
`kind=Object`/`array_length=258` header warnings, and the UUID v7 monotonicity
method still hits JUnit's 120 second timeout. Those are non-crash residuals and
should be triaged separately from this resolved SIGSEGV/abort issue.
