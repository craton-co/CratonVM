# Moving-young collector throughput

Slug: `moving-young-throughput` · 2026-07-26
Follows `docs/internal/fixed-suite-bugs/app-jvm-bugs/moving-young-gen-drops-jit-held-oops-FIXED.md`,
which closed the correctness blocker and left "moving-young is ~3× slower than
the default sweep" as its first residual.

---

## VERDICT

**Most of the gap was not the copying collector. It was a hash table.**

Before every moving cycle, `collect_garbage_inner` walked from-space and
inserted **every object start — live and dead —** into an `FxHashSet<usize>`
(`young_object_starts`), to give `forward_object` an exact "is this an object
start?" predicate that rejects aligned interior words from conservative roots.
The set's size is proportional to **how much was allocated**, not to how much
survives, so a moving collection's cost scaled with the garbage.

`perf record` over a whole bt18 `-Xmx8g` run, moving-young on:

```
40.73%  hashbrown::map::HashMap<K,V,S,A>::insert
 8.80%  cratonvm_vm::jit::helpers::jit_frame_record
 8.32%  hashbrown::raw::RawTable<T,A>::reserve_rehash
 2.77%  cratonvm_gc::gen_heap::GenerationalHeap::forward_object
```

**~49% of the entire process** in hash insertion and rehash, against 2.8% in
the forwarder that does the actual copying. The same profile on the default
build has no hashbrown frame in its top 20 at all.

Replacing the set with a bit-per-8-bytes bitmap over `[base, base + used)`
(`young_mark::ObjectStartBits`) removes it. bt18, 6 interleaved rounds, min:

| bt18 `-Xmx8g` | min | vs default |
|---|---:|---:|
| default (non-moving sweep) | 1363 ms | 1.0× |
| `CRATONVM_MOVING_YOUNG=1`, hash set | 6833 ms | 5.0× |
| `CRATONVM_MOVING_YOUNG=1`, bitmap | 2905 ms | **2.1×** |

At `-Xmx512m` (25 moving cycles) it is 7692 ms → 3049 ms. Note that the
default build **cannot run bt18 at `-Xmx512m` at all** — it dies with
`OutOfMemoryError: young gen exhausted`, while the compacting collector
completes. That is the case moving-young exists for, and it now costs about
what it should.

The bitmap is an *exact* substitute, not an approximation: every object start
is 8-byte aligned (`gen_object_total_size` rounds every footprint up to 8, the
walk begins at the page-aligned arena base, and the free-block / TLAB-tail
skips it honours are allocation-granular). An unaligned start would alias a
neighbour's bit, so `insert` refuses it and the cycle diverts to the
non-moving sweep — the same fail-closed exit the walk already had for a corrupt
header. `vec![0u64; n]` allocates zeroed, so a 64 MB bitmap for a 4 GB
from-space is a zero-page mapping rather than a memset, and the only pages
touched are the ones the walk touches anyway.

---

## Method

The host is shared with several other JVM suites and rustc jobs, and its VM
exposes **no PMU** (`perf stat -e instructions` reads `<not supported>`), so
neither instruction counts nor a single wall-clock sample is usable. Two
things were needed to get numbers that reproduce:

1. **Interleave the configurations.** Running all N samples of config A and
   then all N of config B lets one slow window land entirely on one config. An
   early pass done that way "measured" the default build at 9.8 s against
   moving-young at 6.0 s — i.e. it inverted the result being investigated.
   Round-robin the configs and take the min per config.
2. **Report the minimum, not the mean.** Under a drifting load the minimum is
   the least-contended sample; means and medians track the neighbours' jobs.

`scripts/`-free harnesses were used ad hoc; the shape is in this document's
history. Anyone re-measuring should check `uptime` first — at load ~20 the
spread between rounds was 3× and nothing was decidable.

---

## Decomposition of what remains

bt18 `-Xmx20g` runs **zero** moving cycles (the young gen never fills), which
isolates the codegen overhead from the collector. 6 interleaved rounds, min:

| config | min | delta |
|---|---:|---|
| default | 1325 ms | — |
| `CRATONVM_JIT_FULL_SELF_CALL_SPILL=1` | 1471 ms | +146 ms |
| `MOVING_YOUNG=1`, `CRATONVM_SHADOW_NOPUSH=1` | 1708 ms | +383 ms |
| `MOVING_YOUNG=1` | 2008 ms | +683 ms |

So on bt18 the moving-young **codegen** cost is ~+50%, roughly half of it the
shadow-stack push/reload (2008 → 1708) and the rest the things moving-young
forces on every safepoint: `can_elide_self_call_register_spill` returns false,
so a hot self-recursive call takes the full `emit_pre_safepoint_spill` instead
of the two-instruction metadata-only path (the
`CRATONVM_JIT_FULL_SELF_CALL_SPILL` row prices that at ~+146 ms on the default
build); plus `flush_scratch_registers` and the post-safepoint oop-local reload.

**Correction to an earlier claim.** The FIXED doc originally attributed part of
this to "the full-GPR safepoint spill". That is wrong: `precise_maps` has been
default-on since 2026-07-07 and `precise_implies_reg_spill` folds it into
`safepoint_reg_spill_all`, so the blind spill already runs on the **default**
path. Measured on bt16, `CRATONVM_NO_PRECISE_REG_SPILL=1` saves ~34 ms on the
default build. Moving-young does not add the spill; it removes the elision that
kept the spill off self-recursive calls.

At `-Xmx8g` with the bitmap, the whole remaining picture is:

| component | ms |
|---|---:|
| default | 1363 |
| + moving-young codegen (no shadow) | ~+370 |
| + shadow push/reload | ~+300 |
| + one moving cycle | ~+870 |
| = `MOVING_YOUNG=1` | 2905 |

The post-fix profile at `-Xmx512m` (25 moving cycles, so the collector's share
is at its largest) shows no remaining hot spot: `collect_garbage_inner` 9.3%,
`forward_object` 2.6%, `compact_oop_scan` 2.4%, `take_dirty_cards` 1.4%.

---

## Remaining ideas, not taken

1. **`pointer_map` is still an `FxHashMap<usize, usize>`**, one entry per
   *surviving* object. That is the classic Cheney forwarding table, and a
   classic Cheney collector does not need it — it writes the forwarding pointer
   into the from-space object's own header. Worth doing only if
   `forward_object` becomes hot again; it is 2.6% now. The blocker is that
   `GcResult.pointer_map` is a published API consumed by monitor cleanup and
   reference processing, so the change is not local.

2. **Restore self-call spill elision under moving-young.** The predicate
   currently fails closed on `moving_young_enabled()`. It could instead ask the
   real question — whether any live oop at the call is register-resident and
   unpublished — which is now answerable, since the safepoint already computes
   `collect_live_oop_homes`. Worth ~150 ms of the ~370 ms codegen delta on
   bt18.

3. **The `jit_frame_record` prologue helper is 8–13% of both builds.** Not a
   moving-young cost, but it is the largest single non-GC symbol in every
   profile taken here and nobody has priced it.

None of these changes the conclusion: bt18 is the worst case for a copying
collector (a very large live set, so copying cost is near its maximum relative
to sweeping), and at 2.1× on that workload — while being the only configuration
that completes at `-Xmx512m` — moving-young is no longer disqualified on
throughput.

## Status: the default has flipped; these optimizations have not landed

`types/src/flags.rs::DEFAULT_MOVING_YOUNG` is now `true`, with
`CRATONVM_NO_MOVING_YOUNG` as the compatibility opt-out. The footprint result
decided it: a configuration that cannot complete bt18 at `-Xmx512m` is not a
safe default, whatever its steady-state throughput on a large heap.

So the three residuals above are **open optimization work on the default
path**, not preconditions for a flip that has not happened yet. They belong to
the [framework and CPU throughput program](framework-throughput.md), and should
be read together with the second gate documented in
[`ARCHITECTURE.md`](../ARCHITECTURE.md#memory-gc-crate): the flag being on does
not mean a given cycle compacted, so any profile must be read against
`moving_young: cycles=N coverage_fallbacks=M` before time is attributed to
compaction.

The workload mix this document originally asked for is still owed. It is now a
regression-budget question — whether the moving default costs more than its
footprint win on the named CPU and framework workloads — rather than a go/no-go
one.
