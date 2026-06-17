# Bug 06b — JIT-frame conservative root-scan cache is unsound (the ReflRepro residual)

**Severity:** High — **CratonVM-only**, JIT-on only. Heap corruption: the
non-moving young sweep reclaims a *live* young object whose only reference is a
JIT-frame conservative root that the per-thread scan **cache** dropped. The
freed slot is reused; a later sweep walks the resulting garbage header and aborts
with `non-moving sweep: stopping walk … implausible object size …` (`rc=132`
SIGILL).

**Status: FIXED** (worktree `C:\craton\CratonVM-gcsweep`, branch
`fix/gc-young-sweep-array-header`). The JIT-scan cache is **default-OFF**;
re-enable for benchmarking with `CRATONVM_JIT_SCAN_CACHE=1`.

This is the residual tracked after [bug-06](bug-06-jit-junit-discovery-reflection-corruption.md)
(reflection mirror arrays not GC-rooted) was fixed. Bug-06's `pin_native_root`
sweep fixed the mirror-array builders; this is a **separate** root cause in the
GC-root machinery itself, surfaced by the same reflection-heavy workload.

## Reproduce

```
cd <repo>
target/release/cratonvm.exe --java-home "<jdk25>" \
  -cp wildfly-suite/repro  ReflRepro 8000
# clean. Now force a young GC every 64 KB:
CRATONVM_DBG_GC_STRESS=65536 target/release/cratonvm.exe --java-home "<jdk25>" \
  -cp wildfly-suite/repro  ReflRepro 8000        # rc=132 (before fix)
```

`ReflRepro.scan` iterates `Class.getDeclaredFields()/getDeclaredMethods()` and
builds strings with `StringBuilder` — i.e. many object-returning native calls
under an active JIT frame, exactly the cache's hot path.

## Diagnosis chain (bisection)

| Experiment | Result | Conclusion |
|---|---|---|
| `CRATONVM_DISABLE_JIT=1` + stress | clean | JIT-on only |
| `CRATONVM_JIT_BISECT_ONLY=ReflRepro` | crash | corruptor is a ReflRepro method |
| `…BISECT_SKIP=ReflRepro.scan` | clean | **`scan` is the corruptor** |
| `…DISABLE_INLINE_NEW` / `…NO_BCE` / `…NO_SPEC_BCE` | crash | not alloc, not bounds-check elimination |
| **`CRATONVM_NO_JIT_SCAN_CACHE=1`** | **clean** | **the JIT-scan cache** |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | (slow) | a fresh full scan finds the dropped root |
| `CRATONVM_DBG_GCPATH` (added) | single-threaded; `collect_roots` finds 40–57 fresh JIT roots | the *authoritative* scan is complete |
| `CRATONVM_DBG_SWEEP_EDGES` | `root=0 young-survivor=0 old-gen=0` | reclaimed node has **no** heap/root/card edge — its only ref was a register/native-stack root the marker couldn't see |

A bespoke cache-miss probe in `scan_active_jit_frames` confirmed the cache
returns **fewer** roots than a fresh scan at the same generation (`cached=26`
vs `fresh=40`; the 14 dropped were mostly `cid=0 kind=Array` char[]/byte[] and
reflection objects).

## Root cause

The conservative scanner ([`conservative_roots::scan_active_jit_frames`])
reports every 8-byte word in `[scanner_sp, entry_sp]` that
`is_object_address`-validates as a root. To avoid re-walking that band on every
object-returning native call, results are cached per thread and reused while the
**`JIT_BOUNDARY_GEN`** (bumped at every JIT runtime-helper entry / chain
mutation) is unchanged. The premise: *"JIT spill slots can only change while
compiled code executes."*

That premise is **incomplete**. The scanned band `[scanner_sp, entry_sp]` also
covers the **interpreter / native / Rust stack BELOW the JIT frame** — every
callee a JIT method invoked. That region mutates continuously while interpreted
or native code runs, **without any boundary bump**, and it can hold the only
live reference to a freshly-allocated object (a `Field[]`, a `StringBuilder`
char[] in a native's Rust local, …) before it is stored into a tracked slot.
`update_root_snapshot` runs on every object-returning native call and fills the
cache at that generation, so a later root scan at the same generation reuses a
snapshot that **predates** those references and silently drops them. Because an
active JIT frame forces the **non-moving** young sweep (`gc_quiescence`), a
dropped live root is reclaimed in place and its slot reused → the corrupt
header / `implausible object size` walk abort.

A secondary, related flaw: a garbage collection **does not bump
`JIT_BOUNDARY_GEN`**, and several young sweeps can run at one generation during a
single interpreted callee — so the cache could also republish **freed
addresses** across a GC.

## Fix

`vm/src/jit/conservative_roots.rs`, `vm/src/memory/roots.rs`,
`vm/src/runtime/interpreter.rs`:

1. **Cache default-OFF** (`jit_scan_cache_enabled`): the cache is unsound for the
   conservative scanner; the proven-correct behaviour (`NO_JIT_SCAN_CACHE`) is
   now the default. Opt-in via `CRATONVM_JIT_SCAN_CACHE=1` for benchmarking.
2. **`collection_count` cache key** (hardens the opt-in path): the cache is also
   invalidated whenever a GC has occurred, so it can never republish a freed /
   relocated address. Only sampled when the cache is on (keeps the default path
   free of the stats-snapshot cost).
3. **Fresh GC-authoritative scans:** `collect_roots` (current thread) and
   `safepoint_check`'s pre-STW publish (parked thread) call
   `invalidate_scan_cache_for_gc()` before scanning, so the root set a collector
   actually marks from is always a full, current walk even if the cache is
   re-enabled.

## Verification

- `ReflRepro 8000` under `CRATONVM_DBG_GC_STRESS=65536`: **6/6 clean**
  (`ok=8000 bad=0`), incl. the minimal `BISECT_ONLY=ReflRepro` /
  `SKIP=describeField,main` config.
- `ReflRepro 40000` (no stress): clean, output byte-identical to the JIT-off
  golden reference.
- `MinRepro` / `ArrRepro` / `ForEachOrderedRepro` / `PBRepro`: no regression
  (with and without GC stress).

## Performance note & follow-up

Disabling the cache re-introduces the per-native-call conservative scan the
cache was added to avoid (a deep JIT-over-interpreter frame can make that scan
O(stack depth)). The impact is bounded: `collect_roots` already does its own
fresh scan, so the cache only ever optimised `update_root_snapshot`'s publish —
which is **redundant for the GC-initiating thread** and only consumed for a
*parked* thread during a cross-thread STW. The right long-term fix is a **sound**
cache that records the JIT frame's spill bounds (precise oop maps) and caches
only that genuinely-frozen region, re-scanning the mutating native stack below
it each time. Until then, OFF is correct; a cheaper interim win is to skip the
JIT-frame scan in `update_root_snapshot` entirely when `alive_count <= 1` (no
cross-thread consumer can ever read it).
