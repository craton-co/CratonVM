# Hibernate UUidV6V7GeneratorTest SIGSEGV after longer timeout

## Status

Open pending the original Azure class-level rerun and gdb backtrace. A candidate
fix is on `debug/uuid-v6v7-sigsegv` and covers the native fault mechanism found
locally.

## Symptom

`org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest` was previously
reported as a hang under a 600 second outer timeout. With the timeout raised to
1200 seconds on the Azure Linux real-JDK harness, the class runs long enough to
terminate with SIGSEGV (`rc=139`).

Original harness shape:

```bash
cd /home/victor/hibpkg/runner
printf 'org.hibernate.orm.test.id.uuid.rfc9562.UUidV6V7GeneratorTest\n' > /tmp/uuidcrash.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/uuidcrash.txt 0
```

## Root cause found locally

This does not match the previously fixed
`docs/internal/jit-sigsegv-regression-20260704-FIXED.md` deopt/trap-stub shape.
The found mechanism is a native GC-rooting bug in the Java atomic update
intrinsics:

- `AtomicReference.getAndUpdate` and `AtomicReference.updateAndGet` saved raw
  `ObjectRef` values for the receiver, operator, and previous value.
- They invoked Java `UnaryOperator.apply(previous)`.
- That callback can allocate and can trigger moving young GC.
- After the callback returned, the native loop reused the stale raw references
  in `compareAndSet`.

That pattern can CAS through stale object addresses after a moving collection.
The same callback-across-GC pattern existed in the `AtomicInteger`,
`AtomicLong`, and `AtomicReferenceFieldUpdater` update loops, so the candidate
fix covers those as well.

`UUidV6V7GeneratorTest` is a strong trigger because Hibernate's RFC 9562 v6/v7
monotonic generators use `AtomicReference.updateAndGet` in high-iteration UUID
generation loops. The update callback allocates replacement state and is run
often enough that the previous 600 second timeout could mask the crash by
killing the run first.

## Candidate fix

The native update loops now pin all object references that must survive the
Java callback, re-read the forwarded references after `UnaryOperator.apply`
returns, and only then perform the CAS. Pins are released on success and on
error paths.

Covered paths:

- `AtomicInteger.getAndUpdate`
- `AtomicInteger.updateAndGet`
- `AtomicLong.getAndUpdate`
- `AtomicLong.updateAndGet`
- `AtomicReference.getAndUpdate`
- `AtomicReference.updateAndGet`
- `AtomicReferenceFieldUpdater.getAndUpdate`
- `AtomicReferenceFieldUpdater.updateAndGet`

## Evidence

Focused native-builtins regression tests simulate a moving-GC remap during the
operator callback and assert that the update helper re-reads the pinned,
forwarded references before invoking CAS:

```powershell
cargo test -p cratonvm-native-builtins update_rereads_pins_after_operator_gc
cargo check -p cratonvm-native-builtins
```

The original Azure Linux host-level confirmation is still pending. The local
SSH configuration available during this investigation did not expose
`/home/victor/hibpkg`, `/home/victor/jdk25`, or `/opt/cratonvm`, so the
Hibernate harness and requested gdb command could not be run from this Windows
workspace.

## Required final confirmation

Run the original class on the Azure host with:

- current candidate binary, JIT on
- `--nojit`
- `CRATONVM_JIT_OSR=0`
- HotSpot/JDK 25 for comparison

If the candidate binary still crashes, capture:

```bash
gdb --batch \
  -ex run \
  -ex "bt full" \
  -ex "info registers" \
  -ex "thread apply all bt" \
  --args <cv> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/uuidcrash.txt 0
```
