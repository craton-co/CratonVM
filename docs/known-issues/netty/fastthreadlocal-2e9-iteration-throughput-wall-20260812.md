# FastThreadLocalTest — the inline allocator is not missing a contract, it is starved on the default collector

**Status: OPEN — throughput, not correctness.** The wall is unchanged. What
changed on 2026-08-26 is that **this page's item 1 was aimed at the wrong
thing**, and the real blocker is now measured and specified rather than guessed.

The class still does not finish inside any reasonable per-class wall. Read
"What would close it" first; everything above it is why the previous answer to
that question could not have worked.

## The correction

The previous revision of this page said, quoting `emit_inline_tlab_new`'s own
comment:

> `emit_inline_tlab_new` **does not allocate inline**: its own comment records
> that the raw compiled TLAB-cursor bump was routed back through the checked
> runtime helper […] Only the header writes are inline.

**The comment is stale and the code below it does the opposite.** As of the
BinTrees-18 fix, `emit_inline_tlab_new` emits the whole bump: it loads
`thread.tlab.cursor`, aligns it to 8, `LEA`s the new cursor, bounds-checks
against `thread.tlab.end`, writes the **complete** header (class_id, num_slots,
mark word), and only then commits the cursor —

```
    // Commit the bump LAST: [R10 + cursor_off] = RAX. […] x86-64 TSO preserves
    // the required header/body-before-cursor store order; the STW handshake
    // provides the acquire side.
```

That is `Tlab::alloc_initialized`'s publication protocol, in machine code, with
the ordering argument written out. **Item 1 as it stood — "give the JIT-emitted
TLAB bump the publication contract `Tlab::alloc_initialized` has" — asks for
something it already has.** Anyone who took that item would have gone looking
for a missing fence and found one already there.

## What is actually wrong

`thread.tlab` — the exact TLAB that bump reads — is **never refilled under the
default collector**:

```rust
pub fn refill_tlab(&self, requested_size: usize) -> Option<(*mut u8, usize)> {
    match self {
        VmHeap::Generational(h) => h.refill_tlab(requested_size),
        VmHeap::G1(h) => h.refill_tlab(requested_size),
        #[cfg(feature = "zgc")]
        VmHeap::Zgc(_) => None,
    }
}
```

`ZgcRealHeap` implements no `refill_tlab` at all — it has its own, separate
per-thread buffers (`ZArenaTlabRegistry` / `ZArenaTlab`), reached only through
`alloc_raw_tlab`. So on ZGC `thread.tlab` stays empty for the life of the
process, the inline bump's `CMP RAX, [R10+end_off]; JA slow_path` is taken on
**every** allocation, and every compiled `new` lands in `jit_new_object` →
`alloc_raw_tlab`. Which is exactly the profile this page already recorded:
`alloc_raw_tlab` 22.2%, `jit_new_object` 8.5%.

The only production writer of `thread.tlab` is the refill path in
`runtime/interpreter/gc_and_alloc.rs`, which asks the heap and gets `None`.
(`vm_init.rs` also installs one, but that is inside a `#[test]`.)

### The measurement that says so

`CRATONVM_JIT_REAL_NEW_SITE_FLAGS=1` is what lets the inline path be emitted at
all. ONE binary, one probe (`probes/CtorOnly alloc` = `new Object()`), four
collector settings, flag off -> on, three interleaved rounds, ns/op:

| collector | round 1 | round 2 | round 3 |
|---|---|---|---|
| default | 445.7 -> 394.0 | 255.6 -> 249.0 | 299.7 -> 274.4 |
| ZGC (explicit) | 422.9 -> 281.2 | 243.4 -> 265.5 | 406.6 -> 364.7 |
| **Generational** | **205.9 -> 46.6** | **320.0 -> 110.4** | **163.1 -> 39.2** |
| **G1** | **164.3 -> 93.6** | **255.7 -> 46.0** | **161.7 -> 73.1** |

and the `field` shape on the two extremes:

| collector | off -> on | off -> on |
|---|---|---|
| ZGC | 379.4 -> 409.4 | 397.3 -> 335.3 |
| **Generational** | **233.4 -> 137.3** | **260.9 -> 107.7** |

**Neutral where the TLAB is never refilled; 2.9-5.6x where it is.** That is the
signature the `refill_tlab` table predicts, and it is also the explanation this
page owed for a result it recorded but could not account for: the previous
revision measured the flag at 111.4 vs 118.0 ns and called it neutral. It was
measured on ZGC, the one backend where the feature is inert **by construction**.
`default` behaves as ZGC because ZGC *is* the default.

Generational with the flag on reaches **39-47 ns/op** against HotSpot's 11.2 on
the same host — a 3.5-4x gap where the default collector shows 12-15x.

## The trap: that `None` is load-bearing by accident

It arrived with `2f0d5bbbe feat: zgc` and carries no comment, so it reads like a
simple omission — a ten-line `refill_tlab` away from being fixed. **It is not.**

`ZgcRealHeap::alloc_tlab` registers every object base **inside the cell lock and
before the pointer is returned**, because `is_object_address` is a *mutator*-path
oracle on this backend, not just a GC one — `jit_invoke_dispatch` uses it to
resolve a receiver's class. An object produced by the inline bump registers
nothing. So the moment `refill_tlab` starts returning `Some` for ZGC, the JIT
bump begins succeeding and silently producing objects that
`is_object_address` denies exist.

**Anything that makes `refill_tlab` return `Some` for ZGC must, in the same
change, either register inline or keep the JIT bump off on that collector.**

## What would close it, in order

1. **Bridge ZGC to `thread.tlab`, with inline registration.** This replaces the
   old item 1. Worth the 2.9-5.6x measured above, on the collector the netty
   suite actually runs. Three parts:
   * `ZgcRealHeap::refill_tlab` — carve a chunk from the arena and hand back
     `(ptr, size)`. The carving machinery already exists for its own
     `ZArenaTlab` (`tlab_refill`);
   * emit the registry insert inline. This is the part that looks frightening
     and is not: the registry is a **bitmap**, and its address mapping is four
     arithmetic ops —
     `off = addr - base; bit = off >> 3; word = bit >> 6; mask = 1 << (bit & 63)`
     — over a `base`/`span`/`words` triple fixed at heap construction. One
     `lock or [words + word*8], mask` discharges it. The bump already aligns to
     8, which is the `off & 7 != 0` exactness rejection `locate` relies on;
   * `allocated.fetch_add(bytes)` plus the `gc_threshold` compare, so
     collections still trigger: one `lock add` and a branch to a helper on
     crossing.

   Refuse the fast path unless the registry is in `Bits` mode with an empty
   overflow set, and keep a kill switch. What this removes per allocation is a
   TLS lookup, a `Vec` scan, an `Arc::clone`, a `parking_lot` lock, an unlock
   and an `Arc` drop — around the *same two atomics* it keeps.

2. **Cheapen `alloc_raw_tlab`'s fast path** — unchanged from the previous
   revision, and it is what the DEFAULT collector pays today whatever happens
   to (1). The `Arc` clone/drop pair is two atomic RMWs on a cell that is
   thread-local by construction; the mutex exists only so the collector can
   walk other threads' buffers. ~10-20 ns of the ~110, with the stated
   re-entrancy hazard (`tlab_refill` runs under the TLS borrow, so the bump and
   the refill have to be split across it).

3. ~~Cache the per-allocation class-initialisation re-check~~ — **DONE
   2026-08-26.** `jit_new_object` called `ensure_class_initialized_shared` on
   every allocation; `class_init_memo` (a dense per-`ClassId` bitmap, one
   relaxed load and a bit test) already served `getstatic`/`putstatic` and `new`
   never got it. The saving is doubled at this call site because it also skips
   the `jit_thread_mut()` acquire taken purely to have a thread to pass in.
   Measured, ONE binary, 12 ABBA pairs, 24 samples per arm:

   | shape | memo on | memo off |
   |---|---|---|
   | `new Object()` | min **106.4**, p25 120.3, med **125.2** | min 122.4, p25 129.9, med 139.6 |
   | `FtlRate` (the real loop) | min 166.4, med 208.4 | min 163.8, med 204.6 |

   **10-13% on pure allocation and nothing measurable on this page's own loop**,
   where allocation is a smaller share of a ~200 ns operation. Kept because it
   is a strict improvement with a JVMS §5.5 monotonicity argument, and recorded
   here at its real size rather than at the 12.2% its profile share suggested.
   `CRATONVM_JIT=-new-class-init-memo` is the A/B.

4. **Constructor inlining**, unchanged — what lets IR-level escape analysis
   scalar-replace the allocation while keeping the atomic side effect.

## An adjacent decision this reopens, but does not settle

`real-new-site-flags` is off by default, and the stated reason is the neutral
measurement corrected above. On Generational and G1 it is worth 2.9-5.6x
**today**, with no VM change at all.

That is not sufficient to flip it, and this page is not claiming it should be.
`skip_helper` elides `jit_post_tlab_init`, which installs non-zero primitive
`Value` discriminants and registers finalizers; a wrong `has_prim_init` /
`has_finalizer` is silent corruption, not a slow path, which is why the
conservative `(true, true)` existed. What has been run is
`regression-suite/run.sh` across six arms — Generational, G1 and default, flag
on and off — **72 passed, 0 failed in every one**. That is a floor, not a
clearance: the map-view fail-fast attempt passed `cratonvm-native-collections`
136/136 and still killed every Spring Boot class in ~1.5 s. A real clearance
needs the map- and allocation-heavy Spring Boot and netty classes on
Generational with the flag on.

## Ground truth

Unchanged, and re-confirmed on the 2026-08-26 binary. `probes/FtlRate`, default
collector, host load 22-25:

```
ftl ns/op=173.4  fullLoopSec=372   advanced=5000000
ftl ns/op=210.4  fullLoopSec=452   advanced=5000000
ftl ns/op=226.2  fullLoopSec=486   advanced=5000000
```

`advanced` matching the iteration count on every arm is the correctness half
holding. HotSpot on the same host: 8.4-47.5 ns/op, i.e. 18-102 s for the whole
loop.

## The per-call spill, narrowed 2026-08-26

Not this page's allocation cost, but the other half of the same wall, and
recorded here because this is the family page. `emit_pre_safepoint_spill`
blind-copied the whole GPR file — 14 stores — into frame slots at EVERY
GC-capable call; it is now elided where the caller frame is provably oop-clean
(`CRATONVM_JIT_CALL_SPILL_ELISION`, default on), which is **1.4-2.2x on every
shape of compiled call** in `probes/CallArgCostProbe.java`. It does NOT fire on
reference-manipulating frames: 100% of the refusals on both netty exhaustive
loops are the single clause `ref-local-in-reg`, with the operand-stack,
scratch-survivor and moving-young clauses all zero. The next lever there is to
NARROW the spill to the registers that can hold an oop rather than elide it. See
[`../perf/per-call-blind-gpr-spill-20260826.md`](../perf/per-call-blind-gpr-spill-20260826.md).


## Repro

```bash
cd apps/netty-suite-runner
echo io.netty.util.concurrent.FastThreadLocalTest > /tmp/one.txt
CV_BIN=<binary> bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 2400 --out /tmp/repro
```

`probes/FtlRate.java` and `probes/CtorShapeRateProbe.java` are the quick
re-check. **Interleave the arms, and take the MINIMUM, not the mean.** A first
attempt at the item-3 A/B used three interleaved pairs and produced arms that
disagreed *between shapes* — `alloc` said the memo helped, `ftl` said it hurt —
with ~2x spread inside a single arm at load 22-25. Twelve pairs and a
min/p25/median summary resolved it. On this host the mean is a measurement of
who else is on the box.
