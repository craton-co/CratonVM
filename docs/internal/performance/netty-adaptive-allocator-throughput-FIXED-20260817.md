# netty's `AdaptivePoolingAllocator` — the RBC.6 refusal is fixed, and what the 196x actually was

| | |
|---|---|
| **Status** | **FIXED / RETIRED 2026-08-17.** The blocker this page named is gone, two further defects its own instruction (profile it) turned up are fixed, and its headline explanatory number is corrected. |
| **Opened** | 2026-08-17, as `known-issues/perf/perf-netty-adaptive-allocator-throughput-20260817.md`, characterised-not-fixed |
| **Closed by** | `perf/netty-adaptive-allocator-20260817` |
| **Acceptance** | `hot_but_stuck_in_interpreter` **1 → 0** on the pcap loop, and `Magazine.allocate` compiles; `probes/Rbc6AllocThrowProbe.java` matches HotSpot in all four arms while its five methods go **5 refused → 0 refused** |

The page asked three things and got three answers, two of which are not what it
expected.

## 1. The blocker — and why clearing `new` alone would have measured nothing

RBC.6 refused `io/netty/buffer/AdaptivePoolingAllocator$Magazine.allocate` with
`reason=rbc6-handler-reads-unsafe-local(pc=338,op=0xbb)`. The page said
admitting `new` without giving its lowering a precise exceptional frame is a
miscompile, and it was right; it also warned, in the same paragraph, that RBC.6
bails on the FIRST unadmitted opcode, so **list every one inside the method's
protected ranges before pricing the work**.

That warning was load-bearing. Scanning all four protected ranges of
`Magazine.allocate` — `[179,197)`, `[213,215)`, `[319,383)`, `[401,403)` —
leaves exactly two unadmitted opcodes, and they are seven bytes apart:

```
338: new           <-- the reported refusal
341: dup
342: invokespecial
345: athrow        <-- where the refusal would have moved
```

`throw new X(...)` is one four-bytecode sequence. Admitting `new` on its own
would have re-run the compile, hit the `athrow`, refused again, and reported a
clean null result. Both lowerings therefore grew a publishing exit in the same
change:

* **`new` (0xbb).** Every non-scalar-replaced arm — the inline-TLAB fast path
  (whose slow edge falls into `jit_new_object`), the resolved helper stub, and
  the DEFERRED constant-pool stub for a class not yet loaded at compile time —
  funnels through `emit_post_alloc_oom_check`. That guard branched to the
  SHARED sentinel-only stub, which records nothing at the bci. It now records a
  reason-9 frame whenever the pc is protected, exactly as
  `emit_post_invoke_exception_check` does. All three ways the site can raise —
  a `<clinit>` failure, a resolution failure on the deferred arm, heap
  exhaustion — are reported through the same `0`/null sentinel the guard
  already tested for, so one publishing exit covers the lot. The
  scalar-replaced arm emits no call, raises nothing, and needs nothing.
* **`athrow` (0xbf).** Its lowering called `jit_throw_exception` — which had
  already stashed the exception and this athrow's own bci — and then ran the
  epilogue directly. Inside a protected range it now jumps to the reason-9
  stub, which spills the trapping registers, materializes the frame, and runs
  the same epilogue.

`CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW=1` withdraws the admission — not the
exits, which are correct for the opcodes already admitted around them — so one
binary A/Bs against itself.

### The acceptance test is a differential, not a green

`probes/Rbc6AllocThrowProbe.java` is the `new`/`athrow` sibling of
`Rbc6FieldProbe`: five methods with a non-parameter local written inside a
`try` and read by the handler, 200 000 iterations, handler taken every eighth.
Its checksum matches HotSpot in all four arms — HotSpot, CratonVM JIT, CratonVM
JIT with the admission withdrawn, and `--nojit`.

On its own that proves nothing, because a refused method is an interpreted
method and an interpreted method is correct. What makes it non-vacuous is
`jit-method-stats` printed beside it:

| arm | `hot_but_stuck_in_interpreter` | interpreted invocations |
|---|---:|---:|
| admission withdrawn | **5** — all five methods, every one `op=0xbb` | 801 972 |
| admission on | **0** | 2 820 |

Same answers out, five methods compiled instead of none.

**Read that alongside one thing this work found and did not fix.** The sibling
probe `Rbc6FieldProbe.java` — the acceptance test named in
`precise_field_ops_enabled`'s own doc — was run here as a regression control
and is **RED on this tree, and was red at the branch point**: under JIT, at
200 000 iterations, it lets a `NullPointerException` escape a handler that
catches it, and `CRATONVM_JIT_NO_PRECISE_FIELD_OPS=1` makes it correct in one
run. That is filed as
`known-issues/jit/rbc6-getfield-putfield-admission-lets-an-npe-escape-its-handler-20260818`.
It is a different admission through different emitters, and the `new`/`athrow`
half above is validated on its own probe — but nobody should read "RBC.6
admitted two more opcodes" as "the RBC.6 admission family is healthy". It is
not, and the failing member is the one that has been shipping since 2026-08-02.

On the pcap loop the same counter moves 1 → 0 and the tracked interpreted
invocation count falls from 137 643 to 90 733.

## 2. What profiling the loop actually named

The page's instruction was to start from measurements. `perf record` on the
reduced loop (`../../../apps/probes-adalloc/PcapThroughput.java`, now in-tree — the netty
checkout and its suite runner are host-side, the probes are not) put the largest single symbol somewhere the page had not looked:

| symbol | self |
|---|---:|
| `G1Collector::is_addr_in_live_region` | **9.10%** |
| `interpreter::execute_frame_from_index` | 6.89% |
| `__memcmp_evex_movbe` | 2.40% |
| `execute_invokevirtual_cached` | 2.28% |
| the mimalloc trio (`_mi_page_malloc_zero`, `mi_free`, `mi_theap_malloc_aligned`) | 3.03% |
| `NativeMethodRegistry::find` + `find_with_kind` + `slot_for_exact` | 2.84% |

Two of those rows are defects, and both are shapes this tree has closed before.

### 2a. G1 took the global regions mutex for every in-arena address

`is_addr_in_live_region` rejects an out-of-arena address lock-free, and that
gate is exactly why the conservative stack scan is affordable. Every address
that survives it — that is, **every real object pointer** — then took
`regions.lock()`. The callers are not rare: `is_object_address` runs it per
candidate word of every JIT frame scan and once per object-shaped argument of
every native call (`pin_value_for_native_call`). On this workload the stacks
are full of genuine `ByteBuf` pointers, so the arena gate rejects almost
nothing and the mutex sits on the per-call path. The inverted profile is
explicit: **2.91% of the whole run is the `MutexGuard` drop inside that one
function.**

This is the third instance of one pattern, after `G1::needs_gc` scanning the
region table under this mutex per allocation and every G1 accessor taking it to
ask `humongous_span`. Both of those were fixed on 2026-08-17; this one was
still open.

The fix is a per-thread POSITIVE memo which answers only "yes":

* An entry records `[base, limit)` where `limit` was `base + cursor` at fill
  time. A cursor only GROWS within an incarnation, so a memoized limit is an
  UNDER-approximation: an address past it misses and takes the authoritative
  path, which refreshes the entry. The memo cannot accept an address the lock
  would have rejected on cursor grounds.
* Recycling or retyping a region is exactly what `rset_cache_epoch` already
  counts — bumped under the regions lock, with `Release`, at the start of
  `young_collection`, `mixed_collection` and `cleanup`. An entry carries the
  epoch read BEFORE its fill took the lock, so a recycle racing that fill
  leaves the entry stamped with an already-superseded epoch. **This adds no new
  maintenance obligation:** `post_write_barrier_rset`'s TLS cache already leans
  on the same counter against the same hazard.
* Entries are keyed on the collector's minted `instance_id`, so a collector
  constructed at a dropped one's address cannot inherit them — the failure that
  field exists for.
* The evacuation-failure arm (`kept_unresolved_*`) is deliberately excluded:
  liveness there is per-ADDRESS set membership, which a span cannot express.

Kill switch `CRATONVM_G1_NO_LIVE_REGION_MEMO=1`. Engagement counter
`CRATONVM_DBG_G1_LIVE_MEMO=1`, where a hit is one `regions.lock()` that did not
happen. On 3 000 pcap iterations it reads:

```
[g1-live-memo] FINAL hit=42223615 miss=553301 hit_rate=98.7%
```

**42.2 million mutex acquire/release pairs removed from one 3 000-iteration
run** — about 14 000 per iteration. Read that second number twice: it is not a
statement about the memo, it is a statement about how often this VM asks
"is this address a live object?" at all, and it is the larger residual (§4).

Do not time a run with that counter on. It is 42 M contended atomic increments
that the OFF arm does not pay, which makes the instrument a variable of its own
comparison — the same trap as timing a build with one arm instrumented. The
load-independent measurement is the profile share, taken with the counter off,
`-F 199 -g`, 4 000 iterations:

| symbol | memo OFF | memo ON |
|---|---:|---:|
| `is_addr_in_live_region` | **11.64%** | 6.32% |
| `is_object_address` | 1.43% | 4.43% |
| **the pair** | **13.07%** | **10.75%** |

Both rows have to be read together: with the memo on, the early return inlines
back into its caller, so part of what leaves one row reappears in the other.
The pair is the honest figure — **-2.3 percentage points of the whole
process**, or about a sixth of what that pair used to cost.

### 2b. The interpreted `new` re-resolved its class on every execution

`Instruction::New` did all of this per allocation: a `class_manager` read plus
`get_class_name(cp_index).to_string()` — **a fresh `String` for every `new`** —
a full `resolve_class_loader_aware`, a second `class_manager` read for the
JVMS §5.4.4 access check, an `ensure_class_initialized_shared` (a third; its
own "fast path" still takes that lock), and a fourth for `num_total_fields`.
That is the `__memcmp` row, the hashbrown row and most of the mimalloc row.

The resolved-constant-pool machinery for exactly this already existed
(`interpreter/site_cache.rs` — per-thread, direct-mapped, validated by the
class-definition and resolution epochs) and had a field arm and a method arm.
It had no class arm. It does now.

Unlike its two siblings this one is **default-ON**, for a workload-shape reason
rather than a sizing one. The field arm is off because its hit rate does not
generalise and a miss there costs only a revalidation. A `new`-site miss costs a
`String` allocation plus a class resolution plus four lock acquisitions. Only
sites whose referencing class has **no loader namespace** are ever filled — a
property immutable per class, which is what lets the hit path skip the
re-check. A hit also skips the access check (a function of two class
identities, neither of which changes once defined) and the initialization check
(monotonic), and an entry is filled only from a class that is actually
`Initialized`, not merely one for which `ensure_class_initialized_shared`
answered `Ok`: those two differ inside a recursive `<clinit>`, and an entry
filled in that window could otherwise allocate past a class that went
Erroneous.

Kill switch `CRATONVM_JIT_NO_NEW_SITE_CACHE=1`. `CRATONVM_DBG=field-site` now
reports a `new:` arm beside the other two, and on the pcap loop it reads
`new: hit=5269 miss=1651 fill=1651 reject_loader=0` — a 76% hit rate.

**That count is also this fix's own limit, and it is worth stating plainly.**
Once §1 lets `Magazine.allocate` compile, only about seven thousand `new`s in
the whole run are still interpreted, because a compiled `new` uses the JIT's
own inline-TLAB path and never reaches this arm. On THIS workload the cache is
therefore a small effect that the host cannot resolve. Its reach is on
class-loading-heavy and interpreter-heavy work, where the same counter will
report a much larger population; it is landed here because the defect is real
and the profile named it, not because this loop is where it pays.

It is also withdrawn automatically whenever `CRATONVM_DBG_H2TRACE`,
`CRATONVM_DBG_LOADER_TRACE` or `CRATONVM_NSEE_TRACE` is armed: a hit skips the
class-name derivation all three print from, and an instrument that silently
reports a SUBSET of the sites it is asked about is worse than none.

## 3. The correction — `new Object()` was never 674 ns of allocation

The page's closing argument rested on one number: "`new Object()` measures
674 ns interpreted against HotSpot's 10 ns, and the pcap loop runs at about
that ratio, which is the number to explain."

**That rung had no control.** `../../../apps/probes-adalloc/AllocFloor.java` carries one: the
same loop, the same `getstatic`, the same array store, and no allocation.

| rung | HotSpot | CratonVM JIT | CratonVM `--nojit` |
|---|---:|---:|---:|
| control — loop and array store, no allocation | 3.0 | **193.8** | **665.2** |
| `new Object()` | 1.7 | 265.3 | 1206.6 |
| `new Small(i)` — 3 fields | 1.7 | 312.6 | 2125.7 |
| `new byte[24]` | 2.3 | 221.1 | 756.4 |

ns/op, steady state, 200 000 iterations, every rung warmed before any is timed.

Subtract the control and the marginal cost of an allocation is **~71 ns
compiled and ~540 ns interpreted** — not 674. On the compiled rung the
allocation is **27%** of what the page attributed to it; the other 73% is the
loop, which is ordinary bytecode plus a `getstatic` that resolves per
iteration.

That is not a footnote, it is the difference between two programmes of work.
"Allocation costs 674 ns" says go and fix the allocator. The controlled numbers
say the allocator is a minority of even the allocating rung, and the cost is in
what surrounds every operation — which is what the flat profile in §2 shows,
and what the page's own "ceiling, measured" section was already circling when
it wrote that whatever is spent per allocation is spent in roughly the same way
compiled or not.

**A per-iteration figure without a control rung cannot separate the operation
from the loop.** All three fixes above are in what surrounds the operation;
none of them is in the allocator.

## 4. What is left, and how to measure it here

The reduced loop still runs far off HotSpot and this page is retired without
closing that. What it no longer contains is a mystery:

* The profile is **flat**. After 2a the largest symbol is under 7%, and the
  residue is interpreter dispatch, invoke dispatch and native dispatch spread
  over dozens of rows. That population belongs to
  `a-compiled-call-goes-out-to-rust-two-causes-20260817`, which states both of
  its causes as counts and names the codegen each needs.
* `perf-bintrees-9x-gap-characterised` is the same question on a different
  workload and stays open on its own terms.

* The call VOLUME behind §2a — **~14 000 `is_object_address` queries per pcap
  iteration** — is untouched by this work and is the bigger of the two
  quantities. The memo made each query cheaper; nothing here asked why there
  are that many. That is the next question on this workload, and it is a
  question about the conservative root scan and the native-call pin path, not
  about G1.

**A note on this host, for whoever measures next — this is the reusable part.**

The shared Azure box has **no PMU**: `perf stat -e instructions` answers
`<not supported>`, so the obvious load-independent instrument is unavailable.
Its load average moved between 6 and 33 during this work.

A first 5-round interleaved wall-clock A/B looked clean — ON ahead in 4 of 5
rounds, 1243 → 1144 µs/iteration, **-8.0%**. A later 24-run set with the SAME
configuration measured twice per round says that number was luck: the two
identical `all-on` arms within one round differed by up to **1.6x**, and across
rounds the same arm spread **1.9x** (655 to 1223 µs). **The instrument's
repeatability is larger than every effect on this page.** The -8.0% is not
reported above as a result, and should not be quoted as one.

That is why every claim here is a **count**, a **profile share**, or a
**controlled ratio**, and why each fix ships with the counter that says whether
it fired: `hot_but_stuck_in_interpreter`, `new: hit/miss/fill`,
`[g1-live-memo] hit/miss`. An inert change reads as `hit=0` in a second,
whatever the clock says. Size anything here with those first, and do not open a
wall-clock A/B on this box for anything under about 2x.

## Repro

```bash
# CP comes from the host-side netty suite runner; the probes are in this repo.
CP="$(sed -n 2p <netty-suite-runner>/common.args)"
javac -nowarn -cp "$CP" -d /tmp/probes probes-adalloc/*.java

# the loop, and the three kill switches (all default-ON, all A/B-able on ONE binary)
<cv-bin> --java-home "$JDK" --Xmx 1500m -XX:+UseG1GC -Diters=3000 \
    -cp "/tmp/probes:$CP" PcapThroughput
CRATONVM_DBG=jit-method-stats <cv-bin> ...    # hot_but_stuck_in_interpreter
CRATONVM_DBG_FIELD_SITE=1     <cv-bin> ...    # new: hit / miss / fill
CRATONVM_DBG_G1_LIVE_MEMO=1   <cv-bin> ...    # regions.lock() calls avoided

# the RBC.6 acceptance differential — read jit-method-stats beside the checksum
javac -d /tmp/probes probes/Rbc6AllocThrowProbe.java
<cv-bin> ... -cp /tmp/probes Rbc6AllocThrowProbe 200000
CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW=1 <cv-bin> ... Rbc6AllocThrowProbe 200000

# the controlled allocation floor
<cv-bin> ... -cp /tmp/probes AllocFloor 200000 3
```
