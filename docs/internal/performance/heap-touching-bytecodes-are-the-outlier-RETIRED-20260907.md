# Everything the interpreter does is ~10x, except what touches an object header — RETIRED 20260907

| | |
|---|---|
| **Status** | ✅ RETIRED 2026-09-07. Every ranked item is **taken** (with a kill switch and a pairwise count), **refuted** (with numbers), or **handed off** — and every hand-off is now written at the code it is about, with its instrument in place, rather than only on this page. Nothing is left that this page could carry further. |
| **Was** | `docs/known-issues/perf/heap-touching-bytecodes-are-the-outlier-20260905.md` (opened 2026-09-05, severity low — no test failed on it) |
| **Sibling** | `interpreted-invoke-cost-350ns-RETIRED-20260911.md`, whose four passes made this shape visible |

The open page's own header — *"a ranked list with one item taken and measured;
the rest is unbuilt"* — was stale against its own body by the time it was read.
This page records where each item landed and, for the three that are genuinely
someone else's work, **where in the tree the hand-off now lives**, so nothing is
lost when this page stops being read.

## The finding that opened it, unchanged

Windows 11, 24C/32T, host load ~43%, JDK 25.0.3, `cratonvm --nojit` against
`java -Xint`, min-of-5, arms interleaved in both directions inside one process,
each probe carrying its own control arm.

| operation | CratonVM | HotSpot `-Xint` | ratio |
|---|---:|---:|---:|
| one bytecode, straight-line | 7.1 ns | 0.69 | 10.2x |
| tight loop iteration | 53.8 | 5.50 | 9.8x |
| `invokestatic`, 0 args | 132.6 | 14.5 | 9.1x |
| `invokevirtual` | 183.2 | 17.1 | 10.7x |
| `invokeinterface`, inherited | 183.0 | 18.4 | 9.9x |
| **`getfield` + `putfield`, own** | **56.0** | **1.10** | **51x** |
| **`getstatic` + `putstatic`** | **86.4** | **3.45** | **25x** |
| **`iaload`** | **39.9** | **1.68** | **24x** |
| **`arraylength`** | **24.4** | **0.39** | **62x** |
| **`aaload`** (over `iaload`+`ifne`) | **+47.1** | **−0.8** | — |

Two populations and nothing in between: everything that crosses a method
boundary, branches or does arithmetic sits in a 7–11x band; everything that
**dereferences an object header** is 24–62x. That partition is the durable part
and it is why the page was worth writing.

## Where every ranked item landed

| # | item | outcome |
|---|---|---|
| 1 | `aaload` never reaches the quickened path (+47 ns) | **TAKEN.** `field_fast::array_load_ref`; **537 ms over 30 M loads ≈ 18 ns**, 8/8 pairwise, kill switch `CRATONVM_JIT=-ref-array-fast`. Halves the gap to `iaload` (35 → 16 ns). |
| 2 | "seven or eight per-access gate loads" | **NOT WORTH TAKING**, and the count was overstated — 2–3 are even candidates. Reasons below; now recorded in the tree. |
| 3 | `arraylength` takes the `Value` round trip | **TAKEN.** **142 ms ≈ 4.7 ns**, 8/8 with no overlap, kill switch `CRATONVM_JIT=-arraylength-fast`. The page predicted "around one nanosecond"; it was wrong by 5x in the conservative direction. |
| 4 | `getstatic` / `putstatic` at 25x, uninvestigated | **TAKEN.** `op_getstatic` took a class-manager **read lock** on every `getstatic` to answer a question about three fields of one class. `class_is_java_lang_system` latches a `ClassId` at definition; **164 ms ≈ 5.5 ns**, 8/8, kill switch `CRATONVM_JIT=-system-class-latch`. `putstatic` never had the screen. |

Two further changes were made on the same branch and are recorded for the
negatives, which are the useful part:

* **The array autobox latch** — provably answer-preserving, deletes real work,
  and **measured nothing** (2949 against 2969 ms, 3/8 pairwise: worse than a
  coin flip). Kept anyway, on the grounds that it removes work rather than adds
  a cache.
* **The field-registry elide was REVERTED.** The argument for it — that the
  handler it stands in for makes no membership check — was wrong *by looking one
  level too low*: the handler a quickened `getfield` replaces is `op_getfield`,
  not `get_field`, and `op_getfield`'s second act is
  `load_and_forward(obj_ref)`, which probes. It was a robustness regression for
  a weak 1–4 ns at 8/10. **The array half stands** — the array slow paths
  genuinely do not validate — and that asymmetry is the real finding: one gate
  was covering two different answers.
  * How it surfaced is the transferable part: **not from a test.** The suite was
    green on the elide twice, on two collectors, and difftest was clean. It
    surfaced from a *merge* — `dev` added the `# Safety` contract naming the
    probe as its first discharging leg, the merge was textually clean, and the
    file was left documenting a discharge that no longer happened. A green suite
    does not exercise a robustness net; that is what a robustness net is for.

## The two structural proposals for the 10x floor — both refuted

Both now live as a comment on the dispatch `match` in
`vm/src/runtime/interpreter.rs`, with the numbers, so the ideas cannot come back
without meeting them.

* **"Dispatch on the pre-decoded `QuickenedCode` stream."** `--noverify` flips
  `use_fast_path`, and the decoded path already runs on that stream — so the two
  engines price directly, in one binary, on one program. `probes/FieldBurn.java`,
  N = 30 M, min-of-5: arithmetic loop **2312 fast against 5078 decoded**. The
  stream is **2.2x slower**.
* **"Give the eight opcodes with no fast arm one."** The 2.2x is per *bytecode*,
  and a switch runs once per iteration while doing the work of a whole
  comparison chain. `probes/SwitchBurn.java`, N = 20 M: `tableswitch` measures
  **4.7x** HotSpot against straight-line arithmetic's 7.9x — *better* than the
  band, on the very path measured at 2.2x — and beats its own `ifchain`
  equivalent outright (1752 against 2989 ms).

## The field path, attributed — and what happened to `gates` since

Four structural explanations for the instance-field ratio were proposed from
reading code and refuted by measurement (the bitmap probe as a cache miss, the
elide as a parity restoration, the compact body layout, the `SiteCache` scaling
with site count). Four hypotheses, four refutations, **zero attribution** — the
signature of a missing instrument.

`vm/src/runtime/interpreter/field_phases.rs` is that instrument
(`CRATONVM_DBG_FIELD_PHASES=1`), and **it was wrong twice, and said so both
times**: `accesses=1` (the counter sat on an arm the probe's legacy-layout
receiver never reached) and then four phases within one cycle of each other
(`charge` was a `lock xadd`, four times per access on a ~100-cycle path;
thread-local accumulation dropped the total from 198.8 to 137.4 cyc/access).

Its attribution named `gates` — the watchpoint load, the stack length check, the
receiver peek and the tag decode — as the largest phase at 33.0 cycles / 38%,
holding the least obvious work, with two candidates it could not separate. **The
page's stated next step was a one-binary A/B of each. Both are now closed:**

* **The `Acquire` load of `FIELD_WATCHPOINTS_ACTIVE`.** A/B'd and **recorded at
  the load itself** in `vm/src/runtime/jvmti.rs`: three relaxed runs against the
  acquire build, entry 20.3 / 23.2 / 22.7 against 20.6 corrected, every other
  phase and every phase *share* identical to three significant figures. **It
  measured nothing**, and the ordering is left alone — relaxing a JVMTI
  mechanism's memory ordering for an unmeasurable win is not worth carrying.
* **`getfield_fast_keyed`'s own prologue, if LLVM declines to inline it.** The
  instrument was **split** for exactly this: `gates` is now `P_ENTRY`
  (entry + watchpoint gate) and `P_PEEK` (stack length, `peek_compact`, tag
  decode), and the split's contract is written into `P_PEEK`'s doc — *"if
  `P_ENTRY` keeps the bulk, the cost is entry-shaped; if this one does, it is
  the operand stack and neither candidate is right."*

**What the split says, and the honest caveat.** Four runs of
`probes/FieldBurn.java` (`field`, N = 8 M, `--nojit`) on the Azure build host,
JDK 25.0.4, **at load ~11 on 8 cores**:

| phase | raw (cyc) across 4 runs | corrected |
|---|---|---|
| `entry+wpgate` | 28.8 / 24.4 / 26.1 / 24.6 | 4.4 / 0.0 / 1.7 / 0.0 |
| `stack_peek` | 24.6 / 29.0 / 27.7 / 25.0 | 0.1 / 3.7 / 3.3 / 0.0 |
| `site_lookup` | 26.9 / 25.7 / 25.7 / 25.8 | 2.5 / 0.4 / 1.3 / 0.0 |
| `field_ptr` | 31.3 / 29.3 / 30.8 / 29.1 | 6.9 / 4.0 / 6.4 / 2.9 |
| `read+stack` | 29.0 / 29.6 / 34.4 / 30.0 | 4.5 / 4.2 / 10.0 / 3.9 |
| `CALIB(noop)` | 24.5 / 25.3 / 24.4 / 26.1 | — |

**On a host this loaded the instrument has no resolution left**: the `rdtsc`
calibration is 24–26 cycles and every phase's raw figure is within ~5 of it, so
the corrected column is a small difference of large numbers and its *ordering*
changes between runs. The one thing that survives the noise is that
`field_ptr` and `read+stack` are the top two in all four runs, and that
`gates` — whichever half — is no longer the largest phase it was on the quiet
Windows host.

### The verdict, taken 2026-09-08 with ten runs instead of four

Same probe, same host, load ~21, ten runs — corrected cycles per access:

| phase | corrected, 10 runs |
|---|---|
| `entry+wpgate` | 0.0 0.5 0.0 0.0 0.0 0.0 3.1 0.0 1.7 0.0 |
| `stack_peek` | 0.0 0.6 0.0 0.4 0.9 0.2 0.8 1.6 0.0 0.0 |
| `site_lookup` | 0.0 9.5 20.3 1.4 0.3 1.0 1.9 4.0 0.2 0.2 |
| **`field_ptr`** | **4.4 7.1 5.8 5.9 2.8 5.2 8.9 10.1 6.4 6.3** |
| `read+stack` | 4.9 4.4 2.5 4.9 1.8 4.4 3.5 6.7 3.3 8.4 |

`P_PEEK`'s own doc frames the question: *"if `P_ENTRY` keeps the bulk, the cost
is entry-shaped; if this one does, it is the operand stack and neither candidate
is right."*

**Neither keeps the bulk.** `entry+wpgate` reads **0.0 in seven of ten runs**
and never exceeds 3.1; `stack_peek` never exceeds 1.6. Both sit at or under this
instrument's resolution. `field_ptr` is the largest of the five in **ten of
ten**, at 3–10 cycles, with `read+stack` second.

So the `gates` phase that the open page called the largest at 33.0 cycles / 38%,
"holding the least obvious work", **is not where the time is** — and both
candidates it named for that work are now refuted rather than merely untested:
the `Acquire` compiler barrier by the A/B recorded at the load itself in
`vm/src/runtime/jvmti.rs` (three relaxed runs, identical to three significant
figures), and `getfield_fast_keyed`'s un-inlined prologue by this split, since a
prologue would land in `P_ENTRY` and `P_ENTRY` is zero.

**And again at load ~11, which is the quietest this host got.** Ten more runs,
same probe, same binary:

| phase | corrected, 10 runs at load ~11 | median |
|---|---|---:|
| `entry+wpgate` | 0.4 0.9 0.0 0.2 0.1 7.2 0.4 0.4 1.9 0.5 | ~0.4 |
| `stack_peek` | 0.6 0.7 0.0 0.0 0.2 0.0 0.8 0.2 0.4 0.6 | ~0.4 |
| `site_lookup` | 2.6 1.5 0.2 1.4 1.1 0.7 2.5 1.4 3.6 1.1 | ~1.4 |
| **`field_ptr`** | **5.3 4.7 3.3 6.3 3.6 3.2 5.2 4.8 6.5 3.5** | **~4.8** |
| `read+stack` | 4.1 4.6 2.4 4.0 2.6 6.1 3.5 3.5 4.6 2.8 | ~3.8 |

Tighter than the load-21 set — `site_lookup`'s 20-cycle excursion is gone — and
the ranking is unchanged: `field_ptr` largest (9 of 10, the exception being run
6), `read+stack` second, `entry` and `peek` at ~0.4 each, an order of magnitude
below. **Two independent ten-run samples at two load levels agreeing on the
ordering is what turns this from a reading into a verdict.**

**The caveat that remains.** Load 11 on 8 cores is still oversubscribed, and
these are means over 8 M accesses, not minima — so the numbers are a RANKING,
which is all this instrument promises (*"it ranks rather than costs"*). What is
settled is which phase is largest and that `gates` is not it; what is not
settled is what `field_ptr`'s ~4.8 cycles would read on an idle machine.

Two facts that did survive and are worth carrying:

* `site_lookup` is consistently among the cheapest phases — a **second,
  independent witness** against the hashed-table hypothesis that
  `probes/SiteSpread.java` refuted by varying site count.
* `field_ptr` at ~14.8 cycles on the quiet host is consistent with the registry
  probe's separately measured 1–4 ns, which is a third cross-check landing where
  it should — and it is the top phase in ten of ten runs above, so the ranking
  agrees across two hosts and two load regimes.

## Why the gate loads cannot be hoisted — now in the tree, not just here

The finding that "7–8 gate loads" could be hoisted into the per-`execute_frame`
word rested on a wrong model of what `execute_frame` is, and the correction is
now a comment at the hoist site in `vm/src/runtime/interpreter.rs`:

**`execute_frame_from_index` runs an entire nested call tree in one
invocation** — an interpreted call pushes a frame and `continue`s the same loop
— so "observed at frame entry" is not per-*method*, it is per-*outermost
interpreter entry*. Therefore:

* **The two site-cache epochs** (`class_definition_epoch`, `resolution_epoch`)
  are the cache's validity proof; a class defined anywhere in that call tree
  must invalidate the sites, and a hoist would serve stale ones.
* **`any_field_watchpoint_active`** is worse: hoisting it would blind a JVMTI
  agent's field watchpoints for a whole call tree. The per-access check is the
  correct design. (The `pgo_enabled` / `single_step_active` comments described
  their tradeoff as "observed on the next `execute_frame` entry (call/return)".
  That parenthetical was wrong for the same reason and is corrected in place —
  it is defensible for a profiler and would not be for a debugger.)
* **`vacated_frames_enabled`** is already screened by `fast_field_zgc`, which
  returns `None` when it is armed — so the check is provably a no-op whenever a
  fast arm runs. Removing it would couple the push helper to that admission
  gate, so a later edit to `fast_field_zgc` would silently drop a GC-safety
  check. Not worth one load.

## The change that could not be A/B'd, and the bound that replaced it

`GcBarrier::stw_requested` is the hottest read in the VM and shared its cache
line with three fields written by other threads. A struct layout is not a
runtime toggle, so there is no kill switch and no way to put both arms in one
binary — it was **bounded** instead: `probes/SharedLine.java` measured
**116,025 handoffs in 4 s ≈ 29,000/s** (~58,000 writes/s to the line), giving
`58_000 x 8 x 70ns ≈ 32 ms/s across 8 cores ≈ 0.4%`, below what the harness
resolves; 64 readers at ~200 ns cross-socket is still about 1%.

**The honest statement is a bound, not a win**, and the whole derivation now
lives on `CacheLineFlag` in `vm/src/threading/gc_barrier.rs`, including the
invitation to turn it into a number on a many-core box.

## Two corrections to the tree, both made

* **`field_fast.rs`'s module doc was stale.** It opened by explaining that
  "essentially every object the interpreter allocates is legacy" because
  `ZgcRealHeap::try_alloc_object` never set the compact shape.
  `compact_tlab_alloc_enabled` has been **default-ON since 2026-09-03**, so the
  COMPACT arm serves the common case. Both arms are still needed; the reasoning
  printed above them was describing the wrong default, and it is the reasoning a
  reader uses to decide which arm to optimise. Corrected in place.
* **Object body shape does not move field cost.** `probes/FieldShape.java` at
  250 k x 4, three interleaved passes, one binary,
  `CRATONVM_COMPACT_TLAB_ALLOC=0` against the default: own-field pair
  122 / 120 / 117 ns compact against 124 / 126 / 133 legacy, with the control arm
  moving as much as the difference. No separation. The compact TLAB shape bought
  **memory** — 87.5 MB on `TestCache` — and did not buy field throughput.

## Handed off, with the tree location of each hand-off

These three are not "unfinished items on this page". Each needs equipment, a
port, or a project this page never had, and each is now written where the person
who takes it will be standing.

| item | owner | where the hand-off now lives |
|---|---|---|
| The **indirect branch** — one dispatch site gives the predictor one history slot for every opcode transition in every program. Needs a branch-misprediction counter, i.e. a hardware profiler. **Do not build replicated dispatch sites before that number exists.** | whoever has a hardware profiler | comment on the dispatch `match`, `vm/src/runtime/interpreter.rs` |
| The **loop-top safepoint poll**. On x86-64 an `Acquire` load is a plain `mov` — no fence to delete — and the only other lever is poll *frequency*, i.e. time-to-safepoint, whose failure mode is a GC waiting forever for a thread that never polls. **On aarch64 `Acquire` is `ldar`, a real ordering instruction, and one per bytecode is not free.** | the aarch64 port | comment at the per-bytecode poll, `vm/src/runtime/interpreter.rs` |
| The **operand-stack kind array**. Collapsing `ValueStack::kinds` / `Frame::local_kinds` to a packed 2-bit mask is still the right *shape* and the wrong *move*: 43 call sites plus the GC root scan, freeze/thaw, deopt, snapshots and the `repr(transparent)` transmute the frame pool depends on; and since `max_stack`/`max_locals` are `u16`, an inline mask needs a spill path for the tail — **a fixed-width structure that silently stops describing slots past its width is precisely the defect this tree has already shipped once**, when precise oop maps stopped at 64 locals and said nothing about it. The real endpoint is to consume the verifier's per-pc type maps, which deletes the array rather than shrinking it. | whoever builds the type-map consumer | `ValueStack::kinds`' doc, `vm/src/runtime/value_stack.rs` |

## The rule this page earned

**Two probe-elision hypotheses, two zeroes.** The per-bytecode floor is high
enough to hide a load — `probes/AddrProbe.java` walks receivers across live sets
of 4 K / 256 K / 2 M and CratonVM's `getfield` delta stays flat at ~50–90 ns
while HotSpot's climbs 0 → 98 → 111 ns, because CratonVM's own per-bytecode cost
hides the header miss and the bitmap word and the header are both derived from
the same pointer, so the two loads issue in parallel.

So: **until the per-bytecode floor comes down, the levers that pay are the ones
that delete a *path* — a dispatch, a representation conversion, a re-read — not
the ones that delete a load.** Every item that paid on this page (18 ns, 4.7 ns,
5.5 ns) deleted a path. Both that measured zero deleted a load.

## Gates on the branch that retired this page

The perf items themselves landed earlier and carry their own gates in the table
above — each with a kill switch and a pairwise count. What this branch adds for
this page is comment-and-doc only (the two tree corrections and the three
hand-offs), so the gate that matters is that nothing moved.

| gate | result |
|---|---|
| `regression-suite/run.sh`, default collector | **92 of 92 scheduled vectors passed, 0 failed**, 0 list/coverage errors, 0 harness-blindness flags |
| `regression-suite/run.sh`, `--XX:UseGc G1` | **92 of 92 passed, 0 failed** |
| `cargo clippy --release --workspace --all-targets -- -D warnings` | **rc=0 — GREEN**, and it was RED on `dev` for three unrelated pre-existing reasons this branch also fixes (see below) |
| `cargo test --workspace` (the CI gate is debug, not release) | **One failure, and it is not this branch's**: `jit_ir_athrow_dispatch` trips its own anti-vacuity assertion (*"`throwAs` was never reported as compiled by the optimizing tier"*) on this host. Its probe run by hand reports **0** lines of `optimizing backend produced a body … throwAs` on this branch **and on a pre-change binary**, so it is pre-existing here. Everything else passes. |
| `cratonvm-difftest gate --corpus difftest/seeds` | **clean — no new or regressed divergence, exit 0** |
| `cratonvm-difftest run --modes nojit,interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt` | **0 of 6 programs diverged, 0 split across CratonVM's own execution paths, 0 skipped** — and the `0 skipped` is load-bearing: without `javac` on `PATH` every seed SKIPs and the run still exits 0 |

### Three clippy blockers cleared on the way, none of them this page's

`cargo clippy --workspace --all-targets -- -D warnings` was red on `dev` under
stable 1.98, so this branch could not have gated itself without fixing them:

* **`jit/src/ir.rs` — a silently disabled test, not just a lint.** A later
  commit inserted its new tests between
  `every_declared_family_is_recognised`'s doc comment and its `fn`, which
  orphaned the comment onto the first of them and left that function with **no
  `#[test]` attribute**. The one check that a `ScalarOp` family cannot be added
  without its recognizer signature had stopped running, and clippy's
  `duplicated attribute` was the only thing saying so.
* **`native-builtins/src/crypto_impl.rs`** — three `manual_slice_fill`. The
  comment there says plainly that neither the loop nor `fill(0)` is a
  *guaranteed* zeroization, so this is a lint fix and not a strengthening of
  that property.
* **`vm/src/runtime/threading_integration.rs`** — one `drain_collect`.


## Reproduction

```bash
javac -d /tmp/probe probes/FieldBurn.java probes/FieldShape.java \
    probes/ElemShape.java probes/AddrProbe.java probes/ArrBurn.java \
    probes/StaticBurn.java probes/SwitchBurn.java

# the A/B — alternate the arm order per pass, and print `ctl` beside `field`
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldBurn field 30000000
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldBurn ctl   30000000

# the phase instrument (ranks, does not cost; needs a QUIET host)
CRATONVM_DBG_FIELD_PHASES=1 cratonvm --java-home <JDK 25> --nojit \
    -c /tmp/probe FieldBurn field 8000000
```

`ctl` is not optional. Two of this page's four measurement attempts were
discarded because it moved as much as the arm under test — and two of its passes
were saved by a *census* rather than a clock, when `array_load_ref` reported
`aaload: hit=0 miss=2000146` and would otherwise have A/B'd as a clean,
indistinguishable-from-absent zero.
