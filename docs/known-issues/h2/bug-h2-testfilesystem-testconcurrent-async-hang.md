# `TestFileSystem.testConcurrent` hangs against the `async:` filesystem — three real bugs fixed, one open performance gap remains

## Status
**PARTIALLY FIXED, 2026-07-25 update.** Three bugs fixed 2026-07-23 (merged
`4afa20cea`) plus a fourth, dominant one found and fixed 2026-07-25 (merged
`dev` `74b709d8e`): `ArenaStore::locate()`'s O(n) linear scan, ~24% of all
CPU on this workload — see "Session 2026-07-25" below. Even with all four
fixes, the class **still exceeds** the suite runner's 300s per-class
watchdog under `--jit on --Xmx 1g` — confirmed via a from-scratch,
timeout-free run that did not complete within 3600s (60 minutes) either,
before or after the 2026-07-25 fix (see below for exact figures). This is
now well-characterized as a **flat, distributed performance ceiling**
across normal interpreter/JIT-dispatch overhead — not a single further
discoverable bug — see "Open: remaining performance gap" for the precise
state and what fixing it completely would actually require.

## Three bugs fixed 2026-07-23 (merged to `dev` `4afa20cea`)

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

## What actually reproduced 2026-07-23 (differs from the original hypothesis)

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
  space`) within ~10s. This is now root-caused — see
  [`bug-g1-native-alloc-no-safepoint-oom.md`](../bug-g1-native-alloc-no-safepoint-oom.md).

## Session 2026-07-25: dominant cost found and fixed (merged `dev` `74b709d8e`)

Picked up this doc's own next-steps #1 (wall-clock number) and #2 (proper
profile). Repro'd against a fresh worktree off `dev` `346c74b71` (confirmed
all three 2026-07-23 fixes present) on the same Azure host.

**Wall-clock #1 (this doc's suggested next step):** a from-scratch,
timeout-free `TestFileSystem` run under `--jit on --Xmx 1g` did **not**
complete within 3600s (60 minutes) — the process was hard-killed by the
observation harness's own `timeout --kill-after=10 3600` wrapper at exactly
that mark. `gdb`/`perf` sampling throughout confirmed it was never stuck —
both real threads were continuously executing different code each time
sampled — consistent with 2026-07-23's characterization, just now with a
concrete lower bound: **it takes longer than 60 minutes**, not just "longer
than 20 minutes."

**Wall-clock #2 (proper profile, this doc's suggested next step #2):**
`perf record -F 999 -p <pid> -- sleep 30` (no `--call-graph`, since this
binary's `debug=line-tables-only`/no-frame-pointer release profile makes
`perf`'s call-graph attribution unreliable — confirmed independently the
same day chasing a `getenv` hotspot in CratonBench, see
`ld-preload-getenv-tally-beats-dwarf-callgraph` — but flat, non-call-graph
leaf sampling needs no unwinding and is fully reliable) on the live,
30-minutes-in process: **38,854 samples**, one function dominating at
**23.95% of all CPU time** —
`cratonvm_native_builtins::unsafe_natives_ext::unsafe_arena::ArenaStore::locate`.
Every other function was under 6%.

**Root cause:** `ArenaStore` (`native-builtins/src/unsafe_natives_ext.rs`)
backs `sun.misc.Unsafe`'s off-heap memory natives
(`allocateMemory`/`getByte`/`putByte`/etc — used by direct `ByteBuffer` I/O,
which is what `AsynchronousFileChannel` uses under the hood).
`ArenaStore::locate()` found the arena containing a given address by first
trying an exact-key `HashMap` lookup (an existing code comment noted this
"only succeeded at offset 0"), then falling back to a **full linear scan
over every currently-live arena** for any non-zero offset — the common case
for literally any multi-byte buffer access. `testConcurrent`'s tight
read/write loop over real files hits this on every single byte/short/
int/long access at a non-zero offset, and the live-arena count only grows
as the test allocates more buffers over its 10,000 iterations — a real,
severe, and previously-undiagnosed algorithmic bottleneck.

**Fix:** arena bases only ever increase (`ArenaStore::allocate`'s
`next_addr` is monotonic), so live arenas are always disjoint, base-ordered
`[base, base+len)` ranges — exactly what a `BTreeMap` answers in O(log n)
via one `range(..=addr).next_back()` query. Switched the backing map from
`HashMap` to `BTreeMap` and rewrote `locate()` accordingly (same lookup
semantics, no behavior change). Commit `491679a63`.

**Verification:**
- `cargo test -p cratonvm-native-builtins --lib unsafe_natives_ext`: 5/5 pass
  (this file's own dedicated tests — there is no arena-specific unit test,
  correctness was additionally verified by the live runs below).
- Full crate suite: 3075/3080 pass; the 5 failures
  (`cglib_enhancer::fb_ref_bytecode_tests::fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`,
  `lang_string::tests::string_join_array_uses_to_string_for_custom_charsequence`,
  `logmanager::tests::t19_h3_get_logger_names_returns_snapshot_enumeration`,
  `logmanager::tests::t19_h3_reset_clears_logger_registry_but_keeps_singleton`,
  `regex_matcher::regex_lookbehind_tests::pem_block_to_der_roundtrip`)
  reproduce **identically pre- and post-patch, single-threaded** (ruling out
  test-order flakiness) — pre-existing `dev` state in modules with zero
  connection to `ArenaStore`, not investigated further here.
- Re-profiled the SAME live workload after the fix (fresh run, ~20+ minutes
  in, same 30s/999Hz sampling): `ArenaStore::locate` **no longer appears in
  the hot list at all**. The profile is now flat — the highest single
  function is `invoke_on_class_shared_inner` at 7.17%, everything else
  lower, spread across normal interpreter/JIT-dispatch machinery
  (`NativeMethodRegistry::find_with_kind`/`find`, `execute_invokevirtual_cached`,
  `resolve_method_metadata`, `find_method_recursive`, `jit_invoke_virtual_mic`,
  etc.) — a real, confirmed, substantial win, eliminating the single
  largest fixable cost found.

**Even after this fix, the class still exceeds the suite runner's 300s
watchdog** — a from-scratch, timeout-free run with the fixed binary also
did not complete within 3600s (60 minutes), hard-killed at exactly that
mark, identically to the pre-fix baseline. The remaining cost is now
genuinely distributed (no function above ~7%), matching this doc's own
2026-07-23 characterization of "a real performance ceiling," not a further
discrete bug. See "Open: remaining performance gap" below for what closing
it completely would require.

## Open: remaining performance gap (not fully closed)

Even with all four fixes above, a fresh `TestFileSystem` class run under
`--jit on --Xmx 1g` (the suite runner's real-JDK default) still exceeds the
runner's 300s watchdog by a wide margin — confirmed: a from-scratch,
timeout-free run with the 2026-07-25 fix applied **also did not complete
within 3600s (60 minutes)**, hard-killed by the observation harness at
exactly that mark, identically to the pre-fix baseline. The fix is real and
measured (the dominant, single fixable hotspot is gone, see above), but the
remaining cost — now confirmed to also exceed 60 minutes on its own — is far
larger than any further micro-optimization could plausibly close. This is a
**real, still-open performance ceiling**, not a correctness bug and — as of
2026-07-25 — not attributable to any single further fixable hotspot: the
post-fix profile is flat, with the largest remaining single function at
~7% of CPU.

**What the flat profile is actually made of** (2026-07-25 sample, largest
items): `invoke_on_class_shared_inner` (~7%), `NativeMethodRegistry::
find_with_kind`/`find` (~10% combined — native-dispatch hashing, already
partially addressed by 2026-07-23 fix #3, this is the *remaining*
irreducible per-call hash+lookup cost, not a duplicate-hash bug), general
interpreter dispatch (`execute`, `execute_frame_from_index`,
`execute_invokevirtual_cached`, `execute_instruction`), and
`find_method_recursive`/`Class::find_method` (~3% combined) — the last
being a linear method-table scan that `jit_invoke_virtual_mic` pays on
every call to a *permanently-interpreted* callee (e.g. the 2026-07-23
fix-#2 AQS/RRWL skip-listed methods): the JIT's monomorphic inline cache
only caches a *compiled* callee's entry pointer, so a stable-receiver-class
call into a method that will never compile still re-resolves via
`invoke_or_native`/`find_method_recursive` on every single call. This is a
real, second-tier optimization opportunity (an "interpreted-dispatch cache"
analogous to the existing compiled-entry MIC), but implementing it safely
means touching several JIT hot-dispatch paths
(`jit/src/helpers.rs::jit_invoke_virtual_mic` and friends) with a full
regression pass — out of scope for this session's time budget, flagging
here precisely rather than rushing it.

**Suggested next steps for whoever picks this up:**
1. ~~Get a wall-clock number for a genuinely completed run~~ — attempted
   2026-07-25: confirmed **>3600s (60 min) both pre- and post-fix** (both
   runs hard-killed at exactly the 3600s observation ceiling, never
   completing naturally). Nobody has yet observed this class complete
   naturally under `--jit on --Xmx 1g` at all; the true completion time
   (whatever it turns out to be) is still the most useful missing data
   point, and now clearly requires either a much longer observation window
   (hours, not one) or the deeper JIT-coverage work in item 5 below before
   it's practically obtainable.
2. ~~Profile a live run~~ — done 2026-07-25 (`perf record -F 999`, flat
   sampling, no call-graph). Dominant cost found and fixed
   (`ArenaStore::locate`, commit `491679a63`). Remaining profile is flat;
   see above for the breakdown and the one identified further opportunity
   (interpreted-dispatch caching for permanently-skip-listed callees).
3. Consider whether `afc_read_at`/`afc_write_at`'s per-file
   `std::sync::Mutex` (native-io/src/lib.rs) is itself a bottleneck under
   this test's tight two-thread read/write interleaving on the same file —
   still not directly investigated (did not show up as a distinct hotspot
   in the 2026-07-25 profile, but wasn't isolated either).
4. ~~Root-cause the `--nojit`/G1 OOM~~ — done 2026-07-25, see
   [`bug-g1-native-alloc-no-safepoint-oom.md`](../bug-g1-native-alloc-no-safepoint-oom.md).
   Not fixed (needs interpreter-wide safepoint-checkpoint additions with a
   full regression pass); the doc has a precise, scoped fix plan.
5. New: implement the interpreted-dispatch cache described above if the
   ~10-13% combined native-registry/method-resolution cost is worth
   pursuing further — likely the next-largest opportunity now that the
   arena bug is fixed, but architecturally more involved (JIT MIC/PIC
   changes, not a contained data-structure swap).

## Repro
```
cd apps/h2database-suite-runner   # or a checkout with H2_ROOT set
TMPDIR=/data/data H2_ROOT=<h2 checkout>/h2 \
  CRATONVM_BIN=<fixed cratonvm binary, dev >= 74b709d8e> JDK25=/home/victor/jdk25 \
  bash run-h2-suite.sh run --category all --only 'TestFileSystem' \
  --jdk real --jit on --class-to 300
# -> still HANG at 300.0s (performance gap confirmed to persist post-fix -- see above)
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
