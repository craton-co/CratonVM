# FastThreadLocalTest — the OSR door's constructor bind, and why `CRATONVM_JIT_ENABLE_INLINE_NEW` was never going to help

**Status: the constructor half is FIXED 2026-08-17** (branch
`fix/netty-dns-ftl-zgc-20260817`), worth 2.1x on the real loop and permanent.
The allocation half is **not** fixed and now has a named blocker instead of a
mystery. This page supersedes the open questions in
`known-issues/netty/fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`;
see "What is still open" for what that page should be read as after this.

## The two questions that page left, and their answers

### 1. "Why the codegen refuses that shape is unresolved"

It does not refuse the shape. The 2026-08-13 attempt to route a **non-elidable
`invokespecial …<init>()V`** site in the OSR door through the eager-compile +
direct-bind path made `compile_with_param_slots` refuse the enclosing method,
which marks it OSR-denied for the process lifetime, so the hot loop
interpreted forever — `new A()` 311 ns → 1412 ns, the real
`new FastThreadLocal<Boolean>()` 451 ns → 1868 ns. It was reverted and filed
as specific to the `()V` shape.

The cause was **a direct-bound site with no `JitInvokeInfo`**. The codegen's
direct-call arm reads that record to name the callee for the exceptional-return
service, and a bind without one makes it refuse the whole method. That is not
a new discovery — it is written down a few hundred lines below the reverted
code, in the fix for the door's *sibling* admission:

> "an `invokespecial` bind without it makes `compile_with_param_slots` refuse
> the whole method, so the OSR artifact never materialises and the loop
> silently runs interpreted forever"

That fix landed for kind-1 sites arriving through the **scan loop**. `()V`
constructor sites arrive through the **deferred-elidability list** instead,
bypassed it, and so still had none. Routing them into `pending_callee_compiles`
makes them take the same bind, and the `JitInvokeInfo` comes with it.

The lesson is not about constructors. **Two entry paths into one bind, and the
missing-record fix was applied to only one of them** — the same shape as the
`cov-04` finding the page itself quotes ("35 of the compiles this term disabled
had no `new` at all — they were compiled CONSTRUCTORS; it survived here").
When a fix says "sites reaching this bind need X", enumerate every way a site
reaches it.

Measured, Azure Linux, same binary, `CRATONVM_NO_OSR_CTOR_BIND=1` as the
off-arm, interleaved:

| loop (ns/op) | bind OFF | bind ON | HotSpot |
|---|---|---|---|
| `new X()` where `X()` is `i = ATOMIC.getAndIncrement()` | 611-680 | **406-419** | 7.3-7.7 |
| `new X()` where `X()` is `i = ++staticInt` | 380-428 | **173-174** | 2.0-2.3 |
| **`new FastThreadLocal<Boolean>()`, the real class** | 420-495 | **199-266** | 7.3-7.8 |

`InternalThreadLocalMap.lastVariableIndex()` advances by exactly the iteration
count in both arms, and `CTOR-SHAPE-DONE next=24000000 plain=12000000` matches
HotSpot exactly, so the correctness half the 08-12 fix established is
untouched.

`CRATONVM_NO_OSR_CTOR_BIND=1` restores the old dispatch.

### 2. "Measure before implementing this one — the TODO is real but it is not what this loop is paying for"

The page reached that verdict from a clean A/B:
`CRATONVM_JIT_ENABLE_INLINE_NEW=1` moved the allocation rate 5 243 769/s →
5 277 311/s. Re-taken on 2026-08-17: 108.7 → 108.6 ns/op. Flat both times.

**The verdict is right and the reasoning behind it was not**, and the
difference matters because the page used the flat A/B to retire an in-tree
TODO that is still worth doing.

`bytecode_walk`'s admission is

```rust
let skip_helper = !has_prim_init && !has_finalizer;
let can_inline = <not disabled> && (skip_helper || CRATONVM_JIT_ENABLE_INLINE_NEW) && …;
```

The flag forces `can_inline`. It does **not** touch `skip_helper` — so it
swapped a `jit_new_object` call for an inline header write plus a
`jit_post_tlab_init` call. It could not have moved the number. And
`skip_helper` itself was unreachable from two of the three compile doors,
because the interpreter's first-call path and `compile_osr_artifact` both
pushed a literal

```rust
new_info.push((pc_new, target_id.as_u32(), num_fields, true, true));
```

under a comment promising "a follow-up should extract the real flags from class
metadata to enable the skip path" — while `resolve_jit_new_site`, in the same
file, had been computing the real flags all along. So **no OSR-compiled loop
could inline-allocate, whatever the class looked like**, and the flag's A/B
was measuring an arm that never reached the path under test.

That follow-up is now implemented (`jit_new_site_flags`, shared by all three
doors) and it is **off by default**, behind
`CRATONVM_JIT_REAL_NEW_SITE_FLAGS`. Turning it on is neutral:

| | inline arm off | inline arm on |
|---|---|---|
| `new Object()` (3 interleaved rounds) | 111.4 / 127.7 / 116.0 ns | 118.0 / 113.9 / 109.2 ns |

Because — and this is the part no measurement of the flag could have shown —
`emit_inline_tlab_new` **does not allocate inline**. Its own comment:

> "A raw compiled bump updates `Tlab::cursor` without going through the
> allocator's publication protocol. In concurrent Elasticsearch merge churn
> that left a malformed young-space span before the next collection could
> obtain an exact object map. Route through the checked runtime helper until
> the raw JIT path can share the same atomic publication contract … The helper
> retains TLAB allocation (and its fast path); it merely removes the
> unsynchronised machine-code cursor writer."

Only the header writes are inline. `skip_helper` removes a
`jit_post_tlab_init` call and leaves the allocation itself exactly where it
was. Shipping the real flags on a neutral measurement would be the mistake
`measure_the_thing_the_feature_is_for_before_defaulting_it_on` records, so the
lever exists, is documented, and is off — ready for the day the bump is
genuinely inline, which is the day it starts paying.

## A third door problem, found while sizing the remainder: the IR tier cannot emit an intrinsic

The shape breakdown after the constructor bind had one number that did not
add up. On a quiet Windows host:

```
ctorAtomic   255.8 ns/op     ctorPlain 136.1     allocObject 98.4
atomicOnly     5.8 ns/op
```

`ctorAtomic - ctorPlain = 120 ns` for replacing `i = ++staticInt` with
`i = ATOMIC.getAndIncrement()` inside a constructor — while the *same call*
in a bare loop is **5.8 ns**. The atomic intrinsic was measured green and was
not being applied.

`CRATONVM_DBG=intrinsic` named it in one run:

```
[cratonvm-intrinsic] IR body installed (single-pass call-site intrinsics NOT
                     registered) CtorShapeRateProbe$CtorAtomic.<init>()V
```

**Only the single-pass backend can emit a call-site intrinsic.** The
optimizing tier lowers every invoke it admits into a real call — a direct
cross-call for a statically-bound site, a MIC/PIC cascade for a virtual one —
so a method containing an intrinsic site gets *slower* by being optimized.
Nothing in the pipeline notices: the matcher, the codegen ladder and 1991 unit
tests all agree the intrinsic exists.

Confirmed by A/B before writing any code —
`CRATONVM_JIT_IR_CALL_VIRTUAL=0` (which pushes such methods off the IR tier
wholesale) took `ctorAtomic` **509 -> 183 ns/op**, landing it *level with*
`ctorPlain` at 189, which is the signature of `lock xadd` actually being
emitted.

Fixed by refusing the IR tier for a method that contains a call-site intrinsic,
so it falls back to single-pass. Measured, interleaved, same binary
(`CRATONVM_JIT_IR_OVER_INTRINSIC=1` as the off-arm):

| | old | fixed |
|---|---|---|
| `ctorAtomic` | 351.3 ns/op | **165.0 ns/op** |
| `ctorPlain` (control, no intrinsic) | 175.9 | 181.4 |
| `atomicOnly` (control) | 6.7 | 7.5 |

`ctorAtomic` now sits *below* `ctorPlain`, and
`CTOR-SHAPE-DONE next=24000000 plain=12000000` matches HotSpot exactly, so
nothing about the counted side effects moved.

### Two traps this one carried

**The refusal has to ask all THREE resolvers.** The first cut gated on
`try_resolve_intrinsic` alone and measured *zero* — because `AtomicInteger`
and `String` have their own resolvers (`try_resolve_atomic_intrinsic`,
`try_resolve_string_intrinsic`), needing a field layout. So the gate missed
precisely the family that motivated it. Only the engagement trace caught it:
the body was still logging `IR body installed`.

**And it does NOT help the class this investigation was about.**
`FastThreadLocal.<init>` calls `invokestatic
InternalThreadLocalMap.nextVariableIndex()I` — the atomic is one level deeper
— so the constructor legitimately stays on the IR tier and the real loop is
unmoved: interleaved, 279/213/197/204 ns/op before against 279/260/209/221
after, i.e. noise on a loaded host. Kept because it is a real 2.1x on any
constructor whose own body touches an atomic, and because `String` intrinsics
were losing the same way; **not** counted toward the FastThreadLocal gap.

## Where the remaining time goes

`perf record` on `probes/CtorOnly.java` (one loop, so a whole-process profile
is a profile of that shape), Azure Linux, after the constructor bind:

| | share | what |
|---|---|---|
| `ZgcRealHeap::alloc_raw_tlab` | 22.2% | the allocation helper |
| `vm_object::set_static_shared` + `jit_putstatic_object` + `jit_putstatic_class_init_guard` + `get_static_shared` | ~17% | the probe's own `sink = …` static store |
| `jit::helpers::jit_new_object` | 8.5% | the slow-path allocation helper |
| `is_class_initialized_via_manager` + `ensure_class_initialized_shared` | 12.2% | per-allocation class-init re-check, for monotonic state |
| `tlab_refill_wedge_break` | 3.3% | |

Inside `alloc_raw_tlab`, the fast path is not a pointer bump. Per allocation it
does a thread-local lookup, a linear scan of a `Vec<(u64, Arc<Mutex<ZArenaTlab>>)>`,
an `Arc::clone`, a `parking_lot` lock, the bump, an unlock, an `Arc` drop, and
an atomic `fetch_add`:

```
3.73%  lock<parking_lot::RawMutex, ZArenaTlab>
3.43%  drop_glue<Arc<Mutex<ZArenaTlab>>>
3.39%  alloc_tlab
3.26%  attach → try_with<RefCell<Vec<(u64, Arc<Mutex<ZArenaTlab>>)>>> → map
3.00%  fetch_add (register_allocations)
```

The `Arc` clone/drop pair is two atomic RMWs on a cell that is thread-local by
construction; the mutex exists only so the collector can walk other threads'
TLABs. Removing the clone (lock inside the `try_with` closure) is a bounded
~10-20 ns of the ~110, and carries a re-entrancy hazard — `tlab_refill` runs
under that borrow — so it is named here rather than done blind.

## What is still open

The class still cannot finish inside the netty suite's 180 s per-class wall,
and the arithmetic says why: at ~200-235 ns/op the 2 147 483 639 iterations
need ~430-500 s, and **allocation alone is ~110 ns/op = ~236 s**. The
constructor is no longer the gap; the allocation helper is, and it is a helper
on purpose.

Ordered by what it would take:

1. **Give the JIT-emitted TLAB bump the publication contract**
   `Tlab::alloc_initialized` has. This is the single change that makes the
   ~110 ns collapse and makes `CRATONVM_JIT_REAL_NEW_SITE_FLAGS` worth
   defaulting on. It is also the change that caused heap corruption the last
   time it was tried, so it needs the contract, not a revert of the revert.
2. **Cheapen `alloc_raw_tlab`'s fast path** — the `Arc` clone/drop and the
   TLS `Vec` scan, ~10-20 ns, no protocol change.
3. **Cache the per-allocation class-init check**, 12.2% of the profile for a
   monotonic predicate.
4. **Constructor inlining**, which is what lets IR-level escape analysis
   scalar-replace the allocation while keeping the atomic side effect — what
   C2 does here, and why HotSpot's constructor loop outruns its own bare
   atomic loop.

Until 1, the honest description is what the 08-12 page already recommends: a
recorded throughput gap. It is now a **2.1x smaller** one with a specific
blocker rather than an open question, and the 12 other tests in the class
remain unknown for the same reason as before — JUnit runs
`testConstructionWithIndex` in the same fork and the harness kills it at the
wall before any `@@RESULT` is emitted.

## Two traps re-confirmed

* **A flag that cannot reach the code it names measures flat, and flat reads
  as refuted.** Before trusting an A/B on a lever, check that the arm changed
  something — `print what the arm CHANGED beside what it COST`. Here the
  engagement evidence was available (`skip_helper` is a separate term in the
  same boolean) and nobody read it.
* **`disp_calls` going to zero is what both the fix and the breakage look
  like** — kept from the 08-12 page because it stayed true through this round.
  `jit_entries` and wall-clock are the discriminators.

## Repro

```bash
# rate, both arms, one binary
CRATONVM_NO_OSR_CTOR_BIND=1 cratonvm --real-jdk --cp "<netty-cp>" FtlRate
                            cratonvm --real-jdk --cp "<netty-cp>" FtlRate
# shape breakdown
cratonvm --real-jdk --cp "<netty-cp>" CtorShapeRateProbe
# the inline-new lever, now non-inert but neutral
CRATONVM_JIT_REAL_NEW_SITE_FLAGS=1 cratonvm --real-jdk --cp "<netty-cp>" CtorShapeRateProbe
```

Interleave the arms. The Azure host runs at load 20-35 from other sessions and
a non-interleaved comparison there moved every number — including
`atomicOnly`, which no change in this branch can touch — by 2x.
