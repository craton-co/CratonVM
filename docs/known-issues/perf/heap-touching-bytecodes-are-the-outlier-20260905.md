# Everything the interpreter does is ~10x, except what touches an object header

| | |
|---|---|
| **Status** | OPEN — a ranked list with one item taken and measured; the rest is unbuilt |
| **Severity** | low — no test fails on this |
| **Opened** | 2026-09-05 |
| **Sibling** | `interpreted-invoke-cost-350ns-20260825.md`, whose four passes are what made this shape visible |

The invoke page closed its third pass saying `invokestatic` and `invokespecial`
were "the two worst interpreted call shapes by a wide margin" and named them as
the next target. They were taken, and the result is that **no call shape is an
outlier any more**. Re-measuring the whole operation table on current dev turns
up a different partition, and it is a much cleaner one.

## The measurement

Windows 11, 24C/32T, host load ~43%, JDK 25.0.3, `cratonvm --nojit` against
`java -Xint`, min-of-5, arms interleaved in both directions inside one process,
each probe carrying its own control arm. `probes/ElemShape.java` is new; its
control has the same bytecode **count** as its array arms and touches only
locals, so the difference is the element access and nothing else.

| operation | CratonVM | HotSpot `-Xint` | ratio |
|---|---:|---:|---:|
| one bytecode, straight-line | 7.1 ns | 0.69 | 10.2x |
| tight loop iteration | 53.8 | 5.50 | 9.8x |
| `invokestatic`, 0 args | 132.6 | 14.5 | 9.1x |
| `invokestatic`, 4 args | 181.5 | 21.2 | 8.6x |
| `invokevirtual` | 183.2 | 17.1 | 10.7x |
| fixed target (private / special) | 171.9 | 15.7 | 11.0x |
| `invokeinterface`, inherited | 183.0 | 18.4 | 9.9x |
| each extra `int` argument | 12.2 | 1.67 | 7.3x |
| **`getfield` + `putfield`, own** | **56.0** | **1.10** | **51x** |
| **`getfield` + `putfield`, inherited** | **53.2** | **1.37** | **39x** |
| **`getstatic` + `putstatic`** | **86.4** | **3.45** | **25x** |
| **`iaload`** | **39.9** | **1.68** | **24x** |
| **`iastore`** | **33.9** | **1.09** | **31x** |
| **`arraylength`** | **24.4** | **0.39** | **62x** |
| **`aaload`** (over `iaload`+`ifne`) | **+47.1** | **−0.8** | — |

Two populations, and nothing in between. Everything that crosses a method
boundary, branches, or does arithmetic is inside one 7–11x band. Everything that
**dereferences an object header** is 24–62x.

`arraylength` is the one to read first: no resolution, no cache, no barrier, no
allocation — it reads one word out of a header and pushes an int, and it costs
three and a half straight-line bytecodes' worth of time to do it.

`aaload` is the worst, and the precise statement matters. It **is** in the
dispatch loop's `0x2e..=0x35` arm — but that arm's quickened half,
`field_fast::array_load_prim`, declines it: `prim_elem_for_opcode` has no
reference entry, so every `aaload` falls through to the full barrier-aware
`VmHeap::get_array_element`. It costs 47 ns more than an `iaload` + `ifne`
doing the same work, where HotSpot has the two identical to within noise.

The same arm was worth reading for a second reason: it opened with two
`pop_unchecked()` calls, decoding both operands into the 16-byte `Value` enum
*before* it could offer them to the quickened path, and pushed both wide values
back on the fall-through. Every array element load paid that, `iaload`
included. **Correction to an earlier draft of this page:** the `*astore` arm
(`0x4f..=0x56`) does *not* have the defect — it already peeked the raw slots
and decoded only on the decline. It was one arm out of step with its
neighbour, not an opcode family. Fixed by giving the load arm the store arm's
shape.

## The item that was taken: the registry probe

`field_ptr_for` and `prim_elem_ptr` both opened with
`ZgcRealHeap::is_object_address`, i.e. `ZObjectStartBits::contains`: one
`Acquire` load of a word of the object-start bitmap, indexed by the receiver's
own address. (One load on the hit path, not two — the `overflow_len` read beside
it is only reached when the bitmap says no. An earlier draft of this page said
two and was wrong.)

**The handler those arms replace does not make that test.**
`ZgcRealHeap::get_field` is `let header = self.header(obj)` — a bare pointer cast
— followed by `check_field_index` against that header; `set_field` is the same.
What establishes that the site's recorded offset is legal for *this* receiver is
the triple compare immediately after the probe — `class_id`, `num_slots`, compact
flag — and for arrays the `kind` / `element_type` / `array_length` tests. Those
run unchanged, and a mismatch still falls back to the full handler.

The one thing the probe bought incidentally was a stale-receiver screen: a
relocated address is pruned from the registry, so it missed and fell back to
`op_getfield`, whose `load_and_forward` heals the receiver. That heal exists for
a window these arms do not have — `op_getfield` pops the receiver into a bare
Rust local and *then* calls `resolve_field_ref`, which can load a class,
allocate and provoke a collection while the local is invisible to the root scan
(GCBARRIER-CDLWAIT-FIX, 2026-07-17). Between `peek_compact` and the header read
the quickened arm allocates nothing, takes no lock and calls nothing that can
reach a safepoint, so its window is zero-length.

Kill switch: `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE=1` (`CRATONVM_JIT=-field-addr-elide`).

### Engagement first, and this one needed a trick

The switch changes a branch and nothing observable, so neither census nor clock
could say whether it reached the code — and a null result from an unengaged
switch is the failure mode this page's sibling documents twice.

`CRATONVM_ZGC_STARTBITS=0` swaps the object-start registry from the bitmap to a
`Mutex<FxHashSet>`, which makes every `contains` take a lock. That turns
engagement into a 2x2 with a prediction in each cell. `probes/FieldBurn.java`,
N = 30 M (60 M instance field accesses), wall ms:

| | bitmap registry | mutex registry | Δ |
|---|---:|---:|---:|
| elide (default) | 3617 | 3700 | +83 |
| probe restored | 4023 | 4460 | **+437** |

**The registry implementation only matters in the arm that still probes it.**
That is engagement, and it is not inferable from any counter in the tree.

### The A/B

`probes/FieldBurn.java` at N = 30 M, ten interleaved passes, alternating order,
default bitmap registry, host load ~34%. `ctl` is the same loop over locals —
the arm the switch cannot reach.

| pass | elide field | probe field | elide ctl | probe ctl |
|---|---:|---:|---:|---:|
| 1 | 3686 | 4026 | 2281 | 5203 |
| 2 | 3935 | 7957 | 2106 | 2129 |
| 3 | 3584 | 4586 | 2211 | 2217 |
| 4 | 3544 | 4072 | 2005 | 2489 |
| 5 | 3179 | 3099 | 1657 | 1698 |
| 6 | 3155 | 2937 | 1836 | 1827 |
| 7 | 2911 | 3495 | 1920 | 1886 |
| 8 | 3055 | 3313 | 1883 | 1880 |
| 9 | 3074 | 3426 | 1862 | 1856 |
| 10 | 2833 | 3018 | 1702 | 1689 |

* **field: 8/10 pairwise** in favour of the elide.
* **ctl: 5/10** — the coin flip an honest control has to give.
* min-of-10: field 2833 against 2937 (**104 ms** over 60 M accesses); ctl 1657
  against 1689 (32 ms, the noise floor). Net ≈ **1–2 ns per instance field
  access**, and the quiet tail (passes 5–10, where `ctl` is stable) puts it
  nearer 4 ns with two passes going the other way.

**This is a weak positive, and it is recorded as one.** It is not the
4/4-no-overlap standard the sibling page holds its own claims to. The change is
kept on the grounds that earned the sibling page's unmeasured deletions: it
*removes* work, restores parity with the handler it stands in for, adds no cache
and no invalidation, and its engagement is proven independently of the clock.

### The hypothesis this refutes

The elide was proposed on the theory that the bitmap is sized by the heap
(~16 MB for 1 GiB) so the probe is a random access that misses once the working
set is real. **That is wrong, and the reason is worth keeping.**

`probes/AddrProbe.java` walks receivers scattered across live sets of 4 K, 256 K
and 2 M objects. CratonVM's `getfield` delta stays flat at ~50–90 ns across all
three, while HotSpot's climbs 0 → 98 → 111 ns. HotSpot feels the header miss;
CratonVM does not — **its own per-bytecode cost is large enough to hide a cache
miss behind it**. The bitmap word and the header are also both derived from the
same pointer, so the two loads issue in parallel and the second miss largely
overlaps the first.

So the elide is worth one load, everywhere, and never more. A corollary worth
carrying to the rest of this page's list: *no* interpreter change here should be
justified by a cache-locality argument until the per-bytecode floor comes down.

### Correctness

`regression-suite/run.sh` against the branch binary, elide on (the new
default): **90 of 90 scheduled vectors passed, 0 failed**, no list/coverage
errors and no harness-blindness flags. That suite is a HotSpot differential —
a vector passes only when CratonVM's output matches the oracle's — so it is
the right gate for a change that alters how a field is read.

**Final gates, whole branch.** `regression-suite/run.sh` on the finished
binary: **90/90 on the default collector and 90/90 under
`--XX:UseGc G1`**. And `cratonvm-difftest gate --corpus difftest/seeds`:
**clean, exit 0**, across `jit-on`, `nojit` and `interp-decoded`. That last
axis is the one that matters most here: the interpreter has two
implementations of every opcode, `--noverify` selects between them wholesale,
and this branch added two new fast arms — `interp-decoded` is the axis built
for exactly that failure, and it is the only mode that runs the decoded path.

**Once per collector, because one of these changes is not in the interpreter
at all.** The autobox latch below touches `G1Collector::get_array_element`,
and every run above used the default collector, which would never have
executed it. `CRATONVM_ARGS="--XX:UseGc G1"`: **90 of 90 passed, 0 failed.**
A change to a collector that is never run under that collector is untested no
matter how green the default suite is.

Two harness notes for whoever repeats it, because the first attempt was
worthless and did not look it:

* `JDK=` must be a **Windows** path. A POSIX one is accepted by the launcher
  and then rejected behind it, and all 90 vectors fail identically with
  `HARNESS FAULT — VM REJECTED THE JDK IMAGE`. Use
  `JDK=$(cygpath -m "$(dirname "$(dirname "$(command -v javap)")")")`. The
  harness diagnoses this itself in its own footer; read the footer.
* Do not pipe `run.sh` into `tail`. The pipeline reports `tail`'s exit status,
  so a 0-of-90 run exits 0 and reads as success.

## Two corrections to the tree

* **`field_fast.rs`'s module doc is stale.** It opens by explaining that
  "essentially every object the interpreter allocates is legacy" because
  `ZgcRealHeap::try_alloc_object` never set the compact shape.
  `compact_tlab_alloc_enabled` has been **default-ON since 2026-09-03**. The two
  body arms are still needed; the reasoning printed above them no longer
  describes the default configuration, and it is the reasoning a reader uses to
  decide what to touch.
* **Object body shape does not move field cost.** `probes/FieldShape.java` at
  250 k x 4, three interleaved passes, one binary,
  `CRATONVM_COMPACT_TLAB_ALLOC=0` against the default: own-field pair 122 / 120 /
  117 ns compact against 124 / 126 / 133 legacy, with the control arm moving as
  much as the difference. No separation. The compact TLAB shape bought memory —
  87.5 MB on `TestCache`, as its own note claims — and it did not buy field
  throughput.

## What is left, ranked by evidence

1. **`aaload` never reaches the quickened path** (+47 ns over the equivalent
   `iaload`, against HotSpot's −0.8). It enters the `0x2e..=0x35` arm and is
   declined by `array_load_prim`, so it always takes the full
   `get_array_element`. This is the largest item on the page and nothing has
   been built for it. A reference element needs the load barrier that
   `getfield`'s reference arm already performs inline
   (`load_and_forward` + the autobox latch), which is the shape to copy.
2. **Seven or eight per-access gate loads to read one field.** Counted through
   `getfield_fast_keyed`: `any_field_watchpoint_active`, `any_class_redefined`,
   `class_definition_epoch` and `resolution_epoch` (the last two inside
   `SiteCache::get`, on every hit), `site_stats::on()`, and
   `vacated_frames_enabled` inside `check_vacated_compact` on the push. The fix is
   the pattern this file already uses a dozen times — hoist them into the
   per-`execute_frame` word beside `fast_field_zgc`, with the same
   "observed at frame entry" contract.
3. **`arraylength` takes the `Value` round trip.** The `0xbe` arm pops into the
   16-byte `Value` enum, matches, and on the non-object path pushes it back —
   which keeps it live across the arm — then goes through the `VmHeap` enum
   `dispatch!`. For one header word. `peek_compact().as_object_ptr()` + a direct
   `header.shape` read + `push_int_unchecked` is the whole opcode.
4. **`getstatic` / `putstatic` at 25x** were not investigated. They do not go
   through `field_ptr_for` and were the internal control for this pass.

## The autobox latch: right fix, wrong cause, and it measured nothing

Recorded in full because the negative is the useful part, and because it is the
**second** time on this page that eliding an `is_object_address` probe has come
back at zero.

`crate::autobox`'s module note explains that the latch exists so a read does not
pay "an unconditional `is_object_address` probe plus a header read on EVERY
compact reference-field read". The FIELD paths took that advice;
`ZgcRealHeap::get_array_element` (both arms) and `G1Collector::get_array_element`
did not, and called `autobox_payload` — whose first statement is that probe —
on every non-null `aaload`. Neither file mentioned `wrapper_exists` anywhere.

Latching them is provably answer-preserving (`autobox_payload` returns `Some`
only for `AUTOBOX_CLASS_ID`, which cannot exist unless something set the latch)
and it deletes real work, so it is kept. **It measured nothing:**
`probes/ArrBurn.java`, 30 M iterations, quiet host (`ctl` 1661–1873 ms
throughout, `iaload` 2326–2556):

| | min-of-8 | pairwise |
|---|---:|---|
| latch (default) | 2949 ms | — |
| probe restored | 2969 ms | **3/8** |

3/8 is worse than a coin flip. 20 ms in 2960 over 30 M reads is below the noise
floor, and it is the same answer finding 01 got about the same probe for the
same reason: the bitmap word is hot, and the probe is worth 1–2 ns.

**What the probe that found this out then said instead.** `ArrBurn` runs
`aaload` and `iaload` in identical loop shapes over 1024-element arrays:
**99.9 ns per iteration against 79.0**. They differ in exactly one way —
`iaload` is served by `array_load_prim`, `aaload` falls through to
`VmHeap::get_array_element`, because `prim_elem_for_opcode` has no reference
entry. So the 21 ns is the whole slow path (an enum `dispatch!`, the
collector's own header and bounds re-reads, an `Acquire` load for the barrier
arm, `read_prim_element`'s match, and a 16-byte `Value` returned to be
re-encoded into an 8-byte slot), not one probe. `field_fast::array_load_ref`
is the arm that follows from that reading.

**The rule this page is now willing to state.** Two probe-elision hypotheses,
two zeroes. Do not propose a third on locality grounds. The per-bytecode floor
is high enough to hide these loads, and until it comes down the levers that
pay are the ones that delete a *path* — a dispatch, a representation
conversion, a re-read — not the ones that delete a load.

## The latch that was armed at bootstrap, and the two arms it had killed

This is the largest finding on the page and it was produced by a counter, not
a clock. It also corrects the section above it.

### What the census said

`field_fast::array_load_ref` was written to serve `aaload`. Its first census
run, `probes/ArrBurn.java` at 2 M iterations:

```
arraylength: hit=2000374 miss=0 | aaload: hit=0 miss=2000146
```

**It never fired once.** The A/B that was queued behind it would have reported a
clean zero, indistinguishable from an arm that does not exist — and the same
instrument had already been the only thing standing between this branch and a
wrong conclusion once before.

Naming the cause took two more splits, and reproducing the same defect at each
level is worth recording: `miss` folded three refusals into one number, and
after splitting it, `miss_screen` folded two process-wide latches into one.
**The rule that a skip census must not fold "never a candidate" into a refusal
reason has to be applied at every level of the refusal, not once.** Reasoning
did not settle it either — the hypothesis was right, but a reference-field probe
appeared to contradict it (its receiver was legacy-layout and never reached the
compact arm), and only the split counter was decisive:

```
aaload: hit=0 miss_barrier=0 miss_wrapper=2000146 miss_shape=0 miss_word=0
```

### The premise is false

`autobox::wrapper_exists()` is **true in every process**. The class-mirror
populator puns a `ClassId` (and `Int(-1)` for primitive mirrors) into slot 0 of
an object stamped `java/lang/Class`; that store goes through
`box_for_reference_slot`, which arms the latch unconditionally, at bootstrap,
before any application code runs. The latch's own module note justifies it on
the premise that "a process that never boxes — the overwhelming majority — pays
one relaxed load ... and never touches the address validator". There is no such
process here.

Three fast paths were screened on it, and all three were dead:

| | |
|---|---|
| `array_load_ref` | never fired; added on this branch |
| the array autobox latch | a permanent no-op — **the complete explanation for its measuring exactly nothing** |
| the compact `getfield` reference arm | **pre-existing**, and has never served a non-null reference field on a compact object since it was written |

### The fix, and what it is not

The screen asked the wrong question. "Has anything ever boxed" is a
process-wide latch; what a reader needs is "is the value I just loaded a
wrapper", which is one header compare on an object it already holds.
`autobox::header_is_wrapper` is that test — the same discriminator
`autobox_payload` applies, minus the `is_object_address` probe that finding 01
established is not needed for parity. Semantics are unchanged: a wrapper still
declines to the handler that un-boxes it. What changes is that everything which
is *not* a wrapper — every reference value in an ordinary program — reaches the
quickened path instead of being turned away by a latch about something else.

After it, `aaload: hit=2000146` and every miss counter zero.

### The measurements, on a host at 1% load

`probes/ArrBurn.java`, N = 30 M, eight interleaved passes, arms alternated per
pass, wall ms, min-of-8.

**`aaload`** — `CRATONVM_JIT=-ref-array-fast`. `iaload` and `ctl` are the arms
the switch cannot reach.

| | min-of-8 | pairwise |
|---|---:|---|
| quickened | 2685 ms | — |
| general path | 3222 ms | **8/8** |

**537 ms over 30 M loads ≈ 18 ns per `aaload`**, and it halves the gap to
`iaload` in the same run (35 ns → 16 ns).

**`arraylength`** — `CRATONVM_JIT=-arraylength-fast`, engaged at 100%.

| | min-of-8 | pairwise |
|---|---:|---|
| quickened | 1890 ms | — |
| general path | 2032 ms | **8/8, no overlap** |

**142 ms ≈ 4.7 ns per `arraylength`**, with the control flat at 1774–1855 ms in
both arms. The quickened arm's worst pass (1944) beats the general path's best
(2032). This page predicted "around one nanosecond, below what this host
resolves"; that was wrong by a factor of five, in the conservative direction.

## The 10x floor: both structural proposals refuted, by measurement

The audit that opened this page named one structural change as "the only one on
this page that attacks the 10x rather than the outliers": dispatch on the
pre-decoded `QuickenedCode` stream instead of on raw bytes, making
superinstruction fusion a build-time rewrite and branch targets resolved
indices. It also named the eight opcodes with no fast arm as a consistency gap
worth closing. **Both are wrong, and the numbers are cheap to reproduce.**

### The pre-decoded stream is 2.2x SLOWER than the raw-byte match

`--noverify` flips `use_fast_path`, which switches every arm at once from the
155-arm raw-byte match to the decoded path — and the decoded path already runs
on the `QuickenedCode` stream. So the two engines can be priced directly, on
the same program, in one binary. `probes/FieldBurn.java`, N = 30 M, min-of-5,
wall ms:

| | fast path | decoded path (`--noverify`) |
|---|---:|---:|
| arithmetic loop | **2312** | **5078** |
| instance-field loop | 2954 | 12963 |

The stream is not the lever. Dispatching off `ops[]` as it stands would be a
**regression**; the cost is `execute_instruction`'s out-of-line call, its
~200-variant match on a 16-byte `Instruction`, the `thread.frames[frame_idx]`
re-index per operand, and the `Result` round trip with its post-call error
checks. An index-threaded loop that kept those handlers would inherit all of
it. Rewriting them too is a different and far larger project than the audit
described, and nothing here says it would pay.

(The field row is 4.4x because `--noverify` also turns off the quickened field
arms, so it measures `op_getfield`/`op_putfield` as well. Only the arithmetic
row is a clean dispatch-engine comparison.)

### Fast arms for `tableswitch` / `lookupswitch` are not a lever either

The obvious inference from the row above — "decoded costs 2.2x, so the eight
opcodes without a fast arm cost 2.2x" — **does not hold**, and it is worth
writing down why, because it is an easy mistake to make twice. The ratio is per
*bytecode*. A switch occurs once per iteration and does the work of a whole
comparison chain, so its share of an iteration is small even at 2.2x.

`probes/SwitchBurn.java`, N = 20 M, wall ms. `ifchain` performs the identical
selection using only fast-path arms:

| arm | CratonVM | HotSpot `-Xint` | ratio |
|---|---:|---:|---:|
| `ctl` (all fast arms) | 1801 | 229 | 7.9x |
| `ifchain` (all fast arms) | 2989 | 505 | 5.9x |
| **`table`** (decoded) | **1752** | 374 | **4.7x** |
| **`lookup`** (decoded) | **1787** | 343 | **5.2x** |

**A `tableswitch` is BETTER than the band** — 4.7x against straight-line
arithmetic's 7.9x — while sitting on the path this section just measured at
2.2x. It also beats its own `ifchain` equivalent outright (1752 against 2989),
which is what an O(1) jump table should do and what the decoded handler
delivers.

This confirms the earlier note that said "fast-path arms for `tableswitch` /
`lookupswitch` are not a lever ... they measure 11.0-11.5x, the same band as
arithmetic that never leaves the fast path". That note was right. This page
nearly re-derived the opposite from a correct number applied at the wrong
granularity.

### What is actually left

Straight-line arithmetic on the fast path costs ~7.5 ns per bytecode against
HotSpot's ~0.95 — about 26 cycles for work HotSpot does in three. That is the
whole remaining floor, and it is **not** the dispatch mechanism and **not** the
missing arms. It is the per-bytecode preamble plus the indirect branch, and
`Three findings resolved without a change` below already establishes that most
of the preamble cannot move: the site-cache epochs and the JVMTI watchpoint
gate are correctness-bound, and the safepoint poll has no instruction to save
on x86-64.

The one item never tested is the indirect branch itself — a single dispatch
site giving the predictor one history slot for every opcode transition in every
program. Testing it needs a branch-misprediction counter, which is a hardware
profiler question and not one this host answers cheaply. **Do not build
replicated dispatch sites before that number exists**; this section is two
refutations long precisely because structural proposals here have not survived
contact with a probe.

## Three findings resolved without a change, and why

These were on the original ranked list. Each was read to the point of a verdict
rather than left open, because a finding that is quietly dropped comes back.

### The per-access gate loads: mostly not removable, and the count was overstated

The claim was "seven or eight gate loads to read one field", fixable by hoisting
them into the per-`execute_frame` word beside `fast_field_zgc`. On inspection
most of them cannot move:

* **The two epochs** (`class_definition_epoch`, `resolution_epoch`, read by
  `SiteCache::get` on every hit) are the site cache's validity proof. They
  cannot be hoisted per frame, because **`execute_frame_from_index` runs an
  entire nested call tree in one invocation** — an interpreted call pushes a
  frame and `continue`s the same loop — so "per frame entry" is not
  per-method, it is per outermost interpreter entry. A class defined anywhere
  in that call tree must invalidate the sites, and hoisting would serve stale
  ones.
* **`any_field_watchpoint_active`** has the same problem and it is worse:
  hoisting it would blind a JVMTI agent's field watchpoints for the duration of
  a whole call tree. The existing per-access check is the correct design.
  (The comments on `pgo_enabled` and `single_step_active` describe their
  tradeoff as "observed on the next `execute_frame` entry (call/return)". That
  parenthetical is wrong for the same reason — those gates persist across the
  call tree too. It is defensible for a profiler; it would not be for a
  debugger's watchpoints.)
* **`vacated_frames_enabled`**, read by `check_vacated_compact` on every push,
  is already screened by `fast_field_zgc`, which returns `None` when it is
  armed. So the check is provably a no-op whenever a fast arm runs, and the
  only cost left is its own relaxed byte load. Removing it would couple the
  push helper to that admission gate — a later edit to `fast_field_zgc` would
  silently drop a GC-safety check — which is not worth one load.

What is left of the finding is one gate load per push. It is recorded as
**not worth taking**, and the count in the original write-up (7–8) should be
read as 2–3 that are even candidates.

### The loop-top safepoint poll: no instruction to save on x86-64

The proposal was to replace the per-bytecode `stw_requested.load(Acquire)` with
a `poll_pending` local set at frame entry. **On x86-64 an `Acquire` load is a
plain `mov`** — there is no fence to delete, so the only thing the change buys
is that the compiler may keep the flag in a register, and the only other lever
is poll *frequency*, which is time-to-safepoint. Weighed against a failure mode
whose shape is a GC that waits forever for a thread that never polls, that is
the wrong trade for roughly one L1 hit — especially now that the flag has its
own cache line and that hit is clean.

**It should be revisited on aarch64.** There `Acquire` is `ldar`, a real
ordering instruction, and one per bytecode is not free. The aarch64 port is in
flight; this belongs on its list, not this one.

### The operand-stack kind array: a project with a specific hazard

Collapsing `ValueStack::kinds` / `Frame::local_kinds` from `Vec<u8>` to a packed
2-bit mask is still the right shape — three values, and the second `Vec` costs a
bounds check, a cache line and a pooled buffer per frame. It is not a point fix:

* 43 call sites read the two arrays, plus the GC root scan, freeze/thaw, deopt,
  `snapshot_raw`/`from_snapshot`, and the `Vec<u64> ↔ Vec<CompactValue>`
  `repr(transparent)` transmute the frame pool depends on. `local_kinds`
  deliberately **is** the pool tuple's `Vec<u8>` half, so removing it reshapes
  the pool.
* `max_stack` and `max_locals` are `u16`. An inline `u64`/`u128` mask therefore
  needs a spill path for the tail — and a fixed-width structure that silently
  stops describing slots past its width is precisely the defect this tree has
  already shipped once, when precise oop maps stopped at 64 locals and said
  nothing about it.

And `ValueStack`'s own doc names a better endpoint: consume the verifier's
per-pc type maps (`classloading/type_maps.rs`) so the kind of every slot at
every pc is a static fact and **no** per-slot runtime tag is needed. That
deletes the array rather than shrinking it. Whoever takes this should build the
type-map consumer first.

## Reproduction

```bash
javac -d /tmp/probe probes/FieldBurn.java probes/FieldShape.java \
    probes/ElemShape.java probes/AddrProbe.java

# engagement — the registry implementation must matter only when the probe runs
for bits in 1 0; do for arm in "" "CRATONVM_JIT_NO_FIELD_ADDR_ELIDE=1"; do
  env CRATONVM_ZGC_STARTBITS=$bits $arm cratonvm --java-home <JDK 25> \
      --nojit -c /tmp/probe FieldBurn field 30000000
done; done

# the A/B — alternate the arm order per pass, and print `ctl` beside `field`
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldBurn field 30000000
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldBurn ctl   30000000
```

`ctl` is not optional. Two of this page's four measurement attempts were
discarded because it moved as much as the arm under test.
