# `new Object()` published sixteen zero bytes, and the collector could not parse its own arena

## Status

**FIXED 2026-09-08.** Retired from
`docs/known-issues/h2/testvaluememory-every-value-measures-zero-20260904`, whose
last title was "`System.gc()` under `-XX:+UseGenerationalGC` leaves 7 MB where
HotSpot leaves 0.5". Two defects, both closed:

| # | defect | fix |
|---|---|---|
| A | `System.gc()` under `-XX:+UseGenerationalGC` retained ~100% of an allocation-only workload's garbage, ~2.1 MB per round, monotonic, until the collector thrashed | `GC_FLAG_HEADER` — every allocator now publishes a header that is not all-zero |
| B | `Runtime.freeMemory()` reported the young arena's bump cursor, which the in-place sweep never retreats, so the reported heap filled once and never emptied | `heap_allocated_bytes` answers from `live_bytes_estimate` |

`org.h2.test.unit.TestValueMemory`, `-Xmx2g`, all 40 types:

| arm | before | after |
|---|---|---|
| CratonVM `-XX:+UseGenerationalGC` | **FAIL** at Type 0, `Used memory: 7018` (7.2x a 3x threshold) | **PASS**, worst row 2.30x |
| CratonVM default (ZGC) | PASS, worst row 2.67x | PASS, worst row 2.67x (unchanged — see "what B did not do") |
| CratonVM `-XX:+UseG1GC` | FAIL at Type 0, `Used memory: 3224` | FAIL, **unchanged, byte for byte** |
| HotSpot JDK 25 | PASS | PASS |

The G1 row is not this page's subject and is not a regression from it — the
predecessor page never ran that arm, and the number is identical on the binary
before this work. It is conservative JIT-frame roots at region granularity, and
it is now written down as
`docs/known-issues/h2/testvaluememory-fails-under-g1-on-conservative-jit-roots-20260908.md`
rather than left as a surprise for whoever runs the class next.

## A: the field-less object was byte-identical to a hole

`java/lang/Object` is `ClassId(0)`. A field-less object's `shape` is `0`.
`MARK_NEUTRAL`, `ObjectKind::Object` and `ArrayElementType::Reference` all
encode as `0`, so its mark word was `0` too. **A fresh `new Object()` therefore
published sixteen zero bytes** — indistinguishable, byte for byte, from
reclaimed, zeroed, unlisted arena space.

That `ClassId(0)` is the whole of it, and it is measured rather than reasoned
from the class store: churn `new Object()` against a zero-field USER class,
which is identical in every way except that its class id is not zero.
`-XX:+UseGenerationalGC -Xmx256m`, 125,000 per round, `usedKb` by round:

| churn | 0 | 1 | 2 | 3 | 4 |
|---|---:|---:|---:|---:|---:|
| `new Object()`, before | 3844 | 6662 | 9478 | 12295 | **15111** |
| `new Empty()` (zero fields, own class id), before | 3460 | 5508 | 6532 | 6532 | 7799 |
| either, after | 1341 | 1346 | 1346 | 1346 | **1346** |

+2.8 MB a round against a plateau, and the only difference between the two
workloads is whether the first header word is zero. After the fix the two rows
are indistinguishable, because the second word is no longer zero either.

(`probes/ChurnKind2.java`, added here. Run it as a pair: one arm alone cannot
separate "this shape leaks" from "this workload allocates".)

The young non-moving sweep walks the arena linearly, and its 2026-07-02
hardening treats an all-zero header word as *never* a walkable object: it is
freed-but-unlisted residue, or a live slot whose header a stale register-held
reference clobbered. Neither may be parsed and neither may be freed. Runs of
`new Object()` hit that arm two ways, and both retain:

* the empty-object-run recovery (added 2026-08-12) accepted a run and **stepped
  over it** — on-grid, keeping the reclaim decisions behind it, but never
  freeing the run itself;
* a run it refused **unwound every reclaim decision since the last anchor**, so
  one refusal cost the whole cycle.

`System.gc()` is what made this the whole of the leak rather than a slow drift:
`collect_garbage_inner`'s `explicit_full_gc` disjunct diverts every explicit
collection to the non-moving sweep, so the moving (Cheney) path — which reclaims
this garbage perfectly, and which the previous page measured doing so — never
ran.

### The fix

`cratonvm_types::GC_FLAG_HEADER` (`0x08`, the `gc_flags` nibble's fourth bit,
mark-word bit 59): **these sixteen bytes are a published object header.** Set by
`ObjectHeader::new` and by both JIT inline-allocation emitters
(`x64::objects::emit_inline_tlab_new`, `runtime_lowering::emit_inline_tlab_new_ir`),
never cleared, and preserved by every mark-word transition because it lives
inside `MARK_QUARTET_MASK`, which `quartet_of` carries through thin-locking,
inflation, forwarding and hash installation alike.

This is HotSpot's own property arrived at from the other end: its unlocked mark
word is `0b01`, not `0b00`, so no live header there is ever all-zero and its
linear walks never had the question to answer.

The zero-run machinery is untouched and stays as defence. What changed is that
real objects no longer reach it.

### Measured, `ChurnLoop 8 125000`, `-Xmx256m`, `-XX:+UseGenerationalGC`

`ChurnLoop` allocates 125,000 immediately-dead `Object`s per round, stores
nothing, and calls `System.gc()`. It is the minimal reproducer the previous page
arrived at and it is unchanged.

| | before | after |
|---|---|---|
| `usedKb` round 0 -> 7 | 3460 -> **18310**, monotonic | 1341 -> 1346, **flat** |
| `usedKb` round 30 | **64685** | 1346 |
| `zero_spans` (unwind-forcing) | 19 | **0** |
| `zero_empty_runs` (stepped over) | 162 | **0** |
| `live_inside` refusals | 25 | **0** |
| empty-run standing retention, last cycle | **4 285 104 B** | **0** |
| `no_header_flag` | — | **0** |
| 40 rounds | **hit a 320 s cap at round 30** | **1.1 s** |

The "after" column is A and B together: A is why nothing accumulates, B is why
the figure reads 1346 rather than the arena's high-water mark. With A alone the
leak is already gone — `usedKb` plateaus instead of climbing (12676 at round 7,
flat) and every counter above is already zero — and B then brings the reported
number down to the live set.

`no_header_flag` is `SWEEP_NO_HEADER_FLAG`, added here: it counts objects the
young walk sized whose header lacks the flag, i.e. allocation paths that publish
a header without it. Zero is the claim that the invariant actually holds, and it
is printed unconditionally in the young-sweep exit census for the H2-CID0
reason — a counter printed only when non-zero cannot tell "clean" from "the walk
never ran".

## B: `freeMemory()` reported a high-water mark

`Runtime.freeMemory()` is `committed - used`, and `used` came from
`VmHeap::allocated_bytes`, which for the generational arm is the young arena's
raw bump cursor plus old-gen. The in-place non-moving sweep reclaims into a free
list **without retreating that cursor**, so once young filled, `used` stayed
pinned at its high-water mark for the rest of the process however much the
collector freed.

`heap_allocated_bytes` now answers from `live_bytes_estimate` — `young.used -
young.free_list + old.used` — which is what the trait's own doc always described
("bytes currently allocated"): free-list bytes are not allocated, they were
freed. The other collectors' `live_bytes_estimate` falls through to
`allocated_bytes`, so this is a no-op for them.

`ListDropLoop 5 125000`, `-Xmx2g` — build 125k objects into an `ArrayList` and
an `IdentityHashMap`, drop both, collect, read:

| arm | `after` before the fix | `after` with it |
|---|---|---|
| `-XX:+UseGenerationalGC` | 30822 KB | **8646 KB** |
| default (ZGC) | 8817 KB | 8817 KB (already live-based) |
| HotSpot | 1341 KB | 1341 KB |

### What B did not do, and the previous page's attribution was wrong about it

The page said the default collector's 2.3x margin "is what puts the H2 test at
2.3x of its 3x threshold" and called B "a separate, smaller fix" for it. B does
not move that number at all: the default collector is **ZGC**
(`VmConfig::default`, 2026-08-10), ZGC resets its `allocated` figure to the
swept live set on every collection, and its metric was already occupancy. The
page was written as though "default" still meant the generational collector.

The residual is measured and attributed below instead.

## The residual, measured: it is conservative JIT-frame roots

`TestValueMemory` Type 0 reads 2228 KB where HotSpot reads 488. Turning the JIT
off answers the whole of the difference:

| Type 0, `Used memory` | default (ZGC) | generational | G1 |
|---|---:|---:|---:|
| JIT on | 2228 | 2228 | 3224 (fails) |
| `--nojit` | **977** | **977** | **976** |

HotSpot reads 488. With precise roots all three collectors agree to within a
kilobyte; the whole spread is what a *conservative* root costs each of them.

and `probes/RealDrop.java` — added here — isolates the mechanism on H2's shape:
build 125,000 objects into a local array, consume them, drop the array, collect.
`-Xmx2g`:

| arm | before | peak | after | retained |
|---|---:|---:|---:|---:|
| HotSpot | 1785 | 1273 | 1273 | **0** |
| CratonVM `--nojit`, ZGC | 1511 | 1511 | 1511 | **0** |
| CratonVM `--nojit`, generational | 1341 | 1341 | 1341 | **0** |
| CratonVM `--nojit`, G1 | 1420 | 3373 | 1420 | **0** |
| CratonVM JIT on, ZGC | 1511 | 4441 | 4441 | **+2930** |
| CratonVM JIT on, generational | 1341 | 5133 | 4270 | **+2929** |
| CratonVM JIT on, G1 | 1420 | 4445 | 4445 | **+3025** |

Every `--nojit` row reclaims completely; every JIT-on row keeps the whole
structure. `peak` is the second, independent reading, and it separates the two
ways a row can come back clean: `a` is still in a local slot when `peak` is
taken, but its last use is already past, so a collector with per-bci local
liveness may take the structure before that first reading. HotSpot does
(`MethodLiveness` feeds its oop maps) and **so does CratonVM's interpreter**
(`runtime::local_liveness::live_locals_mask`, which `Frame::scan_local_objects`
filters every root through). G1's `--nojit` row is the third case and the reason
not to read `peak` alone: its liveness is region-granular, so the structure is
still counted at `peak` and every byte of it is still freed by `after`.

What does not agree is the compiled frame. A `peak` far above `before` that
`after` does not come down from is a dead spill slot still naming the structure,
and the conservative JIT-frame scan has no way to know it is dead.
Over-retention is the accepted cost of scanning compiled frames without precise
oop maps; it is not a defect of this page.

The last 2x — 977 against HotSpot's 488 — is **not** accounted for here. It is
1.0x of `calculated` against 0.5x, it is present with the JIT off and on both
collectors, and a 16-byte uniform value cell against HotSpot's compressed object
layout is the obvious candidate. That is a hypothesis and nothing on this page
measured it. What the page does establish is that it is not the leak, not the
metric, and not enough to threaten a 3x threshold.

> **CORRECTED 2026-09-09.** The hypothesis in the paragraph above is wrong, and
> so is the sentence that calls 977 a residual at all. `testType` nulls `list`
> and `map` and **does not null `array`**, and it reads `array.length` AFTER the
> measurement — so the 125 000-slot `Object[]` is a LIVE local across both
> `System.gc()` calls, on every JVM. 125 000 x 8 + 16 = 1 000 016 B = 977 KB is
> that array, and HotSpot's 488 is the same array with 4-byte compressed
> references. There is no over-retention in the `--nojit` rows: **977 is the
> floor, reached exactly**, and only compressed references could go below it.
> The value cells are `ValueNull.INSTANCE` — one singleton, which is what
> `size: 1` in the assertion message reports — so their layout cannot be the
> cause. Measured in
> `docs/known-issues/h2/testvaluememory-fails-under-g1-on-conservative-jit-roots-20260908.md`,
> which also prices what the JIT-on rows would cost with precise compiled-frame
> roots.

The first version of this section drew the conclusion from the `retained`
column alone — "`--nojit` retained 0, JIT on retained 2930" — which is the
identical number for "reclaimed early" and "never counted", and would have read
the same way if the probe had been eliding the allocation outright. Then the
first fix for THAT keyed the verdict on `peak` alone, which G1's `--nojit` row
then contradicted. The probe now prints a verdict computed from both readings,
and the table above carries both columns for the same reason.

Three consequences worth stating plainly:

* the 3x threshold has real margin on the two arms this page is about, and the
  margin is bounded by a known mechanism rather than by an unexplained number;
* it does NOT have margin on G1, which is the same mechanism at region
  granularity and reads 3224 — see the known-issue split out above;
* anyone who wants Type 0 at HotSpot's 0.5x is asking for precise stack maps in
  compiled frames, which is a project and not a fix.

## Collateral this change found, and why both were real

**`concurrent_mark_object_size`'s `known_flags` had to grow the bit too.** It
screens `gc_flags & !(OLD_GEN | MARKED | COMPACT) != 0` and rejects the sizing.
Leaving it alone did not weaken the screen — it **inverted** it: with
`GC_FLAG_HEADER` on every object, every plain object failed, G1's concurrent
mark refused every gray entry as a torn header, `cleanup` took its
retain-everything fail-safe (`degraded=mark-implausible-header-retain-all`), and
G1 stopped unloading classes. `RClassUnloadSweep` caught it deterministically,
3/3, and the file's own
`concurrent_mark_object_size_rejects_inconsistent_object_header` caught it in
`cargo test`. **A "list of defined flags" is a definition site, and a new flag
must be added to every one of them in the same commit.**

**`header_reserved_fields_plausible` lost its only field that could fail.** It
screened the `gc_flags` nibble for an undefined bit; with all four defined that
is a tautology. It now screens `MARK_RESERVED_MASK` — the quartet's two reserved
bits (word 54..55), which are zero in every header any allocator publishes and
whose zero-ness the JIT already depends on (`CMP BYTE [recv +
KIND_TAGS_BYTE_OFFSET], 0` separates a plain object from an array in one
instruction, on that same byte). Two bits rather than one, so it rejects 3 of 4
random words on this field where it used to reject 1 of 2.

**The VM had already met this defect three times and written it down as a cost
each time.** None of the three said "the collector cannot parse its own arena",
and that is why it stayed:

* `vm/src/runtime/interpreter/invoke.rs` demoted its "Stale pointer detected"
  WARN to `debug!` for `java/lang/Object`-declared call sites, with the reason
  stated exactly: *"A bare `new Object()` IS all-zero, legitimately … the false
  positive is back … a tripwire that fires on healthy code is one nobody
  reads."* Its discriminator is literally `header_bytes == [0u8; 16]`. The
  premise is gone, so the WARN is restored here — measured at **zero** "Stale
  pointer detected" lines across the 92-vector suite on all three collectors,
  the H2 corpus (14 runs) and the probes. `java/lang/ClassLoader` KEEPS its
  demotion: that one was about receivers that genuinely lost their header
  (WildFly / JBoss Modules), not about the all-zero-by-design shape.
* `h1_tlab_object_header_has_nonzero_hash_at_allocation` was written as
  `assert_ne!(first_16, [0u8; 16])` and later **inverted** to `assert_eq!`, with
  a comment calling the loss "a real regression in the stale-pointer detector's
  discriminator, recorded here rather than hidden". Restored to `assert_ne!`.
* `init_object_header`'s doc claimed `identity_hash_code` was "eagerly assigned
  at allocation time (caller passes `next_identity_hash()`)". `ObjectHeader::new`
  has had no hash parameter since the 2026-08-06/07 shrink, so nothing had
  minted one for a month.

All three were about the same sixteen bytes, and the eager hash all three
reference is the wrong way to restore them: a hash minted at allocation makes
every `synchronized` block lose its `try_thin_lock` CAS and inflate a monitor.
`GC_FLAG_HEADER` is the same property at no such cost.

While reconciling that, the rationale comment above
`old_gen_mark_candidate_plausible` turned out to have been stale for a month: it
still named `_padding` (offset 6-7) and `_gc_reserved` (offset 22-23), fields the
32 -> 24 -> 16 header shrink deleted, and quoted a "~1/2^29
false-negative-on-garbage rate" computed by summing them with "5 undefined
`gc_flags` bits" — a four-bit field. The real figure was 1/2. A screen everyone
believes is 1/2^29 while delivering 1/2 is worse than one nobody trusts; the
comment now states the rate and where it comes from.

## What the previous page got right, and the one place it stopped short

Almost all of it. Its refutations of the original `real: 0` framing stand, its
`ChurnLoop` minimisation stands, its localisation to `System.gc()` and the
`explicit_full_gc` divert stands, and its `MARKWHY` census is what made the
mechanism visible at all. Its "two candidate fixes" section named the right one
first:

> 1. **At the allocator.** If a bare `new Object()` can reach the heap with no
>    parseable header, the collector cannot walk its own arena — a GC must be
>    able to parse every allocated object.

and then talked itself out of it:

> **Candidate (1) below is refuted.** `new Object()` DOES write a header; it is
> simply all zero. […] There is nothing missing at the allocator. Do not go
> looking for an elided header store.

The first sentence is correct and the conclusion does not follow. No store is
elided — and a header that is all zero is not a header the collector can find.
"Nothing is missing at the allocator" was true of the *store* and false of the
*value*, and the page spent two days downstream of that in the sweep as a
result. The lesson is narrow and worth keeping: **when a predicate cannot
distinguish two things, ask whether the thing being described is missing a
distinguishing bit before asking the predicate to guess harder.**

## The reverted change and the `unresolved_snapshot` proposal are now moot

The page carried a reverted `zero_run_verdict` change (resume at a live base
inside the run) and a refined proposal (accept the resume only when the live base
is not in `unresolved_snapshot`, so it is a proved base rather than a raw
conservative candidate). Both were priced at ~880 KB a cycle and neither closed
the leak.

Neither was implemented, and neither should be on the strength of this page.
With the header flag in place, on the workload they were written for:
`zero_spans=0`, `live_inside=0`, `zero_empty_runs=0`. There is nothing left for
the narrowing to narrow. The guard test
`a_live_base_inside_the_run_is_still_a_desync` stands unchanged and still
forbids the unsafe version; if a future workload puts genuine zero runs back
into that arm, the page's analysis of `SWEEP_PHANTOM_INTERIOR_MARKS` and
`unresolved_snapshot` is the right place to start and is preserved in the git
history of the retired file.

## Verification

* `cargo test --release --lib` for every crate this touches —
  `cratonvm-types` (604), `cratonvm-gc` (1883), `cratonvm-jit` (4231),
  `cratonvm-vm` (2630), `cratonvm-native-builtins` (2278),
  `cratonvm-native-api` (347) — all green.
* **`cargo test --workspace` did NOT complete, and not for a code reason.** The
  release profile builds ~277 test binaries under `lto = "fat"`, and the link
  step ran the machine out of disk: `LNK1180: not enough disk space to complete
  the link`, with 0.6 GB free on a 930 GB volume shared with a dozen other
  worktrees' target directories. Freeing 2.2 GB of my own artifacts was enough
  for the per-crate runs above but not for the whole set. Recorded rather than
  quietly downgraded: the integration tests under `vm/tests/` and `vm-cli/tests/`
  have NOT been run against this change, and the regression suite below is what
  stands in for them.
* `regression-suite/run.sh` (92 vectors, each diffed against HotSpot) — 92/92 on
  the default collector, on `-XX:+UseGenerationalGC` and on `-XX:+UseG1GC`.
* Zero `Stale pointer detected` lines across that suite, the H2 corpus (14 runs)
  and the probes, on all three collectors — which is what licenses restoring the
  WARN that had been demoted for `java/lang/Object` call sites.
* The H2 corpus the previous page tabulated, both collectors, `-Xmx2g`:
  `store.TestCacheLIRS`, `unit.TestBitStream`, `store.TestObjectDataType`,
  `store.TestDataUtils`, `store.TestSpinLock`, `unit.TestIntPerfectHash`,
  `unit.TestStringUtils` — all `rc=0`, unchanged.
* `store.TestMVStore` (`rc=124`, capped at 240 s) and `store.TestMVRTree`
  (`rc=1`, immediate) both still fail, exactly as the previous page recorded,
  and both **fail on HotSpot too** (`rc=1` each on JDK 25 against this same
  checkout), so neither is an oracle. `TestMVStore`'s
  CratonVM-hangs-where-HotSpot-asserts asymmetry, which the previous page
  flagged as worth its own look, is untouched by this work and remains open on
  its own terms.
* Two new guards, so this cannot come back silently:
  `heap_types::tests::a_published_header_is_never_sixteen_zero_bytes` asserts on
  the BYTES a minimal header publishes rather than on the flag that currently
  makes them non-zero, and
  `gen_heap::tests::a_freshly_allocated_field_less_object_is_not_a_run_of_zeros`
  runs the sweep's own `zero_run_end` scanner over a real `ClassId(0)`
  zero-field allocation.

## Reproducers

Both are in the retired page's git history and both still run. `ChurnLoop` is
the one that matters:

```bash
cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx256m -cp . ChurnLoop 40 125000
```

Before: `usedKb` climbs ~2.1 MB a round (3460 at round 0, 64685 at round 30) and
the run is still going when a 320 s cap fires. After: flat at 1346 KB, 40 rounds
in 1.1 s.

```bash
H2=<h2 checkout>
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory
```

`rc=0`, 40 types.

## Do not undo

* `GC_FLAG_HEADER` must be set by **every** allocation path. `ObjectHeader::new`
  is the canonical constructor and covers 93 call sites; the two JIT inline
  emitters are the only paths that build a header without it. A path that
  forgets it does not corrupt anything — its objects simply become unparseable
  again, and `SWEEP_NO_HEADER_FLAG` is how that becomes visible.
* Nothing may treat the flag's **absence** as "not a live object" on a path where
  a wrong answer frees memory. `header_reserved_fields_plausible` gates
  conservative-root candidates, where a false negative is a premature free, and
  its doc says so at the site.
* A fifth `gc_flags` bit needs the same audit this one got: `ObjectHeader::new`,
  both JIT emitters, `concurrent_mark_object_size`'s `known_flags`,
  `header_reserved_fields_plausible`, and `JitRefStoreGates`' `young_floor` /
  `post_skip_mask` (both still sound at four bits — the nibble is at most 15 and
  cannot carry `age << 4` up to `(age + 1) << 4`).
