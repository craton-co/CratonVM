# FastThreadLocalTest — the inline allocator is not missing a contract, it is starved on the default collector

**Status: OPEN — throughput, not correctness.** The wall is unchanged. What
changed on 2026-08-26 is that **this page's item 1 was aimed at the wrong
thing**, and the real blocker is now measured and specified rather than guessed.
**2026-09-17: that item 1 was itself tried in September and found to be a net
loss** — see the updated item 1 below; do not re-read this page as saying it
is still free money. **Item 2 shipped the same day**, ~5% on the DEFAULT
collector's allocation fast path — see below. **Item 4's premise was also
refuted**, one day earlier (2026-09-16, `docs/JIT_OPTIMIZATION.md`) — escape
analysis is not declining these allocations for the reason this page named;
it is not reaching them at all, for a narrower and still-open reason. See the
updated item 4. What remains open on this page: item 1's real cost
(`jit_post_tlab_init`'s announcement overhead), and whatever the
`JIT_OPTIMIZATION.md` investigation turns up once it has its next counter.

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

*(The snippet above is the 2026-08-26 shape. `VmHeap::Zgc`'s arm now calls
through to `ZgcRealHeap::refill_tlab` — see item 1 below — but the outcome for
the DEFAULT configuration is the same: that path checks
`CRATONVM_ZGC_JIT_TLAB`, which is opt-in and off, so `thread.tlab` still stays
empty unless that flag is set. Everything in this section still describes
default behavior correctly.)*

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

1. ~~**Bridge ZGC to `thread.tlab`, with inline registration.**~~ **TRIED
   2026-09-02/06, and it is a net loss — do not re-attempt this as free
   money.** Built essentially as specified here (`feat/zgc-jit-tlab-20260902`,
   `gc/src/zgc/vm_tlab.rs`), including the inline bitmap registry insert.
   Correctness took several rounds — a stale TLAB skip span hiding a live
   object from the sweep, a second heap publishing `false` over the first
   heap's TLAB-announce flag, `jit_post_tlab_init` stamping an identity hash
   over the mark word — and landed suite-clean 2026-09-06
   (`07c69f93d`). Then it was priced: `BinTreesClassic 16` at `-Xmx512m`,
   interleaved on a quiet host, identical checksums —

   | | run 1 | run 2 | run 3 | run 4 |
   |---|---:|---:|---:|---:|
   | on | 2841 | 3049 | 2746 | 2564 ms |
   | off | 2218 | 2188 | 2172 | 2007 ms |

   **4/4 slower**, and the slow arm ran FEWER collections (1 vs 2), so it is
   the allocation path itself, not collection frequency (`ebe9ad48e`). The
   cause is structural, not a tuning miss: this collector finds objects
   through an allocation-base registry, so every inline allocation must be
   ANNOUNCED, and the only announcement point that exists is
   `jit_post_tlab_init` — a helper that *also* mints an identity hash, looks
   up the class's compact layout and dispatches primitive-field init. The
   registry insert this item predicted as "four arithmetic ops and one `lock
   or`" is real and is exactly that cheap; what it did not predict is that it
   rides along with the rest of that helper's cost on every single object,
   which is what the two-atomics-saved argument above assumed away. Ships as
   `CRATONVM_ZGC_JIT_TLAB`, **opt-in, default off** — correct, and available
   for a future attempt that first splits the announcement out of
   `jit_post_tlab_init`, but not a lever to flip on the strength of this
   page's original argument.

2. ~~**Cheapen `alloc_raw_tlab`'s fast path.**~~ **DONE 2026-09-17.** The
   re-entrancy hazard named here was real for a naive fix and is why this sat
   unimplemented — a `with_attached<R>(f: impl FnOnce(&Mutex<ZArenaTlab>) ->
   R) -> R` replaces `attach() -> Arc<Mutex<ZArenaTlab>>`, running `f` *inside*
   the `ZGC_TLAB_HANDLES` `RefCell` borrow on the cache-hit path instead of
   cloning the `Arc` out of it. That removes both atomic RMWs on every
   allocation — no clone, no drop — while the `Mutex` itself is untouched, so
   `retire_all_tlabs`'s concurrent, non-stopping walk (`walk_objects`, the
   census driver) can still lock a cell another thread is bumping. The hazard
   is that `f` must not re-enter `with_attached`/`attach` for the SAME
   registry id while it runs, which would double-borrow the `RefCell` and
   panic; audited against the only caller (`alloc_tlab`'s tail: a different
   `Mutex`, the arena lock, and the object registry, never this one) and the
   four real `init` closures that reach it (`gc/src/zgc.rs`), none of which
   allocate.

   Correctness: `cargo test --release --features zgc -p cratonvm-gc` (1907
   passed, 0 failed) plus a 16-thread, 32M-allocation churn probe under a
   constrained 128 MiB heap (many `retire_all_tlabs` cycles racing live
   `attach` calls) — three runs, byte-identical checksums.

   **What it is worth.** `TlabAllocRate` (`new Small(i,i+1)` in a tight loop,
   20-30M iterations), ONE binary pair, ten interleaved rounds, Azure host
   `vm1` (load 2.4-3.5): **complete separation** — old 134.1-137.2 ns/op, new
   127.1-129.7 ns/op, i.e. **~5% faster**, smaller than the ~10-20ns estimate
   above because this probe's loop and field-store overhead dilute the pure
   allocation cost the estimate was against. `perf record` on the same binary
   pair confirms the mechanism, not just the clock: `alloc_tlab`'s self-time
   share drops from 21.99% to 16.18% of the profile (now attributed to
   `alloc_tlab::{closure#0}`, i.e. `with_attached`'s `f`), with total sampled
   event count down to match.

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

4. **Constructor inlining** — **the premise was refuted 2026-09-16, one day
   before this correction.** This item assumed escape analysis was declining
   to scalar-replace `new`-then-consumed-locally allocations because a
   constructor's field stores are invisible unless `<init>` is inlined into
   the caller's IR graph. `docs/JIT_OPTIMIZATION.md`'s "Escape analysis offers
   no scalar-replacement candidates" measured that assumption directly, on a
   probe built to be the friendliest possible input (every allocation
   consumed entirely within the method that made it), forced onto the
   optimizing tier: `candidates=0`, and **all thirteen specific refusal
   reasons — including the "every `new` escapes to its own `<init>`" one this
   item's reasoning rests on — read zero.** Escape analysis is not refusing
   these allocations; it is not seeing an allocation to refuse at all
   (`scalar_scope_census`: `analyses=2, allocation-nodes-seen=0`). The method
   that allocates IS admitted to the optimizing pipeline, so the open question
   is narrower and one level earlier than constructor inlining: why the EA
   pass does not run on that method's own graph — an IR-admitted method
   falling back to the single-pass backend inside the pipeline, and an EA
   pass that ran on some other method's graph, would both produce exactly
   this reading, and telling them apart needs one more counter (which method
   each analysis ran on), which is not built yet. There is also a separate,
   unconfirmed lead in the same write-up that hot methods may never promote
   past C1 at all, which would suppress the whole optimizing tier rather than
   escape analysis specifically.

   **Building constructor inlining now would be solving a problem that is not
   established to exist.** The next step this item should have pointed at all
   along is the one counter `docs/JIT_OPTIMIZATION.md` names, not a JIT
   feature the size of what C2 does. Read that page's "Escape analysis" and
   "An open lead" sections before picking this back up.

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

## The per-call spill — elided, narrowed, then de-duplicated; all three spent

Not this page's allocation cost, but the other half of the same wall, and
recorded here because this is the family page. `emit_pre_safepoint_spill`
blind-copied the whole GPR file — 14 stores — into frame slots at EVERY
GC-capable call; it is now elided where the caller frame is provably oop-clean
(`CRATONVM_JIT_CALL_SPILL_ELISION`, default on), which is **1.4-2.2x on every
shape of compiled call** in `probes/CallArgCostProbe.java`. It does NOT fire on
reference-manipulating frames: 100% of the refusals on both netty exhaustive
loops are the single clause `ref-local-in-reg`, with the operand-stack,
scratch-survivor and moving-young clauses all zero.

The two follow-on levers on a refusal — narrowing the spill to the registers
that can hold an oop (`CRATONVM_JIT_SPILL_NARROW`, default ON), then dropping
the register copy of arguments already staged to the frame
(`CRATONVM_JIT_SPILL_ARGS_PUBLISHED`, default ON) — both shipped 2026-08-27 and
both measured against these same two netty exhaustive loops: no separation on
either (`HeaderValidationLoopRate` 652-685 vs 684-718 ns/iter,
`HttpStatusClassLoopRate` 106.5-121.2 vs 105.8-112.1 ns/iter). There is no
fourth lever of this shape. See
[`../../internal/performance/per-call-blind-gpr-spill-RETIRED-20260917.md`](../../internal/performance/per-call-blind-gpr-spill-RETIRED-20260917.md)
for the full record — the remaining gap on this wall is per-iteration cost and
the inlining gap to HotSpot, not the safepoint spill.


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
