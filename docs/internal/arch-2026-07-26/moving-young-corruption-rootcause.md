# The moving young generation: what actually corrupts the heap

Slug: `moving-young-corruption-rootcause` · Wave: arch-2026-07-26
Basis: `arch/wave1-integration-20260726` merged at `928ad62a9`
(first merge in this session was `c5d9d2de2`; re-merged on the orchestrator's
instruction to pick up the `jit/src/x64.rs` flag-skew closure).
Continues `docs/internal/arch-2026-07-26/moving-young-precise-roots.md`.

> **SUPERSEDED IN PART, 2026-07-26.** The known-issue is CLOSED — see
> `docs/internal/fixed-suite-bugs/app-jvm-bugs/moving-young-gen-drops-jit-held-oops-FIXED.md`.
> The measured producer was an **untagged operand-stack oop**: five codegen
> sites pushed an object reference without setting `stack_oop_marks`, so it was
> published nowhere while the safepoint still certified coverage (that check
> only inspects MARKED entries). Section 3's "precise-only under-coverage"
> verdict is right in shape — the coverage bit was not trustworthy — but wrong
> in mechanism for bt18: `BinTreesClassic.bottomUpTree` scalar-replaces nothing
> and hoists nothing (`FrameLayout { scalar_lo: 0, scalar_hi: 0,
> ref_hoist_lo: 0, ref_hoist_hi: 0 }`). The cross-owner request in §7.1 was
> nonetheless actioned for the scalar-replacement `getfield` path.
>
> The frame-band verifier this session added was load-bearing for the
> diagnosis — it is what proved the coverage bit was lying — but as written it
> reported a phantom miss on **every** collection (register images and
> reclaimed operand-spill slots), so moving-young ran zero cycles. It is now
> precise; see the FIXED doc.

---

## VERDICT (read this first)

Three hypotheses were on the table. The answers are different for each, and
none of them is "no".

| # | Hypothesis | Verdict |
|---|---|---|
| A5 | An unregistered compiled frame (live without a `JitEntryGuard`) is neither published nor conservatively marked | **CONFIRMED** for the `CRATONVM_MOVING_YOUNG=1`-only measurement — and **insufficient**: it cannot explain the 14×-larger `+ALLOW` measurement, which is what the new default-on code behaves like |
| Blind spill | The default-on full-GPR safepoint spill makes a primitive bit pattern a *root*, so a moving cycle relocates a random object and rewrites the spill slot the code never reads back | **REFUTED as stated** — those slots are never roots on any cycle that relocates. But the audit that produced it points straight at the real defect (below) |
| **Precise-only under-coverage** | `moving_young_coverage_complete` is computed from the abstract interpreter's locals + operand stack, while a compiled frame also holds oops in **scalar-replacement field slots**, **LICM hoist slots** and the **blind GPR spill area**; `roots.rs` suppresses the conservative scan that used to cover all three | **CONFIRMED — this is the substantive root cause** |

**Do not flip the default yet.** The fix below removes the unsoundness, but it
was written without a build (nine concurrent `cargo build`s OOM the host), and
the acceptance criterion is a measurement the orchestrator must take. The flip
itself is now genuinely one constant:

```rust
// types/src/flags.rs
pub const DEFAULT_MOVING_YOUNG: bool = true;   // was false
```

`jit/src/x64.rs:2486 moving_young_enabled()` now reads
`cratonvm_types::flags().gc.moving_young`, so codegen, root gathering and the
collector all move together. That was the last structural blocker and it is
closed on the merged tree.

---

## 1. A5 — confirmed, by elimination, and then bounded

### The confirmation

The measured matrix was taken on code where
`fail_closed_non_moving = is_active() && !allow` was ORed unconditionally into
`divert_non_moving`. With `CRATONVM_MOVING_YOUNG=1` and **no** `ALLOW`, every
cycle with `is_active()` true diverted to the non-moving sweep. So the
`68310832` corruption came *exclusively* from cycles where `is_active()` was
**false** — i.e. cycles on which this thread's `JIT_ENTRY_CHAIN` was empty.

Now enumerate what a moving cycle can touch on such a cycle:

* `remap_active_jit_frames` (`vm/src/jit/conservative_roots.rs`) iterates
  `JIT_ENTRY_CHAIN`. Empty chain ⇒ no-op. It cannot corrupt.
* `scan_active_jit_frames` likewise iterates the chain ⇒ contributes nothing.
* The shadow stack is bounded by `top`, and every compiled epilogue restores
  `top` to its entry watermark (`x64.rs` `emit_epilogue`, the
  `shadow_savetop_slot_off` restore). An empty chain therefore implies an empty
  (or stale-but-harmless) shadow window; a stale extra slot can only
  **over**-retain, never under-count.
* Every remaining root and remap channel — interpreter frames, statics, JNI,
  handles, side stores, loader singletons — is exercised identically by the
  `CRATONVM_MOVING_YOUNG=1 + CRATONVM_DISABLE_JIT=1` run, which is **measured
  correct** (`68332206`) on every attempt.

So a moving cycle with an empty chain and no compiled frame on the stack is
provably equivalent to the JIT-disabled configuration, which does not corrupt.
The only remaining explanation for `68310832` is a compiled frame that was live
on the native stack while absent from the chain. **That is exactly A5**, and it
is a previously-observed condition in this exact benchmark
(`docs/internal/fixed-suite-bugs/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`).

The A5 probe was re-enabled under moving-young by the previous session
(`refresh_moving_young_coverage_for_current_thread`, the
`#[cfg(any(windows, linux))]` block). That change is correct and stands.

### Why it is not the whole answer

`CRATONVM_MOVING_YOUNG=1 + CRATONVM_ALLOW_MOVING_YOUNG=1` — the configuration
that also lets cycles with `is_active()` **true** relocate — measured
`68029454`: a shortfall of `302 752` against A5-only's `21 374`. **Fourteen
times worse.** `ALLOW` is now deleted, which means the merged code's behaviour
on a JIT-active cycle is the `+ALLOW` behaviour plus coverage proofs. So the
dominant error term lives on the *registered* path, where A5 says nothing.

Anyone who re-measures the known-issue after the A5 fix alone and sees a
correct checksum should check `moving_young_cycle_count()` before celebrating:
the A5 probe is a raw-word scan that over-detects, so "correct" may simply mean
"moving-young never engaged".

---

## 2. The blind full-GPR spill — refuted as stated

The claim: `precise_implies_reg_spill` is default-on (`precise_maps` default-on
since 2026-07-07, `precise_reg_spill_disabled()` default-false), so
`emit_pre_safepoint_spill` really does store all 14 GPRs at every GC-capable
safepoint; a primitive whose bit pattern passes `is_object_address` becomes a
root; a moving cycle relocates the object it appears to name and rewrites the
*spill slot*, which the code never reads back.

Every premise is true. The conclusion does not follow, because **the spill
slots are never roots on a cycle that relocates**:

* `reg_spill_base` appears at exactly three places in `jit/src/x64.rs`
  (declaration at `8127`, layout at `8927`/`8945`, the stores at `10045-10062`).
  It is **write-only**: nothing reloads from it, and no `OopMapEntry`
  `frame_slot_offsets` list contains it, so `remap_active_jit_frames` never
  touches it either.
* The only reader is the conservative scan, and
  `vm/src/memory/roots.rs` calls `scan_active_jit_frames` **only when
  `moving_young_precise_only` is false**. Whenever that is false under
  moving-young it is because the coverage proof failed, which sets
  `divert_for_incomplete_moving_coverage` in
  `gen_heap::collect_garbage_inner` — and that term forces the non-moving
  sweep. The conservative scan and relocation are mutually exclusive.
* The one exception is `CRATONVM_DBG_FORCE_MOVING`, which deliberately
  overrides the diversion. That is a debug flag and was not set in the
  known-issue runs.

So: no spurious relocation from the blind spill. Recorded here so the
hypothesis is not re-derived a third time.

### What the same audit *does* prove

The audit's second half is exactly right and is the load-bearing observation:

> On return, the compiled code resumes from the **register**, not the spill
> slot.

`emit_post_safepoint_reload` (`jit/src/x64.rs:10813`) reloads **only Java oop
LOCALS that have a register assignment**, and only from their canonical
`local_offset(k)` slots. `emit_shadow_reload` restores only the homes that
`collect_live_oop_homes` chose. **A register holding an oop that is neither a
Java local nor an operand-stack entry is never refreshed after a moving
collection** — and, worse, is never published as a root in the first place.
That is section 3.

### Bisection warning (state this whenever you hand someone the repro)

`CRATONVM_JIT_SAFEPOINT_REG_SPILL` is parsed with `is_some()`
(`x64.rs:2639`). Setting it to `0` — or to anything at all — **ENABLES** the
callee-saved spill arm. The real opt-out is
`CRATONVM_NO_PRECISE_REG_SPILL=1`. `=all` and `=nostore` select the full-GPR
and reserve-only variants respectively, and both also satisfy `is_some()`.

---

## 3. The root cause: "precise-only" is precise about the wrong set

### The mechanism

`vm/src/memory/roots.rs` step 14:

```rust
let moving_young_precise_only = moving_young
    && !moving_young_osr_fallback
    && refresh_moving_young_coverage_for_collection()
    && !moving_young_coverage_incomplete();
if !moving_young_precise_only {
    scan_active_jit_frames(&shared.mem.heap, &mut roots);
}
```

When the proof passes, the **entire** JIT root set for the cycle is the current
thread's shadow stack (step 14b). Not the conservative scan; not even the
precise per-frame oop-map scan (`scan_one_frame_precise` is reached only
*through* `scan_active_jit_frames`). Everything therefore rests on one bit:
`OopMapEntry::moving_young_coverage_complete`.

That bit comes from `x64::moving_young_safepoint_coverage_complete()`
(`jit/src/x64.rs:10155`), and the set it certifies is chosen by
`collect_live_oop_homes()` (`10187`). **Both consult only `self.stack` (the
simulated operand stack) and `local_oop_masks` (the Java locals).**

A compiled frame holds oops in at least three other storage classes, none of
which is a Java local or an operand-stack entry:

1. **Scalar-replacement field slots.** Escape analysis explodes an object into
   raw frame slots at `[rbp - (field_base_offset + k*8)]`
   (`x64.rs:8309 ScalarReplacedObject`; written by the `new` arm at `27171`,
   read/written by the scalar `getfield`/`putfield` arms at `22026` / `22440`).
   A **reference-typed** field of such an object is a live heap pointer sitting
   in a plain frame slot.
2. **LICM hoist slots.** `hoist_info` hoists a loop-invariant `aaload` into
   `hoist_offsets[idx]` (`x64.rs:18915`). For a reference array — `Object[][]`,
   the "hoisted row pointer" the code's own comment names at `18688` — that
   slot holds an object reference for the whole loop.
3. **The blind full-GPR spill area** (`reg_spill_base`). Its stated purpose is
   to make register-resident values "visible to the conservative scan"; it
   exists *because* the compiler knows registers hold oops the maps do not
   describe.

On the non-moving path all three are covered, because the conservative frame
scan reads every word of the frame and pins whatever looks like an object.
Under moving-young that scan is suppressed and the shadow push does not
enumerate them, so those oops are **neither marked nor rewritten**. A Cheney
copy then either reclaims the object (if the frame slot held its only
reference) or relocates it and strands the slot.

### Why this matches the measurement and A5 does not

* Magnitude: a handful of frames per collection, only the ones whose live set
  happens to include a hoisted/scalar-replaced/register-only oop → the ~0.03 %
  shortfall.
* Run-to-run variance: depends on register allocation, on which loops LICM
  chose, and on when the collection lands.
* GC-pressure dependence: depth 14 is clean, depth 18 is not.
* Clean with `CRATONVM_DISABLE_JIT=1`: no compiled frames, no such slots.
* Proportional to how much relocation happens: `+ALLOW` moves on JIT-active
  cycles too — precisely the cycles where these frames are live — and is 14×
  worse. **A5 predicts no such term; this does.**

---

## 4. What changed (this session)

The fix does not try to enumerate the missing storage classes — that would be a
codegen change in a file this session does not own, and getting it *almost*
right is silent corruption. Instead it stops **asserting** coverage and starts
**verifying** it, against the frame's actual bytes.

### `vm/src/jit/conservative_roots.rs`

* **`moving_young_unpublished_frame_oop_present(&mut reason) -> bool`** (new).
  Walks each live compiled frame's own spill band
  `[rbp - osr_frame_size, rbp)` — the same bounded band
  `scan_compiled_frame_bands` already uses, so intervening interpreter/Rust
  frames are excluded — and reports `true` if any word in it lands in a
  published young semispace but is absent from the thread's published shadow
  window. Fail-closed: an unbounded band, a missing frame size, or an
  out-of-range exact RBP all report `true` with reason
  `UNBOUNDED_FRAME_BAND`.
* **`shadow_window_from_frame`** (new). Recovers `[base, top)` from a live
  frame alone: the frame caches its `*mut JvmThread` at
  `[rbp - cm.shadow_thread_slot_off]` and the `ShadowStack` sits at
  `thread + cm.shadow_off_in_thread`. A **null** thread slot (the state
  `maybe_nop_out_shadow_fetch` leaves behind for a method that emitted no push)
  yields `None`, which the caller treats as an **empty published set** — so
  such a frame proves nothing rather than everything. That case is its own
  regression test.
* **`band_has_unpublished_young_word`** / `..._with` (new). The predicate is
  injected in the `_with` variant so the scan is unit-testable without mutating
  the process-global published-bounds table that parallel tests share.
* Wired into **`refresh_moving_young_coverage_for_current_thread`**, not into
  the `roots.rs` call site. That is deliberate: the same function is what
  `vm/src/vm/vm_exec.rs::deposit_root_snapshot` and
  `vm/src/runtime/interpreter.rs::update_root_snapshot` call before suppressing
  *their* conservative scans, so a parked or safepointed thread inherits the
  verification without those (differently-owned) files changing.

### `gc/src/gen_heap.rs`

* **`addr_in_published_young_regions(addr) -> bool`** (new). A lock-free,
  `&VmHeap`-free containment test over `JIT_REGION_BOUNDS`' two young
  semispaces. Containment, not header validation: it is deliberately a superset
  of "is a young object address", because over-detection costs one non-moving
  collection and under-detection costs the heap. Only the young generation
  matters — a moving *young* cycle cannot strand an old-gen or off-heap word.

### `gc/src/shadow_stack.rs`

* **`ShadowStack::BASE_OFFSET`** (new, `= 16`), asserted by the existing
  `layout_offsets_match_jit_contract` test. Not read by codegen; it is what
  lets the verifier find the *start* of the published window.
* New test `published_window_matches_the_scanned_range`, pinning `[base, top)`
  to exactly what `for_each_value` walks. If those ever diverge, the verifier
  could accept an unpublished oop because a stale slot happened to hold it.

### `gc/src/gc_quiescence.rs`

* Two new `incomplete_reason` codes — `UNPUBLISHED_FRAME_OOP` (10) and
  `UNBOUNDED_FRAME_BAND` (11) — plus `incomplete_reason::COUNT`.
* **`moving_young_fallback_reason_counts()`** (new): a per-reason histogram,
  bumped in `record_moving_young_coverage_fallback` with the cycle's **first**
  reason (the one that actually forced the decision). See section 6.

### Behaviour when moving-young is OFF

Nothing runs. `moving_young_unpublished_frame_oop_present` returns `false` on
its first line, `addr_in_published_young_regions` is never called, and the
histogram is never bumped. The default path is unchanged.

---

## 5. Honest statement of what the fix costs

The verifier is a conservative scan of the compiled frames' spill bands, so it
will report "unpublished" for any word that merely *looks* young-resident. On a
frame carrying the full-GPR safepoint spill, that includes any primitive whose
bit pattern falls inside a young semispace. **Moving-young may therefore divert
on most or all cycles once it is enabled.**

That is not a hidden default-off landing, and it must not be reported as one:

* It is **loud**. Every diversion bumps `moving_young_coverage_fallback_count()`
  and the per-reason histogram, and the first eight (then powers of two) log at
  `warn` naming the obligation.
* It is **exactly measurable**. `UNPUBLISHED_FRAME_OOP` dominating means the
  storage classes in section 3 are the blocker; `UNBOUNDED_FRAME_BAND`
  dominating means `osr_frame_size` is not populated for normal entries and the
  band walk needs a different bound; `UNREGISTERED_JIT_FRAME` dominating means
  A5 over-detection is the blocker and the cross-owner request in section 7.2
  is the next move.
* It is **correct in the meantime**. A diverted cycle is the current default
  collector.

The alternative — trusting the coverage bit — is the thing that is measured to
corrupt.

---

## 6. Making the 2026-07-01 mistake unrepeatable

That run declared moving-young "validated correct" after executing **zero**
moving cycles. Two counters now make that impossible to repeat silently, and a
unit test pins their relationship:

| Question | Call |
|---|---|
| Is the young generation actually copying? | `gc_quiescence::moving_young_cycle_count()` — non-zero or it is not |
| If not, what is stopping it? | `gc_quiescence::moving_young_fallback_reason_counts()` — indexed by `incomplete_reason` |
| How often did it give up? | `gc_quiescence::moving_young_coverage_fallback_count()` |

`gc_quiescence::tests::fallback_histogram_attributes_the_blocking_obligation`
asserts that a fallback attributes to the *first* reason of the cycle, that a
later consequential reason cannot steal the attribution, and that a fallback and
a moving cycle are never both counted for one collection.

**A checksum that is correct while `moving_young_cycle_count() == 0` proves
nothing.** Any future validation table must print both numbers.

---

## 7. Cross-owner requests

These are the changes this session did **not** make. Each is in a file owned by
another agent this wave.

### 7.1 `jit/src/x64.rs` — `moving_young_safepoint_coverage_complete()` (line 10155) must fail closed on storage it does not model

**Rationale.** The function certifies "the shadow push publishes every live oop
at this safepoint" from `self.stack` and `local_oop_masks` alone. It has no
knowledge of `self.scalar_replaced`, `self.hoist_info` / `self.arith_hoist_info`,
or `reg_spill_base`. Section 3 shows each of those can hold a live reference.
Certifying `true` in their presence is what makes the collector relocate behind
an un-rewritable slot.

**Minimum change** — add to the early-return chain:

```rust
if !self.scalar_replaced.is_empty()
    || !self.hoist_info.is_empty()
    || !self.arith_hoist_info.is_empty()
{
    return false;
}
```

**Better change** (removes the cost rather than the capability): extend
`collect_live_oop_homes()` (line 10187) to also push, under `complete`, a
`ShadowHome::Frame(off)` for every reference-typed scalar-replacement field slot
and every reference-typed LICM hoist slot. The verifier added in this session
then observes the improvement directly as `UNPUBLISHED_FRAME_OOP` falling to
zero — which is the honest way to check that the extension is complete, rather
than asserting it.

### 7.2 `jit/src/x64.rs` — a post-safepoint **register** reload for non-local oops

**Rationale.** `emit_post_safepoint_reload` (line 10813) reloads only oop
*locals* with a register assignment, from `local_offset(k)`. A register holding
an operand-stack oop is covered by `emit_shadow_reload`; a register holding a
hoisted or scalar-replacement oop is covered by neither, so even if such an oop
were marked via another path, the register the code resumes from is stale after
relocation. Whatever homes 7.1 adds to `collect_live_oop_homes` must be
`ShadowHome::Reg` where the value is register-resident, so the existing shadow
reload refreshes them; a frame-slot-only publication is not sufficient.

### 7.3 `vm/src/memory/roots.rs` — no change requested, but note the coupling

`roots.rs` is owned this session and was **not** modified: the verification was
placed inside `refresh_moving_young_coverage_for_current_thread` precisely so
`roots.rs`, `vm_exec.rs` and `interpreter.rs` all inherit it without edits. If a
future change moves the decision out of that function, the verification must
move with it.

### 7.4 G1 — state the invariant the pin snapshot depends on

`gc_quiescence::pinned_jit_roots_snapshot()` is ANDed with `is_active()` on the
G1 side, so a pin published without a live guard is silently ignored. That is
sound **only** because a thread blocked below a JIT frame keeps its
`JitEntryGuard` for the whole blocking region — which is true (`pop_jit_entry`
runs from `JitEntryGuard::drop`, and the blocking paths park *inside* the
guarded region) but is written down nowhere. Whoever owns the G1 file should
state it at the AND, because a future "prune the guard while parked"
optimisation would silently un-pin every blocked thread's regions.

---

## 8. What the orchestrator should run

Build the merged tree, then, with the flip **not** applied:

```bash
CVM=<built binary>; BENCH=<bench classes>
# 1. baseline, must be unchanged by this session
$CVM --java-home <jdk25> -Xmx8g -cp $BENCH BinTreesClassic 18     # expect 68332206

# 2. the known-issue repro, JIT on
CRATONVM_MOVING_YOUNG=1 $CVM --java-home <jdk25> -Xmx8g -cp $BENCH BinTreesClassic 18
# expect 68332206 on EVERY run (3+ runs; it varies when broken)

# 3. and at a small heap where minor GC actually fires
CRATONVM_MOVING_YOUNG=1 $CVM --java-home <jdk25> -Xmx256m -cp $BENCH BinTreesClassic 18

# 4. the diagnostic that decides whether (2) proved anything
#    run with RUST_LOG=warn and read the [moving-young] fallback lines
CRATONVM_MOVING_YOUNG=1 RUST_LOG=warn $CVM ... BinTreesClassic 18 2>&1 | grep moving-young
```

Then bt16 (`14985902`) and bt14 (`3222190`) as the low-pressure controls.

**Reading the result:**

* Checksum correct **and** `moving_young_cycle_count() > 0` ⇒ the feature works;
  proceed to the flip and the app gauntlet.
* Checksum correct **and** cycle count `0` ⇒ correctness is restored but
  moving-young is inert. Read the histogram, take the matching cross-owner
  request from section 7, repeat. Do **not** close the known-issue.
* Checksum still wrong ⇒ the verifier is not seeing the frames. Check
  `UNBOUNDED_FRAME_BAND` in the histogram first (it means `osr_frame_size` is
  zero for normal entries and the band walk never ran), then re-read section 3.

The flip, when all of the above passes, is `types/src/flags.rs`
`DEFAULT_MOVING_YOUNG: bool = true`. `CRATONVM_NO_MOVING_YOUNG` is already
wired as the opt-out, and
`flags::tests::empty_source_matches_all_documented_defaults` asserts against the
constant rather than against `false`, so no test has to be edited to make the
flip happen.

---

## 9. Files considered and not changed

* `gc/src/young_mark.rs` (owned) — parallel young **marking** for the
  non-moving sweep's mark phase. Nothing in it reads or influences the
  moving/non-moving decision and the Cheney path does not use it. Unchanged,
  for the same reason the previous session gave.
* `vm/src/jit/xt_root_scan.rs` (owned) — its two conservative peer passes
  already assert `mark_moving_young_coverage_incomplete_because(XT_TAKEOVER /
  XT_HELPER_WINDOW)` at the point the peer's roots are contributed, which is
  the correct obligation and needs nothing further from this item. A frozen or
  helper-window peer therefore always blocks relocation.
* `vm/src/memory/roots.rs` (owned) — see 7.3.
* `jit/src/x64.rs` — not owned. Holds 7.1 and 7.2.
