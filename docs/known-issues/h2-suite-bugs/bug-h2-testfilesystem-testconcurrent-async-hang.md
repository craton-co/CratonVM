# `TestFileSystem.testConcurrent` hangs against the `async:` filesystem — three real bugs fixed, one open performance gap remains

## Status
**PARTIALLY FIXED 2026-07-23.** Three independently-confirmed bugs found
during this investigation are fixed and merged to `dev` (`4afa20cea`). The
original doc's headline hypothesis (a `CompilationPolicy`-mutex/`LinkedHashMap`
eviction-callback deadlock) did **not** reproduce this session — a fresh,
from-scratch repro on the same day's `dev` tip showed a different shape
entirely (see "What actually reproduced" below). The class still exceeds the
suite runner's 300s per-class watchdog even with all three fixes applied, but
this is now confirmed to be **severe slowness, not a deadlock** — see "Open:
remaining performance gap" for the precise next step.

## Three bugs fixed this session (merged to `dev` `4afa20cea`)

### 1. `callee_saved_gpr_local_homes_enabled()` read an env var on every single interpreted call VM-wide, uncached
`vm/src/jit/skip_list.rs` — called from `should_skip_jit_internal`, which
runs on every interpreted method invocation in the whole VM. Unlike its four
sibling env-var checks in the same file (`CRATONVM_JIT_BISECT_SKIP/ONLY`,
`CRATONVM_JIT_ALLOW_PACKAGES`), which all cache their result in a `OnceLock`,
this one called `std::env::var()` fresh every time. Under a two-real-thread,
JIT-heavy, high-invocation-count workload (exactly `testConcurrent`'s shape)
this alone produced an apparent hang: live `gdb` attaches during the "hang"
caught a thread stuck inside `std::env::var` → libc `getenv`, actively
burning CPU with no forward progress visible in the test's own output for
minutes. Fixed by caching the decision in a `OnceLock<bool>`, matching the
established sibling pattern.

**Standalone repro/verification:** a minimal two-thread
`ReentrantReadWriteLock` Java program (no H2 involved) reproduced the same
symptom shape; after the fix the thread no longer sampled inside
`std::env::var` in repeated `gdb` snapshots.

### 2. `is_known_miscompile_aqs_family` was missing `compareAndSetState`/`getState`/`setState`
`vm/src/jit/skip_list.rs` — this unconditional (non-gated) skip-list already
covers `acquire`/`release`/`Node` CAS helpers/`ConditionObject` methods for
both `AbstractQueuedSynchronizer` (classic, `int state`) and
`AbstractQueuedLongSynchronizer` (JDK 25+, `long state` — confirmed via
`javap` that `ReentrantReadWriteLock$Sync extends AbstractQueuedLongSynchronizer`
on this JDK), with an existing doc comment citing "a heavy-contention repro
reproduces a PERMANENT hang for both `ReentrantReadWriteLock`... confirmed:
two threads parked forever... in `AbstractQueuedLongSynchronizer.acquire`."
Despite that, the three raw Unsafe-CAS/volatile-accessor wrapper methods
around `state` itself — `compareAndSetState`, `getState`, `setState` — were
absent from the list, even though they are the single hottest, most
contended methods of the entire synchronizer protocol (every acquire/release
funnels through them). Added all three for both classes, matching the
established pattern exactly.

### 3. `invoke_or_native`'s synthetic-stub check hashed the same (class, method, descriptor) triple twice
`native-api/src/registry.rs` / `vm/src/vm/vm_exec.rs` — `invoke_or_native`
called `native_methods.find(class, method, descriptor)` to get a callback,
then immediately called `native_methods.kind_of(class, method, descriptor)`
with the identical three strings to classify it — a second independent
128-bit hash (a full byte-walk of all three strings, twice) on **every**
native dispatch VM-wide, not just AQS-related ones. A `gdb`-sampling profile
(5 rapid snapshots, ~2s apart) of a live, actively-running (not deadlocked)
`testConcurrent` process caught both real threads inside
`hash_byte_pair`/`native_method_hash`/`kind_of` disproportionately often
relative to the sample count. Added
`NativeMethodRegistry::find_with_kind()`, which computes the hash once and
returns both the callback and its category; `invoke_or_native` now uses it
instead of the two separate calls.

**Verification for all three:** `cargo test -p cratonvm-vm --lib
skip_list::tests` — 58/61 pass; the 3 failures
(`elasticsearch_vector_diskbbq_hang_cluster_stays_interpreted_by_default`,
`unboundid_rdn_name_value_pairs_lifts_with_allow_packages`,
`unboundid_rdn_name_value_pairs_skipped_conservatively`) reproduce
identically on unmodified `dev` (confirmed via `git stash` A/B on the same
worktree) — pre-existing, unrelated to this session's changes, not
re-investigated here.

## What actually reproduced this session (differs from the original hypothesis)

A from-scratch repro on a fresh `dev`-tip build (same day, after the six
`AsynchronousFileChannel`/`FileChannel` fixes from
`bug-h2-files-setposixfilepermissions-FIXED.md` were already on `dev`) showed
a **different** shape than the original doc's gdb snapshot:

- **CPU was never near 0%** — every live-`gdb` attach during the "hang"
  (before and after each of the three fixes above) showed both real threads
  actively executing, burning 100%+ combined CPU. This is not a parked
  deadlock in the classical sense.
- **Repeated `gdb` snapshots a few seconds apart showed genuinely different
  code locations each time** for both threads (interpreter dispatch, native
  registry lookups, `jit_invoke_virtual_mic`, `AQS`/`RRWL` CAS retries,
  `LockSupport.park`/unpark cycles) — real, if extremely slow, forward
  progress, not a stuck loop repeating the identical frame.
- The `T19_H6_CAS_DIAG cas_long FAIL` diagnostic that fires early in
  `testConcurrent` (`class=ReentrantReadWriteLock$NonfairSync slot=3`,
  values like `current=Long(4294967296) expected=Long(0)`) looked alarming
  (large, oddly-shifted values) but is a **red herring, not a bug**: it is
  simply the expected shape of a `long`-state CAS on
  `AbstractQueuedLongSynchronizer` — RRWL genuinely extends the long-state
  synchronizer on JDK 25 (`javap`-verified), and `4294967296` (`2^32`) is
  just the shared-lock-count field's normal encoding in the upper 32 bits of
  that `long`. A minimal standalone two-thread `ReentrantReadWriteLock`
  repro (200,000 iterations, no H2) hit this same diagnostic every run,
  under both `--jit on` and `--nojit`, and **always completed correctly**
  (`counter=200000 expected=200000 failed=false`) — just ~240x slower than
  real HotSpot (`~8s` vs `~32ms`). This diagnostic is explicitly documented
  elsewhere in this codebase
  (`fork6-fjp-multithread-jit-root-reclamation-FIXED.md`) as "benign
  lock-free retry noise" for a different synchronizer (`ForkJoinPool`'s
  `ctl`); the same characterization applies here.
- The original doc's specific gdb snapshot (`main-vm` blocked in
  `request_osr()`'s `CompilationPolicy` mutex, reached via
  `native_lhm_put_evict`/`LinkedHashMap.put()`'s `removeEldestEntry()`
  reentrant callback) was **not observed at all** across many repeated
  attach attempts this session, before or after the fixes above. Whether
  that snapshot represented a genuinely different, rarer interleaving that
  simply didn't recur today, or was itself already resolved by something
  unrelated that landed on `dev` between the original doc's writing and this
  session, is unknown — it was not reproduced, so it could not be
  re-investigated.
- A **separate control-run finding**: `--nojit --Xmx 1g` (and even `--Xmx
  4g`) does **not** cleanly pass either — it OOMs (`FATAL: G1: out of heap
  space`) within ~10s, far faster than a genuine 10,000-iteration test
  should exhaust even 1GB. This was flagged as the doc's suggested next step
  #1 and is now answered: `--nojit` does NOT rule in or out the OSR
  hypothesis, because it fails via an unrelated, much faster OOM before
  `testConcurrent` can run long enough to say anything about JIT-specific
  behavior. This OOM is itself unexplained and is a candidate for separate
  investigation (not pursued further this session — the `--jit on` path was
  the actual target).

## Open: remaining performance gap (not fixed this session)

Even with all three fixes above, a fresh `TestFileSystem` class run under
`--jit on --Xmx 1g` (the suite runner's real-JDK default) still hits the
runner's 300s watchdog (`HANG 300.0s`). Direct, timeout-free runs (bypassing
the suite runner) were let continue for 20+ minutes without completing, while
repeated `gdb` sampling confirmed the process remained genuinely (if very
slowly) active the entire time — not stuck.

This is a **real, currently open performance ceiling**, not a correctness
bug: `testConcurrent` combines two real OS threads, real disk I/O through
`AsynchronousFileChannel`'s native Rust implementation (a per-file
`std::sync::Mutex` serializing concurrent read/write on the same handle,
`native-io/src/lib.rs::afc_read_at`/`afc_write_at`), and now-correctly-
interpreted (post-fix-#2) `AbstractQueuedLongSynchronizer`/
`ReentrantReadWriteLock` CAS-retry-heavy locking — a combination whose
current interpreted/dispatch overhead, compounded across the test's 10,000
main-loop iterations plus a continuously-looping second thread, apparently
exceeds 300s (and possibly by a lot) on this hardware.

**Suggested next steps for whoever picks this up:**
1. Get a wall-clock number for a genuinely completed run (let a from-scratch,
   timeout-free repro run for as long as it takes — this session ran out of
   budget before observing a natural completion, only confirmed >20 minutes
   without one). That number tells you whether this needs a 10x fix or a
   1000x fix.
2. Profile a live run with `perf record -g -p <pid>` (or repeated `gdb -batch
   -ex 'thread apply all bt'` sampling, the poor-man's version used this
   session) over a longer window than the 5-sample profile taken here, to
   find the dominant remaining cost with confidence — this session's small
   sample pointed at native-dispatch hashing (now partially addressed by fix
   #3) and interpreted AQS/RRWL overhead (now unavoidable given fix #2 makes
   these methods correctly-but-slowly interpreted), but a proper profile
   would settle which dominates and by how much.
3. Consider whether `afc_read_at`/`afc_write_at`'s per-file
   `std::sync::Mutex` (native-io/src/lib.rs) is itself a bottleneck under
   this test's tight two-thread read/write interleaving on the same file —
   not investigated this session.
4. Separately: root-cause the `--nojit --Xmx 1g`/`--Xmx 4g` G1 OOM noted
   above (10s to exhaust 4GB is itself suspicious and worth its own
   investigation, independent of this doc's JIT-mode focus).

## Repro
```
cd apps/h2database-suite-runner   # or a checkout with H2_ROOT set
TMPDIR=/data/data/tmp H2_ROOT=<h2 checkout>/h2 \
  CRATONVM_BIN=<fixed cratonvm binary, dev >= 4afa20cea> JDK25=/home/victor/jdk25 \
  bash run-h2-suite.sh run --category all --only 'TestFileSystem' \
  --jdk real --jit on --class-to 300
# -> still HANG at 300.0s (performance gap, not a deadlock -- see above)
```
Minimal non-H2 repro used to isolate/verify fixes #1 and #2 (no disk I/O, no
H2 dependency — completes in ~8s post-fix vs hanging pre-fix on the env-var
issue specifically):
```java
import java.util.concurrent.locks.ReentrantReadWriteLock;
public class RrwlRepro {
    public static void main(String[] args) throws Exception {
        final ReentrantReadWriteLock rwLock = new ReentrantReadWriteLock();
        final int ITERS = 200_000;
        Thread writer = new Thread(() -> {
            for (int i = 0; i < ITERS; i++) {
                rwLock.writeLock().lock();
                try { /* ... */ } finally { rwLock.writeLock().unlock(); }
            }
        });
        Thread reader = new Thread(() -> {
            for (int i = 0; i < ITERS; i++) {
                rwLock.readLock().lock();
                try { /* ... */ } finally { rwLock.readLock().unlock(); }
            }
        });
        writer.start(); reader.start(); writer.join(); reader.join();
        System.out.println("DONE");
    }
}
```
