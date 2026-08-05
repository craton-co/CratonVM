> **RETIRED 2026-07-26 — archived, non-normative.** All seven bugs this doc
> names are fixed. What is left is a single performance wall —
> `testConcurrent` on `nioMemLZF:1:`, >18 minutes against HotSpot's 862ms —
> carried forward with its one concrete lead as "Residual 4" in
> [`h2-jitban-longtail1-CLOSED-20260805.md`](h2-jitban-longtail1-CLOSED-20260805.md).
> Note the filename is a misnomer: `async:` is the LEAST affected of the
> sixteen filesystems this class exercises. Do not cite this doc as current
> behaviour.

# `TestFileSystem.testConcurrent` is pathologically slow — worst on the LZF in-memory filesystems, and NOT on `async:` — seven bugs fixed, one open performance wall remains

*(Filename kept for the inbound links in `../../internal/fixed-suite-bugs/g1-native-alloc-no-safepoint-oom-FIXED.md`, `bug-h2-files-setposixfilepermissions-FIXED.md` and the Tomcat fixture-completion doc. The `async:` in it is a misnomer — see the per-prefix table below: `async:` is the LEAST affected filesystem of the sixteen this class exercises.)*

## Status
**PARTIALLY FIXED, 2026-07-26 update.** Three more per-invoke costs found and
fixed (uncached `getenv`, a redundant method-ref resolution, and the
native-registry digest), worth ~14% on the reduced lock-only form of this
workload and ~8% on an H2 reconnect workload — see "Session 2026-07-26" below.
The class still exceeds the 300s watchdog; the remaining gap is still the flat
ceiling this doc has described since 2026-07-23, and closing it needs the
architectural item (interpreted-dispatch caching), not more micro-fixes. The
2026-07-25 text below is unchanged.

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
  space`) within ~10s. This was root-caused 2026-07-25 and **FIXED
  2026-07-26** — see
  [`g1-native-alloc-no-safepoint-oom-FIXED.md`](../../internal/fixed-suite-bugs/g1-native-alloc-no-safepoint-oom-FIXED.md).
  `--nojit` no longer OOMs; it now runs into the same performance ceiling
  this doc covers instead (a 5400s `--nojit --Xmx 1g` run made no further
  visible progress once `testConcurrent` started, with a healthy GC cadence
  throughout and zero heap-exhaustion aborts).

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

## Session 2026-07-26: it is not contention, and three more per-invoke costs

Picked this up from the other side: instead of running the 60-minute class,
reduce it. `RrwlSingle` (in this doc's repro section below) does 100k
**uncontended** `ReentrantReadWriteLock` write-lock, read-lock and
`ReentrantLock` lock/unlock pairs on ONE thread, plus 100k `synchronized`
blocks as a control.

```
                 HotSpot     CratonVM (dev @ 8e8d4d4fb)     ratio
write lock         18ms                2950ms              164x
read lock           8ms                3014ms              377x
ReentrantLock       7ms                1936ms              277x
synchronized        2ms                  62ms               31x
```

Two things fall out immediately:

* **The cost is not contention and not parking.** One thread, no waiters, no
  `park`/`unpark` — and the AQS lock path is still 164-377x slower, while the
  VM's own monitor path (`synchronized`) is only 31x. Whatever this doc's
  original title called a hang is per-operation overhead on the AQS/native
  dispatch path, which is exactly what 2026-07-23 and 2026-07-25 concluded from
  the other direction.
* **`--jit on` is SLOWER than `--nojit` here** (1122ms vs 1563ms for the same
  100k triple). That is a new, concrete data point for next-step #5: the AQS
  methods are permanently skip-listed, so every JIT'd caller pays
  `jit_invoke_virtual_mic` -> `find_method_recursive` re-resolution on each
  call and gets nothing back. The JIT is a net negative on this shape.

### Three fixes (commits `2eeff06b8`, `c6c1d233d`)

**1. Six more uncached `getenv` reads on interpreter hot paths.** Same defect
class as 2026-07-23's fix #1, found the same way the CratonBench `getenv`
hotspot was (`ld-preload-getenv-tally-beats-dwarf-callgraph`): an `LD_PRELOAD`
tally counted **6,901,759** `getenv` calls for 300k lock/unlock pairs — ~23 per
operation — of which 6,900,696 were six ad-hoc debug traces reading their
variable inline per invoke / per putfield / per `if_acmpne`:

```
4501107  CRATONVM_DBG_GSE                        (invoke-cache lookup, twice per lookup)
1101202  CRATONVM_DBG_FIELD_WATCH                (every putfield + every field retarget)
 599999  CRATONVM_DBG_WATCHREF
 398881  CRATONVM_DBG_ASSERTEQ                   (every JIT-ABI invoke)
 199000  CRATONVM_EXEC_FRAME_TRACE               (every frame entry)
 100507  CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE (every if_acmpne)
```

Moved to `vm/src/runtime/env_cache.rs`'s existing `cached_is_set!` pattern:
**6,901,759 -> 1,070** calls, ~3.5% of CPU by `perf`.

**2. Three sites in `execute_invokevirtual_cached` resolved a method ref to
test a condition that is almost always false.** Each ran a full
`resolve_method_ref` — a resolution-cache `RwLock` read, a hash probe and four
`Arc` clone/drop pairs — before finding out it had nothing to do:

* the Spring `MergedAnnotation$Adapt.isIn` loader-split bridge. Its outer gate,
  `loader_aware_resolution()`, is **default-ON**, so this ran on EVERY
  invokevirtual/-interface/-special in the VM purely to compare the owner
  against one hard-coded class name. Now behind an arm-once atomic that stays
  false until `resolve_method_metadata` has actually resolved a CP entry naming
  that method — a strict prerequisite, since the guard's only effect is to
  return `CacheMiss` and an unresolved site has no inline-cache entry to bypass
  anyway.
* the cached-`Native` redefine-shadow re-check: now short-circuits on
  `any_class_redefined()` (a relaxed atomic load, and already the first line of
  the helper it feeds) before resolving.
* the cached-`VirtualNative` synthetic-stub re-check: now tests
  `real_protected_stub_class(receiver_name)` — the receiver-name half of the
  predicate it feeds — before resolving.

`resolve_method_ref` calls on the same workload: **6,784,000 -> 1,494,453**.
`resolve_method_metadata` went from 5.33% to 1.44% of CPU.

**3. `NativeMethodRegistry::slot_for_exact` answers the common miss from the
class name alone.** With those two out of the way it became the single largest
entry in a live 30s/999Hz profile of the real `TestFileSystem` — **8.75%**
across its two threads, plus much of the `__memcmp_evex_movbe` time spent
name-verifying digest hits. It sits on the every-invoke path via
`invoke_or_native`, hashes ~60-100 bytes byte-at-a-time across two
accumulators, and the overwhelming majority of its calls name application
classes (`org/h2/mvstore/...`, `org/h2/store/...`) that register no natives at
all. The triple digest is now split into a resumable class-name prefix plus a
finisher; every registered class's prefix hash lives in a set, and a class
absent from that set returns `None` without finishing the digest, probing the
map, or comparing any strings. A hash collision can only add a false positive,
which falls through to the pre-existing full verification. `slot_for_exact`
6.27% -> 3.94% on an H2 reconnect workload.

### The ceiling is NOT flat: one prefix/sub-test is the whole wall

The 2026-07-25 conclusion — "a flat, distributed performance ceiling, not a
further discoverable bug" — is true of the *profile* but not of the *test*.
Instrumenting `testFileSystem(String fsBase)` with a per-sub-test timer (a
compiled overlay of `TestFileSystem.java` prepended to the classpath, so the
shared H2 checkout is untouched) gives a per-prefix breakdown that nobody had
before, because `TestBase` prints nothing unless something fails — which is
also why the earlier "no output for 60 minutes" runs looked like a hang.

`testConcurrent`, CratonVM `--jit on --Xmx 1g` vs HotSpot, same host:

| prefix | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `./data/test/fs` | 83ms | 2038ms | 25x |
| `./data/test/fs` (after `split:10:`) | 58ms | 2020ms | 35x |
| **`async:./data/test/fs`** | 473ms | 2001ms | **4x** |
| `memFS:` | 31ms | 1036ms | 33x |
| `memLZF:` | 102ms | 11584ms | 114x |
| `nioMemFS:` | 44ms | 1143ms | 26x |
| **`nioMemLZF:1:`** | 862ms | **>1100s, never completed** | **>1200x** |

Everything up to and including `nioMemFS:` finishes in ~130s combined. Then
`testConcurrent` on `nioMemLZF:1:` runs for **over 18 minutes without
completing** (killed there; a second run with `CRATONVM_JIT_ALLOW_PACKAGES=
org/h2/` was equally stuck, so this is not the `org/h2` JIT ban). Whole-class
HotSpot total: **4.94s**. That single prefix/sub-test pair is what turns this
class into a >3600s run, and it is where any further work should point.

Note also that **`async:` — the prefix in this doc's title — is the LEAST
affected of them all at 4x.** The title's premise does not survive
measurement; the problem is `testConcurrent` generally, and the LZF-compressed
in-memory filesystems specifically.

### Why `testConcurrent` amplifies: an uncooperative spin lock

`testConcurrent` guards each 64 KB block with a raw, backoff-free spin over an
`AtomicIntegerArray`:

```java
while (!locks.compareAndSet(pos, 0, 1)) { }
try { ... f.read/write ... } finally { locks.set(pos, 0); }
```

with a `Thread.yield()` per reader iteration and two real threads. Both halves
of that are slow here, and they multiply:

* `AtomicIntegerArray.compareAndSet` + `set` measures **535ns/pair on CratonVM
  vs 20ns on HotSpot (27x)** — it goes through the full native dispatch path,
  and a live profile of the stuck `nioMemLZF:1:` run attributes **12.2%** of
  both threads to `NativeMethodRegistry::slot_for_exact` alone (plus 9.3% to
  `invoke_on_class_shared_inner` and 4% to `memcmp`), i.e. a quarter of the
  run is re-looking-up the same native on every spin iteration.
* the critical section itself (the `FileChannel` read/write, LZF
  compress/decompress for the `*LZF*` prefixes) is interpreted-speed, so the
  lock is held far longer than upstream assumes.

A slow lock primitive plus a long critical section plus an unbounded spin is
super-linear, which is exactly the shape of the table above: the prefixes with
the most work inside the lock (`memLZF`, `nioMemLZF`) are the ones that fall
off a cliff, while `async:` — which does its I/O outside the lock — is barely
affected.

**Concrete next target.** `invoke_or_native` probes the native registry on
every call, but `resolve_method_metadata` ALREADY caches the resolved native
callback and kind per constant-pool entry (`ResolvedMethod::native_target` /
`native_kind`) — its own doc comment says "the registry hash is paid once per
resolved CP entry, not once per call site that consumes it". Threading that
precomputed target into `invoke_or_native` instead of re-probing would remove
the ~12% directly and, more importantly, shorten the spin-lock critical
section, which is where the super-linear amplification lives. That is a
narrower and better-evidenced target than the general interpreted-dispatch
cache in item 5.

### Measured effect

Three alternating runs each, same host, same shape:

```
RrwlSingle (write+read+reentrant, 100k each)   2831ms -> 2428ms   -14.2%
SeqRepro   (H2 open/reconnect x300)             35.3s ->  32.5s    -8.0%
```

All three fixes are VM-wide, not H2-specific.

### What the profile looks like now

Live 30s/999Hz sample of the real `TestFileSystem` after the fixes, both real
threads combined: `slot_for_exact` 8.75% (before fix 3),
`invoke_on_class_shared_inner` 7.5%, `execute_frame_from_index` 4.5%,
`execute_invokevirtual_cached` 3.3%, `memcmp` 3.1%, `execute` 3.1%,
`safe_native_call_impl` 2.9%, `_mi_page_malloc_zero` 3.2%,
`jit_invoke_virtual_mic` 2.2%, `pop_coerced_invoke_args_virtual` 2.2%. Still
flat, still dominated by generic invoke plumbing — the shape 2026-07-25
described, one large item lighter.

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
1. ~~Get a wall-clock number for a genuinely completed run~~ — **answered
   2026-07-26, and the question was the wrong one.** The class does not have a
   uniform completion time to measure: it clears ~14 of its ~16 prefixes in
   ~130s and then parks in `testConcurrent` on `nioMemLZF:1:` for >18 minutes
   without completing (HotSpot does that same sub-test in 862ms and the whole
   class in 4.94s). See "The ceiling is NOT flat" above for the full per-prefix
   table and how to reproduce it. Measure THAT sub-test, not the class.
2. ~~Profile a live run~~ — done 2026-07-25 (`perf record -F 999`, flat
   sampling, no call-graph). Dominant cost found and fixed
   (`ArenaStore::locate`, commit `491679a63`). Remaining profile is flat;
   see above for the breakdown and the one identified further opportunity
   (interpreted-dispatch caching for permanently-skip-listed callees).
3. ~~Consider whether `afc_read_at`/`afc_write_at`'s per-file
   `std::sync::Mutex` (native-io/src/lib.rs) is itself a bottleneck~~ —
   **deprioritised 2026-07-26.** Those are the `async:` path, and `async:` is
   now measured as the *least* affected prefix (4x vs HotSpot, against 25-1200x
   for the others). Whatever that mutex costs, it is not what makes this class
   miss the watchdog.
4. ~~Root-cause the `--nojit`/G1 OOM~~ — root-caused 2026-07-25, **FIXED
   2026-07-26**, doc retired to
   [`g1-native-alloc-no-safepoint-oom-FIXED.md`](../../internal/fixed-suite-bugs/g1-native-alloc-no-safepoint-oom-FIXED.md).
   The fix did NOT need the interpreter-wide safepoint-checkpoint additions
   that doc originally scoped: G1 was given its half of the existing
   `young_spill_pressure` → `safe_native_call` boundary-GC protocol the
   generational heap already used, so no interpreter dispatch path changed.
   **Relevant to this doc's own performance investigation:** with GC actually
   running under `--nojit`, `CRATONVM_DBG_G1DIAG=1` shows this workload
   producing ~800 MB of Java-heap garbage per collection interval while
   copying only ~1 MB of live data — essentially all of it the `Integer` box
   plus completed-future wrapper `native_afc_read` allocates on every
   `AsynchronousFileChannel.read`. Cutting that per-call allocation is a
   concrete, measurable lead for item 5 below.
5. New: implement the interpreted-dispatch cache described above if the
   ~10-13% combined native-registry/method-resolution cost is worth
   pursuing further — likely the next-largest opportunity now that the
   arena bug is fixed, but architecturally more involved (JIT MIC/PIC
   changes, not a contained data-structure swap).
   **2026-07-26: this is now the only item left with an order-of-magnitude in
   it, and there is direct evidence for it** — on the reduced lock-only
   workload `--nojit` beats `--jit on` (1122ms vs 1563ms), which is exactly the
   predicted cost of JIT'd callers re-resolving permanently-skip-listed AQS
   callees on every call. The three 2026-07-26 fixes took ~14% off that
   workload and ~8% off an H2 reconnect workload; the class needs roughly 12x
   to fit the 300s watchdog, so no further micro-fix of this kind will close
   it.
6. New: the AQS/RRWL entries in `is_known_miscompile_aqs_family`
   (`vm/src/jit/skip_list.rs`) may be stale. The 200k-iteration two-thread
   `RrwlRepro` below, which this doc records as a PERMANENT hang that motivated
   adding `compareAndSetState`/`getState`/`setState`, now **completes cleanly**
   with the family lifted (`CRATONVM_JIT_ALLOW_PACKAGES=java/util/concurrent/
   locks/`) — 9358ms vs 8519ms with the ban in place, i.e. correct but not
   faster. Not enough to justify lifting on its own (JITting AQS is a small
   net loss on this shape), but worth re-testing as part of item 5: if the
   interpreted-dispatch cache lands, the trade-off changes.

## Repro
```
cd apps/h2database-suite-runner   # or a checkout with H2_ROOT set
TMPDIR=/data/data H2_ROOT=<h2 checkout>/h2 \
  CRATONVM_BIN=<fixed cratonvm binary, dev >= 74b709d8e> JDK25=/home/victor/jdk25 \
  bash run-h2-suite.sh run --category all --only 'TestFileSystem' \
  --jdk real --jit on --class-to 300
# -> still HANG at 300.0s (performance gap confirmed to persist post-fix -- see above)
```
Reduced, single-threaded, uncontended form used for the 2026-07-26 work — no
threads, no disk I/O, no H2, runs in seconds, and reproduces the whole ratio
(`RrwlSingle`):

```java
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.ReentrantLock;
public class RrwlSingle {
    public static void main(String[] args) throws Exception {
        int ITERS = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        long t0 = System.nanoTime();
        for (int i = 0; i < ITERS; i++) { rw.writeLock().lock(); rw.writeLock().unlock(); }
        long t1 = System.nanoTime();
        for (int i = 0; i < ITERS; i++) { rw.readLock().lock(); rw.readLock().unlock(); }
        long t2 = System.nanoTime();
        ReentrantLock rl = new ReentrantLock();
        for (int i = 0; i < ITERS; i++) { rl.lock(); rl.unlock(); }
        long t3 = System.nanoTime();
        Object mon = new Object();
        int s = 0;
        for (int i = 0; i < ITERS; i++) { synchronized (mon) { s++; } }
        long t4 = System.nanoTime();
        System.out.println("uncontended write=" + (t1-t0)/1000000 + "ms read=" + (t2-t1)/1000000
            + "ms reentrant=" + (t3-t2)/1000000 + "ms synchronized=" + (t4-t3)/1000000 + "ms s=" + s);
    }
}
```

To count the `getenv` traffic on any workload, use the LD_PRELOAD tally
(`/data/data/tmp/getenvspy_full.c` on the Azure host):
`GETENV_TALLY_OUT=tally.tsv LD_PRELOAD=/data/data/tmp/getenvspy_full.so <cmd>`.
To count resolution traffic, `CRATONVM_DBG_HOTPATH_COUNTS=1` prints
`force_native`/`resolve_method_ref`/`lookup_loader_initiated`/`retarget_field`
totals.

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
