# Precise JIT Stack Maps as the Validated Default

Status: **largely landed, NOT yet fully validated as a trusted default.** The
mechanism (`CRATONVM_PRECISE_JIT_MAPS`) is already **default-on** on `dev` (opt
out with `CRATONVM_NO_PRECISE_JIT_MAPS`). What remains is the work to make it a
*validated* default — perf, a full app-gauntlet GC-root sweep, and closing the
two still-open family members it does **not** fix. This doc separates what is
done from what "validated default" still requires.

> Note on sources: the orchestration brief referenced
> `docs/internal/reviews/full-review-2026-06-20.md`. That file does **not** exist
> in this tree; the authoritative grounding for this topic is in the
> known-issues family docs and the JIT/GC source, cited inline below. The
> general crate reviews `docs/internal/reviews/jit-review.md` and
> `docs/internal/reviews/gc-review.md` are the closest standing reviews and are
> referenced where relevant.

## Problem & motivation

CratonVM's young-generation GC must run while JIT-compiled frames are live. A
JIT frame holds live object references in spill slots and CPU registers. The
legacy root scan over those frames is **conservative**: it walks the native
stack band in 8-byte strides and treats any qword that
`heap.is_object_address` accepts as a root
(`vm/src/jit/conservative_roots.rs::scan_one_frame`, called from
`scan_active_jit_frames_with_sp`). Two consequences follow:

1. **Register-invisibility.** A live oop whose only copy sits in a
   *callee-saved register of a caller frame* at a deep young-GC safepoint is not
   on the stack band the conservative scan inspects, so it is neither marked nor
   pinned. Under selective promotion the object is evacuated and the young slot
   is zeroed; the stale reference later reads an all-zero header → NPE / CCE /
   `gen_heap::get_field … class_id=0` / SIGSEGV. This is the
   **GC-root-coverage-under-JIT family** (A1–A4) in
   `docs/known-issues/README.md`.
2. **No safe relocation.** A conservatively-discovered slot cannot be
   *rewritten* (a non-oop qword that merely looks like an address must not be
   clobbered), so the moving (Cheney) collector is forbidden under live JIT
   frames (`gc/src/gen_heap.rs` quiescence gate, `:2207` SAFETY comment). That
   is why the non-moving sweep is the only young collector that runs under JIT
   — the throughput wall behind `default-moving-young-gen.md`.

Precise JIT stack maps fix (1) directly: the GC walks **every** active JIT frame
via an exact-RBP chain and consults a per-safepoint oop map, so the
register-invisible caller-frame root is found. They are also the prerequisite
for (2) (safe slot rewriting), though the moving path is **not** the path that
shipped.

## Current state in the codebase (what is ACTUALLY there)

### The shipping mechanism: `CRATONVM_PRECISE_JIT_MAPS`, default-on

- **Gate.** `cratonvm_jit::x64::precise_jit_maps_enabled()`
  (`jit/src/x64.rs:1826`) is **default-on**, opt out with
  `CRATONVM_NO_PRECISE_JIT_MAPS`. The rustdoc there (`:1810`–`:1825`) states it
  is the fix for the GC-root-coverage-under-JIT family (SB-CRASH-04 / A2 / A4)
  and records the verified checksums (`bintrees16/18 == 14985902 / 68332206`).
- **Frame registration (exact RBP).** The prologue emits a call to
  `jit_frame_record` (`vm/src/jit/helpers.rs:5306`) →
  `conservative_roots::set_top_frame_base(rbp)`
  (`vm/src/jit/conservative_roots.rs:971`), recording the EXACT RBP of the
  innermost active JIT frame after `mov rbp, rsp`. The helper-table
  `frame_record` field is populated only when `precise_jit_maps_enabled()`
  (`vm/src/jit/helpers.rs:5277`). Because release builds omit frame pointers,
  this explicit registration (plus the walkable JIT `push rbp; mov rbp,rsp`
  RBP chain) is how the GC reaches caller frames' slots.
- **Per-safepoint oop maps.** `jit/src/x64.rs` builds the maps via
  `compute_local_oop_masks` / `emit_oop_map_for_safepoint`, keyed on a
  `sp_id` (the bytecode PC of the active safepoint) stored in a reserved frame
  slot at `[rbp - sp_id_slot_off]` (`Compiler.sp_id_slot_off`, `x64.rs:5927`).
  `emit_pre_safepoint_spill` already flushes register-resident locals to their
  canonical frame slots, so the map records them.
- **Precise relocation walker.** `conservative_roots::remap_active_jit_frames`
  (`vm/src/jit/conservative_roots.rs:991`) walks each precise frame
  (`cm.sp_id_slot_off != 0`), reads the active `sp_id`, finds the matching
  `OopMapEntry`, and rewrites moved slots via the `pointer_map`. The marking
  path (`scan_active_jit_frames_with_sp`, `:943`) dispatches precise frames to
  `scan_one_frame_precise` and falls back to `scan_one_frame` for frames
  pushed without precise metadata. The precise path is **additive over a
  conservative backstop** — it never *loses* a root relative to the legacy
  scan, it only *adds* the register-invisible ones.
- **Coverage flag.** `CompiledMethod::fully_oop_covered` (`jit/src/lib.rs:921`)
  is computed at finalize (a method is covered only when every spilled
  safepoint also recorded an oop map: `safepoint_pcs ⊆ mapped_safepoint_pcs`,
  `x64.rs:5952`–`5961`). This is the hook for a future coverage-gated
  defer-lift, but the shipping path does **not** rely on it for correctness
  because the conservative backstop still runs.

### The collector policy it rides on (also default-on, separate gate)

Precise maps make the **non-moving young sweep + selective promotion** correct;
they do not introduce a moving collector. Selective promotion is itself
default-on: `gen_heap.rs:3626` reads `CRATONVM_NO_SELECTIVE_PROMOTE` (opt-out),
and the `gen_heap.rs` "Fix A (2026-06-05)" comments document that the
non-moving sweep + selective promotion is the *correct* young collector under
JIT (it over-marks safely, **pins** conservative roots, and tenures heap-interior
nodes to drain young). bt18 = 68332206 = HotSpot is delivered by this pair, not
by a moving GC. See `docs/feature-designs/default-moving-young-gen.md` for the
moving-default project that would supersede the throughput half.

### The superseded mechanism: `CRATONVM_SHADOW_STACK` (do NOT conflate)

A *separate*, earlier mechanism — the per-thread shadow stack
(`gc/src/shadow_stack.rs`, gate `CRATONVM_SHADOW_STACK`,
`x64.rs::shadow_stack_maps_enabled` `:1838`) — pushes every live oop onto a flat
per-thread array at each safepoint so a **moving** collector can rewrite them.
Its current status (per `precise-jit-stack-maps-followups.md` and the
known-issues README):

- It is **default-OFF** and **superseded for bt18 correctness**. With the gate
  on, the moving Cheney path **under-counts** bt18 to 67674804 (the historical
  "golden" that was later proven WRONG; HotSpot = 68332206).
- Its OSR-frame sub-gate `CRATONVM_SHADOW_OSR_TRACK`
  (`jit/src/lib.rs:1463`, default-OFF) is only a **partial** correctness fix
  (moves bt18 67674804 → 68199090, still short of 68332206).
- **It must not be combined with precise maps** — the two interfere and
  reclaim (`SB-SUITE-CRASH-04` update #5). Precise maps alone are the path.
- The shadow infrastructure (multi-thread scan in `roots.rs`/`gc.rs`, unwind
  reset in `helpers.rs::set_jit_thread`/`restore_jit_thread`) remains in the
  tree but is opt-in scaffolding for a future moving path, not the correctness
  path.

### What is validated vs NOT (the honest line)

| Item | State | Evidence |
|---|---|---|
| Precise maps default-on (non-moving sweep + selective-promote) | ✅ **landed + correctness-verified on the GC microbenchmarks** | `SB-SUITE-CRASH-04` update #5: A3 (`MinRegexProbe`) green at `GC_STRESS=524288` **and** 4 MB; bt16=14985902; bt18=**68332206**=HotSpot; matrix600/sieve250k/fib44 correct. Resolved on dev `32649b56`. |
| A2 (`ReflRepro`) register-resident UAF | 🔴 **OPEN** — precise maps do NOT fix it | `reflrepro-register-resident-jit-root-handoff.md`: distinct sweep-walker use-after-free; precise maps *retain more* and surface *more* corruption, verified still-crashing 2026-06-17/18. |
| A4 (`Fork6`, FJP multi-thread) | 🟡 **OPEN / inconclusive** — gated, separate CAS bug masks it | `fork6-fjp-multithread-jit-root-reclamation.md`: only reachable under experimental `CRATONVM_REAL_FORKJOINPOOL=1`; a real-FJP `ForkJoinPool` CAS conflict now fails the repro on both precise-on and precise-off. |
| Moving-GC precise relocation (`remap_active_jit_frames` under a real move) | ⚠️ **implemented, not exercised on the default path** | The non-moving sweep + selective promote does not relocate JIT-held slots, so `remap_active_jit_frames` is inert by default; it is only load-bearing if a moving young gen is ever made default. |
| OSR-point precise tracking | ⚠️ **partial / opt-in only** (`CRATONVM_SHADOW_OSR_TRACK`) | Shadow-path only (68199090, not 68332206); the precise-maps path relies on the conservative backstop for OSR frames, not on OSR oop maps. |
| Full app-gauntlet GC-root regression with precise on | ❌ **NOT done** | `SB-SUITE-CRASH-04` #5: "Either needs a full app/bench regression sweep." |
| Perf acceptable as default | ⚠️ **known regression on call-heavy code** | ~6% alloc-heavy (bt18), ~0% compute, **2.5× on pure call-heavy recursion** (fib44 8.5 s → 21.5 s) from the per-invocation `jit_frame_record` CALL. |

## Proposed design (to reach a *validated* default)

The mechanism is correct; "validated default" is about (a) making the cost
acceptable, (b) proving it across the app gauntlet, and (c) being explicit about
the family members it does and does not retire. No new GC algorithm is proposed
here — that is `default-moving-young-gen.md`.

1. **Inline the frame-record (perf).** Replace the per-invocation
   `call jit_frame_record` with an inlined RBP store into the thread's
   JIT-frame-chain top: cache the chain-top address in a prologue frame slot
   (exactly as the shadow path caches the thread pointer), then each entry is a
   single `mov`, no CALL. This is the documented viable perf fix
   (`SB-SUITE-CRASH-04` #5); the NOP-skip lever is **unsafe** because the
   frame-record anchors the RBP-chain walk and skipping it for an oop-free
   intermediate frame breaks traversal to oop-bearing caller frames.
2. **Coverage-gated trust (robustness).** Surface `fully_oop_covered`
   (`jit/src/lib.rs:921`) at GC time so a frame that is not fully precisely
   covered (un-mapped safepoint, inlined callee, OSR frame) **pins** its
   referents conservatively rather than being trusted for relocation. With the
   non-moving sweep this is already safe (the conservative backstop pins
   everything); the value is making the invariant explicit and a precondition
   for any future moving-default flip.
3. **App-gauntlet GC-root acceptance sweep.** Run the regression pool and the
   GC-root-family repros (A1–A4 + the kafka bug-21/22 / tomcat-style
   register-invisibility reclaims named in the README) with precise on, against
   HotSpot, and record a green baseline. This is the actual gate on "validated".
4. **Scope honesty for A2 / A4.** Document, in the family README and here, that
   precise maps fix the **register-invisibility class (A3)** but NOT A2 (a
   distinct allocation↔sweep-walker UAF) nor A4 (multi-thread, gated, separate
   CAS bug). These are tracked separately and are NOT blockers for the
   single-thread default; they ARE blockers for declaring the *family* retired.
5. **OSR decision.** Either (a) extend precise oop maps to OSR entry points so
   OSR frames are precisely covered (not just conservatively backstopped), or
   (b) formally accept conservative backstop for OSR frames and drop the
   shadow-only `CRATONVM_SHADOW_OSR_TRACK` partial path. The default path does
   not need OSR precision today, so this is a clarity/robustness item, not a
   correctness blocker.

## Incremental delivery plan (small, independently-mergeable, each build-green)

Each step compiles and keeps the current default behaviour unless noted; the
default is *already* precise-on, so most steps are perf/robustness/validation,
not behaviour flips.

- **Step 0 — Documentation of record (this doc).** Land the design; correct the
  README cross-refs so the shadow-stack vs precise-maps distinction and the
  A2/A4 scope are unambiguous. No code. Build-green by construction.
- **Step 1 — Inline frame-record scaffolding (no behaviour change).** Add the
  prologue chain-top cache slot and a feature flag
  `CRATONVM_PRECISE_INLINE_FRAME_RECORD` (default-**off**) that, when on, emits
  the inlined `mov` instead of the `call`. Keep the `call` path as the default
  until validated. Build-green; A/B-able.
- **Step 2 — Validate + flip the inline frame-record.** Prove A3 + bt16/bt18 +
  fib44 (perf) on the inlined path; flip `CRATONVM_PRECISE_INLINE_FRAME_RECORD`
  default-on (opt-out). Removes the 2.5× call-heavy regression.
- **Step 3 — Coverage-gated pin (robustness, no behaviour change under
  non-moving sweep).** Wire `fully_oop_covered` into the GC so un-covered
  frames pin conservatively; assert via `CRATONVM_DBG_VERIFY_OOP_MAPS`-style
  check that the precise map covers every conservatively-found oop. Build-green;
  byte-identical on the current default (the backstop already pins).
- **Step 4 — App-gauntlet GC-root acceptance sweep.** Add a
  `test-infra/regression-pool` lane (or extend the existing one) that runs the
  A1–A4 repros + the named register-invisibility apps with precise on vs
  HotSpot and records baselines. Doc-only + harness; no VM behaviour change.
- **Step 5 — OSR decision (separate, optional).** Either extend precise oop
  maps to OSR entry or formally retire `CRATONVM_SHADOW_OSR_TRACK`. Independent
  of the default flip.
- **Step 6 — Shadow-stack retirement decision (separate).** Once the moving
  default project (`default-moving-young-gen.md`) is decided, either keep
  `CRATONVM_SHADOW_STACK` as the moving-relocation scaffolding or remove it.
  Out of scope for *this* doc beyond noting the dependency.

## Risks & open questions

- **Perf regression on call-heavy code (known).** 2.5× on pure-call recursion
  from the per-call `frame_record`. Mitigation = Step 1/2 inline. Risk: an
  inlined store that mis-tracks the chain top breaks the walk silently (missed
  roots → reclaim). Mitigation: A/B against the call path + the A3 repro at high
  GC stress before flipping.
- **The walk is load-bearing and cannot be skipped per-method.** The NOP lever
  was tried and **reverted** (broke `MinRegexProbe`). Any future "skip for
  oop-free methods" optimisation must preserve RBP-chain traversal to caller
  frames.
- **A2 is NOT fixed and precise-on surfaces MORE of it.** Retaining more live
  objects exposes the distinct `ReflRepro` allocation↔sweep-walker UAF earlier.
  Open question: does the app gauntlet hit A2-class churn anywhere that was
  previously masked by under-retention? The Step 4 sweep is partly to find out.
- **A4 is gated and currently masked by a real-FJP CAS bug.** Cannot be cleanly
  re-verified until that CAS bug is fixed; precise maps neither fix nor regress
  it on current evidence.
- **Moving relocation path is unexercised by default.**
  `remap_active_jit_frames` correctness under a real move is only proven on the
  (superseded, under-counting) shadow path. If the moving-default project flips,
  this code becomes load-bearing and needs its own validation — do not assume it
  inherits the non-moving validation.
- **Multi-thread precise scan.** Marking/remap of *other* threads' JIT frames
  follows the existing per-thread publish/resume protocol; it is exercised by
  the regression pool's multi-threaded apps only lightly. A dedicated
  multi-threaded-JIT-under-GC stress is wanted (shared with A4).
- **Source dating in the older docs.** `precise-jit-stack-maps-design.md` and
  `…-findings.md` predate the 2026-06-05 ground-truth correction and still cite
  the wrong "golden 67674804". Treat `…-followups.md` and the known-issues
  README as authoritative for checksums.

## Validation / acceptance (how to prove it)

A precise-maps default is **validated** when all of the following hold, each
cross-checked against HotSpot (`java -cp bench BenchSuite …` and the real
JDK-25 app runs):

1. **GC microbenchmarks == HotSpot, default config (no env overrides):**
   bt10=135854, bt14=3222190, bt16=14985902, **bt18=68332206**. (Already met on
   dev `32649b56`; must remain green after the inline-frame-record flip.)
   Reproduce: `target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25"
   --Xmx 8g -cp bench BenchSuite bintrees18`.
2. **A3 register-invisibility repro green at high GC stress:** `MinRegexProbe`
   correct at `GC_STRESS=524288` **and** 4 MB (the two stress levels in
   `SB-SUITE-CRASH-04`).
3. **No correctness regression across the regression pool** (`test-infra/
   regression-pool/run.sh`) with precise on, all probes == their HotSpot/
   gate-off baselines; the multi-threaded apps (cassandra, activemq, tomcat,
   wildfly, keycloak, spring-boot, jenkins, felix) green.
4. **App-gauntlet GC-root family baseline recorded** (Step 4): the named
   register-invisibility apps run to completion == HotSpot with precise on.
5. **Perf within budget after inline frame-record:** call-heavy regression
   reduced from 2.5× toward parity (fib44), alloc/compute unchanged (≤~6% /
   ~0%).
6. **Scope statement accurate:** A2 and A4 are explicitly tracked as OPEN and
   NOT claimed closed by this work; the README family map matches reality.

"Validated default" does **not** require A2 or A4 to be fixed (they are
separate bugs), but it **does** require the docs to stop implying the family is
retired. Retiring the *family* is a superset goal that depends on A2's
sweep-walker fix and A4's FJP CAS fix landing separately.

## Scaffolding to land first (minimal, compiling)

These are the small, build-green additions that should precede the validation
work. They are described here; this doc does not implement them.

- **`CRATONVM_PRECISE_INLINE_FRAME_RECORD` feature flag** (default-off), read
  once and cached in `jit/src/x64.rs` next to `precise_jit_maps_enabled()`.
  Gates whether the prologue emits the inlined chain-top `mov` (new path) or the
  existing `call jit_frame_record` (current default). Off → byte-identical.
- **Prologue chain-top cache slot.** Reserve one frame slot (like
  `shadow_thread_slot_off`) holding the address of the thread's JIT-frame-chain
  top, set once in the prologue; the inlined path stores RBP there. Adds a
  `Compiler` field analogous to `sp_id_slot_off`; 0 when the inline flag is off.
- **`CRATONVM_DBG_VERIFY_OOP_MAPS` assertion knob** (default-off): at each GC,
  assert the precise per-frame map covers every conservatively-found oop in the
  frame (completeness check with the backstop still running → zero behaviour
  change). Surfaces map gaps before they can ever matter for relocation.
- **Coverage plumbing.** Expose `CompiledMethod::fully_oop_covered`
  (already present, `jit/src/lib.rs:921`) to the GC marking/remap path behind a
  default-off `CRATONVM_PRECISE_COVERAGE_PIN` knob so the pin-on-uncovered
  policy can be A/B-tested before becoming unconditional.
- **Regression-pool GC-root lane.** A `test-infra/regression-pool` config that
  runs the A1–A4 repros (`docs/known-issues/repros/A2-reflrepro`,
  `…/A4-fork6`, `MinRegexProbe`) + the named register-invisibility apps with
  precise on and diffs against HotSpot baselines. Harness/config only.
