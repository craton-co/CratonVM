# Hibernate `JpaLargeBlobTest` — `read()` dispatch resolves to `java.lang.Object`, not the Blob stream's real class

| | |
|---|---|
| **Status** | 🟠 PATCHED LOCALLY / OPEN RESIDUAL — root cause identified; local Hibernate runner probe no longer reproduces `java/lang/Object.read()I`, but the class still fails later with a no-JIT-independent GC array-kind assertion. |
| **Area** | VM — virtual/interface method dispatch for `InputStream.read()` on a JDBC `Blob`'s binary stream |
| **Symptom** | `java.lang.NoSuchMethodError: java/lang/Object.read()I` |
| **Severity** | medium — single class, but the failure mode (dispatch landing on `Object`'s non-existent method) suggests a general vtable/interface-dispatch defect that could recur elsewhere. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Symptom

`org.hibernate.orm.test.lob.JpaLargeBlobTest` builds a `LobEntity` with a
`byte[]`-backed `blob` field, persists it (insert succeeds — see the SQL log
below), then reads it back and calls into the JDBC `Blob`'s binary stream.
That call fails hard:

```
Hibernate:
    insert
    into
        LobEntity
        (blob, id)
    values
        (?, ?)
@@FAIL org.hibernate.orm.test.lob.JpaLargeBlobTest :: java.lang.NoSuchMethodError: java/lang/Object.read()I
```

`java.lang.Object` has no `read()` method at all — this is not "the wrong
overload was picked," it's the method resolver falling through the entire
class hierarchy and landing on `Object`'s (non-existent) vtable slot instead
of raising `AbstractMethodError`/finding the real implementation. HotSpot
passes this test cleanly (`found=1 ok=1 failed=0`, Azure HotSpot baseline),
confirming this is CratonVM-specific.

## What's ruled out

This is a different call site/object hierarchy from the `bytecode.enhance*`
package's `NoSuchMethodError: java/lang/Object.X` failures documented in
[hib-bytecode-enhancement-loader-faithful-linking.md](hib-bytecode-enhancement-loader-faithful-linking.md)
— that doc's root causes are all specific to ByteBuddy's per-package
`EnhancingClassLoader` and the loader-faithful supertype-linking gate.
`org.hibernate.orm.test.lob` uses no bytecode enhancement and no custom
classloader; the receiver here is whatever `InputStream` implementation H2's
JDBC driver returns from `Blob.getBinaryStream()` (or the object Hibernate's
BLOB-reading utility wraps it in). The shared symptom (`Object.<method>`
NoSuchMethodError as a generic "dispatch gave up" fallback) recurs across
several unrelated docs in this repo
([fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md),
[hib-temporal-gc-lambda-native-stale-local.md](hib-temporal-gc-lambda-native-stale-local.md)) —
worth keeping in mind if a common dispatch-fallback bug is ever found, but
each occurrence so far has had a distinct, unrelated root cause on inspection.

## Root cause / patch (2026-07-05)

Root cause is in the JIT virtual/interface MIC helper, not in H2 or Hibernate.
`jit_invoke_virtual_mic` derived its dispatch owner from the receiver header.
For a non-array receiver whose header reports `ClassId(0)`, the class store
maps id 0 to `java/lang/Object`; a non-Object call such as
`java/io/InputStream.read()I` could therefore be invoked as
`java/lang/Object.read()I`. The interpreter path already had a safer rule for
this case: `ClassId(0)` plus a non-Object method falls back to the CP-resolved
owner. The JIT MIC path did not mirror that rule.

There was a second cache-safety bug in the same path: MIC/PIC publication was
still possible for receiver class id 0. PIC slots use class id 0 as their empty
sentinel, so installing a resolved target under id 0 could turn an "empty" slot
into a callable inline-cache entry for other corrupted/synthetic receivers.

Patch: `vm/src/jit/helpers.rs` now resolves virtual/interface helper dispatch
through one shared target selector. `ClassId(0)` non-Object calls fall back to
the CP owner (`java/io/InputStream` for this call), true Object members still
dispatch through `java/lang/Object`, and CP-fallback / array / bare-object
targets are marked non-cacheable so MIC/PIC state is not poisoned.

## Local validation

Validated in isolated worktree
`C:\craton\CratonVM-hib-jpalargeblob-read-20260705-001`:

```
CARGO_TARGET_DIR=target\codex-hib-jpalargeblob-20260705-001 \
  cargo test -p cratonvm-vm --lib virtual_dispatch_target -- --nocapture

CARGO_TARGET_DIR=target\codex-hib-jpalargeblob-20260705-001 \
  cargo test -p cratonvm-vm --lib jit::helpers -- --nocapture

CARGO_TARGET_DIR=target\codex-hib-jpalargeblob-20260705-001 \
  cargo build -p cratonvm-cli --bin cratonvm
```

Unique binary built for external suite rerun:
`target\codex-hib-jpalargeblob-20260705-001\debug\cratonvm-hib-jpalargeblob-read-20260705-001.exe`.

Focused local runner probe on this box (2026-07-05), using
`apps/hib-suite-runner` and a one-line class list:

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  target\codex-hib-jpalargeblob-20260705-001\debug\cratonvm-hib-jpalargeblob-read-20260705-001.exe \
  --java-home "C:/Program Files/Java/jdk-25" --Xmx 1500m \
  @common.args CratonRunner codex-jpalargeblob-oneclass-20260705.txt 0
```

Result: the original `java/lang/Object.read()I` failure did not reproduce. The
run reached H2's BLOB read path and then failed later in native/GC code:

```
Native method panic caught: assertion `left == right` failed
  left: Object
 right: Array
(native invoked from org/h2/util/IOUtils.readFully(Ljava/io/InputStream;[BI)I)
```

The same one-class probe with `--nojit` failed with the same assertion, so the
remaining class failure is not the JIT virtual-dispatch bug fixed here.

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.lob.JpaLargeBlobTest) 0
```

## Remaining validation

- Re-run the exact Azure/Linux Hibernate repro with the unique binary above to
  confirm the local Windows result on the original host. Expected result for
  this specific bug: no `java/lang/Object.read()I`.
- Track the new residual separately if it persists on release/optimized builds:
  `GenHeap::set_array_element` receives an object where an array is required
  while H2 is executing `IOUtils.readFully(InputStream, byte[], int)`. Because
  the same assertion appears with `--nojit`, start from native/GC array-write
  handling rather than JIT interface dispatch.
