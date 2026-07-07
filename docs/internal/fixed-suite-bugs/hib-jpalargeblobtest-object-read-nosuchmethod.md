# Hibernate `JpaLargeBlobTest` — `read()` dispatch resolves to `java.lang.Object`, not the Blob stream's real class

| | |
|---|---|
| **Status** | 🟢 FIXED — both the original JIT dispatch bug (merged `b09fea46`) and the residual GC-staleness bug (merged, see "Residual fix" below) are resolved. |
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
[hib-temporal-gc-lambda-native-stale-local.md](../hib-temporal-gc-lambda-native-stale-local.md)) —
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

## Residual fix (2026-07-06)

The `GenHeap::set_array_element` `left: Object, right: Array` residual noted
above was **not** a GC bug and **not** JIT-specific (it reproduced identically
with `--nojit`, which was the correct clue). Root cause: two native
`InputStream`/`DataInputStream` bulk-read fallbacks in `native-io/src/lib.rs`
held plain `ObjectRef` locals (`this`, `buf`) across a **re-entrant**
`ctx.invoke_virtual(...)` call and reused them afterward without re-pinning:

- `native_bais_read_bytes`'s non-`ByteArrayInputStream` fallback (used when
  the receiver overrides `read()I` but not `read([BII)I` — exactly the shape
  of H2's Blob binary-stream class) looped calling
  `ctx.invoke_virtual(this, "read", "()I", &[])` once per byte, then reused
  the pre-loop `this`/`buf` locals in `set_array_element` on every iteration.
- `native_dis_read_bytes` / `dis_read_fully_impl` (`DataInputStream`) did the
  same across their bulk-then-byte-by-byte-fallback delegation to the inner
  stream.

`invoke_virtual` runs real bytecode and can trigger an allocation and a moving
young-gen GC. If that GC fires mid-loop, the stale `ObjectRef` can end up
pointing at whatever now occupies its old address — a plain `java.lang.Object`
(explaining the `Object.read()I`-shaped `NoSuchMethodError` that recurred
under GC pressure, indistinguishable from the original JIT dispatch bug at
the symptom level) or a non-array object (explaining the
`set_array_element` `ObjectKind::Array` assertion). This is the same bug class
already fixed once in this file for `BufferedOutputStream.flush` (see the
`native_bos_flush_locked` comment at `native-io/src/lib.rs` — that fix instead
collapsed the loop into a single bulk call so no local needed to survive a
re-entrant call).

Fix: pin `this`/`buf` with `ctx.pin_native_root` before the first re-entrant
call in each of the three sites above, and re-read them with
`ctx.read_native_pin` after every `invoke_virtual` call, unpinning once the
loop/fallback completes — the same pattern already used in
`native-io/src/net.rs`'s `accept0`.

**Validation:** building the real Hibernate/H2 harness on the Azure host was
not attempted (the available `hib-suite-runner`/`hibernate-orm` checkout only
had the top-level Gradle scaffold, not the built module classes — a full
rebuild from source would need cloning the real Hibernate ORM tree). Instead,
validated with a minimal targeted repro (`BaisFallbackRepro.java`): a real
`InputStream` subclass that overrides only `read()I` (matching H2's Blob
stream shape), read via both direct bulk `read(byte[],int,int)` and
`DataInputStream.readFully(byte[])`, with garbage allocated on every
single-byte `read()` call to force young-gen churn during the native loop.
Against unmodified `dev`: failed deterministically (3/3 runs, both JIT and
`--nojit`) with `NoSuchMethodError: java/lang/Object.read()I`. Against the
fix: passed cleanly across 13 runs (JIT and `--nojit`, including 8/8 under a
tight `--Xmx 64m` heap to maximize GC frequency). `cratonvm-native-io` (330
tests) and `cratonvm-gc` (764 tests) unit suites pass unchanged; the
`cratonvm-vm` lock_order and a handful of `cratonvm-native-builtins` test
failures seen in this build are pre-existing `--release`-vs-debug-assertion
artifacts, confirmed identical on unmodified `dev`.

Fixed in worktree `/data/data/wt-hib-jpalargeblob-residual-20260706`, branch
`fix/hib-jpalargeblob-residual-20260706`, merged to `dev`.
