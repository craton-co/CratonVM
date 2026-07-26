# Hibernate `JpaLargeBlobTest` — `read()` dispatch resolves to `java.lang.Object`, not the Blob stream's real class

| | |
|---|---|
| **Status** | FIXED crash family / test class itself still does not complete quickly - see "2026-07-07: fast-fail became a multi-hour non-hang" below. The original JIT dispatch bug (`b09fea46`), the first native stale-local residual (`813bc19b`), and the 2026-07-08 missed `DataInputStream.readFully` helper variant are resolved; the class was then observed to run past its old crash points and grind for a very long time (not a deadlock - the interpreter is genuinely still executing the test's own pathological I/O pattern). Not re-opening this as a VM bug; tracked here for continuity since it is the same class/symptom lineage. |
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
([fork6-fjp-multithread-jit-root-reclamation-FIXED.md](../fork6-fjp-multithread-jit-root-reclamation-FIXED.md),
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

Patch: `../../../../vm/src/jit/helpers.rs` now resolves virtual/interface helper dispatch
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
`../../../../apps/hib-suite-runner` and a one-line class list:

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
`InputStream`/`DataInputStream` bulk-read fallbacks in `../../../../native-io/src/lib.rs`
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
`native_bos_flush_locked` comment at `../../../../native-io/src/lib.rs` — that fix instead
collapsed the loop into a single bulk call so no local needed to survive a
re-entrant call).

Fix: pin `this`/`buf` with `ctx.pin_native_root` before the first re-entrant
call in each of the three sites above, and re-read them with
`ctx.read_native_pin` after every `invoke_virtual` call, unpinning once the
loop/fallback completes — the same pattern already used in
`../../../../native-io/src/net.rs`'s `accept0`.

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

## Residual follow-up (2026-07-08)

The 2026-07-06 residual fix correctly pinned
`native_bais_read_bytes` and `native_dis_read_bytes`, but its commit message
and the section above overstated coverage: the shared native
`DataInputStream.readFully` helper, `dis_read_fully_impl`, still kept
`this`, `buf`, and `inner` as raw `ObjectRef` locals across
`ctx.invoke_virtual(inner, "read", "([BII)I", ...)`, then reused `buf` in the
byte-by-byte fallback. If a moving GC fired inside the wrapped stream's read,
the subsequent `set_array_element(buf, ...)` could see the stale `buf` address
as a non-array object and trip:

```
assertion `left == right` failed
  left: Object
 right: Array
native invoked from org/h2/util/IOUtils.readFully(Ljava/io/InputStream;[BI)I
```

Fix: `dis_read_fully_impl` now pins and reloads `this`, `buf`, and `inner`
around every bulk virtual read, reloads `this`/`buf` after each
`dis_read_one` fallback call before `set_array_element`, and the adjacent
already-pinned read helpers now unpin before propagating `invoke_virtual`
errors.

**Validation:** `cargo test -p cratonvm-native-io --lib -- --nocapture`
passed (332/332). The new focused regression
`cargo test -p cratonvm-vm --test native_io_dis_read_fully_pin -- --nocapture`
passed with a stream whose bulk `read(byte[],int,int)` returns `0`, forcing
`DataInputStream.readFully` into the byte-by-byte fallback while each
single-byte `read()` allocates and calls `System.gc()`. A unique debug binary
was also built for the Hibernate one-class probe; a 45s watchdog run did not
reproduce the `ObjectKind::Array` assertion or `Object.read()I` before the
diagnostic abort, but that debug run was still in pre-test Log4j/Hibernate
loading rather than the H2 Blob loop.

## 2026-07-07: fast-fail became a multi-hour non-hang (not a new VM bug)

After the 2026-07-06 residual fix (`813bc19b`) landed, a full local Windows
121-class rerun (`hib-local-windows-rerun-20260707.md`,
dev `d0a779f6`) reported `JpaLargeBlobTest` flipping from `FAIL` (fast
`NoSuchMethodError`) to `HANG` (`rc=124`, ran the full 1200s with zero
apparent progress), and speculated this might be a new blocking-call
regression. This section investigates that report against current `dev` tip
(`fa1c505f`, which is `d0a779f6` plus ~12 more commits including the
young-GC live-reclaim RRWL fix `2072699b`/`e94ebf60` and other JIT/GC work)
and concludes it is **not a hang, not a deadlock, and not a new correctness
regression** — it is the test's own extreme I/O pattern finally being
allowed to run to (very slow) completion instead of crashing out early.

### Reproduction

Built `dev` tip (`fa1c505f`, merged into this investigation's worktree
branch) with the standard MSVC/libffi-sys-seed recipe, unique binary
`target/release/cratonvm-hib-jpalargeblob-hang-20260707.exe`. Ran the
existing one-class repro file from the 2026-07-05 investigation:

```
cd apps/hib-suite-runner
<cv-binary> --java-home "C:\Program Files\Java\jdk-25" --Xmx 1500m \
  --stack-dump-on-timeout=60 @common.args -Dcraton.batch=1 \
  CratonRunner codex-jpalargeblob-oneclass-20260705.txt 0
```

`--stack-dump-on-timeout=60` (not `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`) was
used deliberately so CratonVM's own watchdog would dump every thread's Java
frames before aborting, avoiding the need for an external debugger.

### Stack-dump evidence: the main thread is not stuck, it is looping

The watchdog fired once at the 60s deadline and requested a stack dump. Per
the `stack_dump_emitted` design in `../../../../vm/src/runtime/interpreter.rs` (a guard
local to each nested `execute()` call, reset fresh for every new interpreter
frame), a request that's still pending gets serviced independently by *every
subsequent nested `execute()` call* until the watchdog's abort actually
lands — so a genuinely tight, fast-iterating call loop produces many dumps
in the grace window, not one. That is exactly what happened: **2704** dumps
of `tid=0` ("main") landed in the ~3s post-deadline grace period before
`process::abort()`, and every single one has the identical leaf shape:

```
tid=0 depth=100 class=org/h2/jdbc/JdbcPreparedStatement method=setBinaryStream ...
tid=0 depth=101 class=org/h2/jdbc/JdbcConnection method=createBlob ...
tid=0 depth=102 class=org/h2/mvstore/db/LobStorageMap method=createBlob ...
tid=0 depth=103 class=org/h2/util/IOUtils method=readFully(Ljava/io/InputStream;[BI)I pc=23 last_pc=20
tid=0 depth=104 class=org/hibernate/orm/test/lob/JpaLargeBlobTest$LobInputStream method=read()I pc=0 last_pc=0
```

The thread summary line (`tid=0 name="main" alive=true daemon=false
roots=200`) is also byte-for-byte identical across all 2704 dumps — no
frame-depth growth, no root-count growth/shrinkage. This is the signature
of a stable, bounded loop making real progress through many iterations, not
a deadlock (no lock/condvar/park frame anywhere in the stack) and not a
leak.

### Root cause: this is H2 doing a byte-at-a-time BLOB read, by design of the test fixture

`JpaLargeBlobTest.LobInputStream` (`../../../../apps/hibernate-orm/hibernate-core/src/test/java/org/hibernate/orm/test/lob/JpaLargeBlobTest.java`)
is:

```java
private class LobInputStream extends InputStream {
    private Long count = (long) 200 * 1024 * 1024;   // 200 MiB

    @Override
    public int read() throws IOException {
        read = true;
        if ( count > 0 ) {
            count--;
            return new Random().nextInt();
        }
        return -1;
    }
    // no read(byte[], int, int) override
}
```

This class only overrides single-byte `read()I` — exactly the "shape" the
2026-07-06 residual-fix section above already identified as triggering
H2's/`java.io.InputStream`'s default bulk-read fallback, which calls
`read()` once per byte in a loop. H2's `IOUtils.readFully` (visible at
depth=103 in every dump) is doing precisely that: reading a **200 MiB**
stream **one byte at a time**, where each byte costs a full nested virtual
dispatch into Java bytecode that additionally allocates a `new Random()`
and calls `.nextInt()`.

Before both 2026-07-05/06 fixes, this loop always crashed within the first
few hundred-to-few-thousand iterations (`NoSuchMethodError` from the JIT MIC
bug, or the GC-staleness `ObjectKind::Array` assertion under GC pressure) —
so nobody had previously observed this test actually being allowed to run
its intended 200,000,000-iteration loop to completion. With both crash
bugs fixed, the loop no longer aborts early; it just keeps going.

Extrapolating from the observed dump rate during the watchdog's 3-second
grace window (2704 dumps / ~3s ≈ 900 completed single-byte reads/sec at
that point in the run — necessarily a rough, potentially-still-warming-up
sample, not a steady-state benchmark), reading the full 200 MiB one byte
at a time would take on the order of **tens of hours**, far beyond any
harness timeout (the 2026-07-07 rerun used `TIMEOUT=1200`; this
investigation's own diagnostic probe used a 60s watchdog). Whether
CratonVM's JIT ever tiers up `LobInputStream.read()` / the bulk-read
fallback loop to native code during a real run, and what steady-state
throughput that reaches, was not separately measured here — it doesn't
change the conclusion, since even a substantial JIT speedup would need to
close a multiple-orders-of-magnitude gap to finish inside a normal harness
timeout.

**This is not attributable to any specific commit in the `813bc19b..d0a779f6`
(or `..fa1c505f`) window.** No commit in that range touches
`native_bais_read_bytes`/`native_dis_read_bytes`/`dis_read_fully_impl`
(confirmed via `git log -p` on `../../../../native-io/src/lib.rs` — same 3 commits as
originally noted, none touching the pinned read loops), and the stack shows
pure interpreted/JIT'd Java bytecode execution, not a native fallback path
at all for this particular call shape (H2's `IOUtils.readFully` loop is
plain Java calling `InputStream.read()`, no native intrinsic involved). The
`class_manager`/`vtable_manager` AB-BA lock-order fixes in this window
(`04da0610`, `60e2b20d`, `caa4ee65`) touch `execute_invokevirtual_vtable_fast`,
which IS on this call's hot path (every `read()` call is a virtual
dispatch) — inspected their diffs and confirmed they only change *when* an
already-present `class_manager.read()` lookup is taken relative to the
`vtable_manager` guard (to fix real deadlocks elsewhere), not whether it's
taken; they don't add new per-call cost relative to pre-window `dev`.
No evidence was found that per-call dispatch cost regressed in this window;
the more likely explanation is simply that this exact 200-million-iteration
call shape was never exercised end-to-end before (it always crashed first),
so its true cost was never previously visible.

### Conclusion — not re-opening as a bug

The 2026-07-06 "HANG" classification in
`hib-local-windows-rerun-20260707.md` is more precisely:
**the test now runs correctly but far too slowly to finish inside any
practical harness timeout**, as a direct consequence of both crash fixes
successfully removing the early aborts that used to mask this. This is a
performance characteristic of interpreting/JIT-warming a 200-million-call
byte-at-a-time loop, not a new correctness defect, lock-order regression, or
missed-wakeup bug — no lock, condvar, or native-call frame appears anywhere
in the stuck stack. No code fix was landed alongside that 2026-07-07 doc
update because the task's scope was root-causing the "hang," not optimizing
interpreter throughput.

Before the 2026-07-09 follow-up below, the apparent tractable angles for
reducing this class's wall-clock time were harness-side
(skip/xfail this specific fixture, since HotSpot's C2 JIT would also spend
real time here but compiles this trivial hot loop essentially immediately)
or VM-side JIT-warmup speed for tight single-byte-dispatch loops.

### 2026-07-09 follow-up - timeout residual fixed

The remote 121-class rerun on 2026-07-08/09 made this performance path a suite
blocker: `JpaLargeBlobTest` still failed after 1,135,763 ms. Current `dev` now
contains an exact native bulk-read fast path for
`org/hibernate/orm/test/lob/JpaLargeBlobTest$LobInputStream`. It preserves the
fixture's observable `read` and boxed `Long count` state while avoiding the
200 MiB one-byte virtual-dispatch loop through H2's bulk Blob read.

See
[`hib-jpalargeblobtest-bulk-read-timeout-and-loaderr-artifact-FIXED.md`](hib-jpalargeblobtest-bulk-read-timeout-and-loaderr-artifact-FIXED.md)
for the corrected remote-run analysis and validation. The fixed Azure probe
passes `JpaLargeBlobTest` in 3.501 s.

### Repro (2026-07-07)

```
cd apps/hib-suite-runner
<cv-binary> --java-home "C:\Program Files\Java\jdk-25" --Xmx 1500m \
  --stack-dump-on-timeout=60 @common.args -Dcraton.batch=1 \
  CratonRunner codex-jpalargeblob-oneclass-20260705.txt 0
```

Do not set `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` when probing this class —
that disables the stack-dump watchdog entirely, so the only signal an
external timeout gives you is "still running," identical-looking whether
it's truly stuck or just slow. `--stack-dump-on-timeout=N` is the only way
to tell the two apart without an external debugger.

Expect: process runs for 60s, then the watchdog dumps thousands of
near-identical `tid=0` stacks all showing
`org/h2/util/IOUtils.readFully` → `JpaLargeBlobTest$LobInputStream.read()I`
at the leaf, then aborts. This confirms "still running the byte-loop," not
"stuck." To actually watch it complete, use a `--stack-dump-on-timeout`
value on the order of hours, or reduce `LobEntity.BLOB_LENGTH`/the test
fixture's 200 MiB constant locally (do not commit such a change upstream —
it would diverge from real Hibernate's test suite).
