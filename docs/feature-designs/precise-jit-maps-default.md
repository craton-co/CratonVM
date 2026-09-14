# Precise JIT stack maps

**Status:** Shipped (default on; `CRATONVM_NO_PRECISE_JIT_MAPS` opts out).

## What it does today

The collector gets a precise description of the oops live in a compiled frame
instead of scanning conservatively. `precise_jit_maps_enabled()` in
`jit/src/x64/licm.rs` is literally
`runtime_var_os("CRATONVM_NO_PRECISE_JIT_MAPS").is_none()` — on unless you turn
it off. Consumers are `vm/src/jit/helpers.rs`,
`vm/src/runtime/interpreter/jit_bridge.rs`, `vm/src/runtime/interpreter.rs`,
`jit/src/ir_lower.rs`, `jit/src/lib.rs` and `vm/src/memory/gc.rs`.

**`CRATONVM_PRECISE_JIT_MAPS` is now a no-op.** It was the enabling switch
before the default flip; setting it changes nothing.

The frame record is inlined rather than called — `mov gs:[disp], rbp` instead
of a `CALL` — which is what made the default affordable on call-heavy code.

Several companion precision features are also default-on and, like the master
switch, are spelled as opt-outs: `CRATONVM_NO_PRECISE_REG_SPILL`,
`CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES`,
`CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD`,
`CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME`,
`CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES`, `CRATONVM_JIT_NO_PRECISE_FIELD_OPS`.

Default-off diagnostics and experiments: `CRATONVM_JIT_SAFEPOINT_POLLS`,
`CRATONVM_JIT_SAFEPOINT_REG_SPILL`, `CRATONVM_STRICT_JIT_ROOTS`,
`CRATONVM_PRECISE_COVERAGE_PIN`, and the superseded `CRATONVM_SHADOW_STACK`
(retained as experimental scaffolding — do **not** conflate it with the precise
maps; it is a different mechanism).

**OSR frames take the conservative backstop.** That was accepted deliberately
rather than deferred.

## What is not built yet

This is a validation gap, not an implementation gap:

- **The cross-thread stop-the-world scan** has not been exercised, so the
  GC-root defect family it belongs to cannot be formally retired.
- **The container-app gauntlet** has not been run on CI against this default.

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
  (`osr_shadow_track_enabled`, `jit/src/lib.rs`, default-OFF) is only a
  **partial** correctness fix (moves bt18 67674804 → 68199090, still short of
  68332206). The precise-maps default accepts the conservative backstop for OSR
  frames (Step 5), so this sub-gate is *not* the correctness path. **Step 6
  decision (2026-06-22): RETAINED as experimental, default-off scaffolding —
  kept, not removed** (paired with `CRATONVM_SHADOW_STACK`).
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
| A2 (`ReflRepro`) heap corruption | ✅ **FIXED on dev** (`6e3ddb05`) — was GC-side free-list accounting, **not** a register-resident root nor a precise-maps concern | `reflrepro-register-resident-jit-root-handoff.md` (now headed FIXED): the non-moving young sweep's coalescer merged only *adjacent* free blocks; *overlapping* ones both survived and `Arena::alloc` double-served the region → overlapping live objects → linear-walk desync. Fix coalesces overlapping blocks too. `ReflRepro 8000 @ GC_STRESS=65536` → `ok=8000 bad=0`; re-verified 2026-06-29. Precise maps are orthogonal to this. |
| A4 (`Fork6`, FJP multi-thread) | FIXED on dev (2026-07-09) | `fork6-fjp-multithread-jit-root-reclamation-FIXED.md`: real-FJP validation now passes after live-blocked STW accounting, correct `Unsafe.compareAndExchange*` CAS witnesses, and all-live reference-local scans on the non-moving snapshot path. |
| Moving-GC precise relocation (`remap_active_jit_frames` under a real move) | ⚠️ **implemented, not exercised on the default path** | The non-moving sweep + selective promote does not relocate JIT-held slots, so `remap_active_jit_frames` is inert by default; it is only load-bearing if a moving young gen is ever made default. |
| OSR-point precise tracking | ✅ **decided (Step 5): conservative backstop accepted; no precise OSR maps** | OSR frames get a `JitEntryGuard` chain entry + are conservatively backstopped in `scan_one_frame_precise` (safe on the non-moving sweep) and are excluded from `fully_oop_covered` (`!compiled_via_osr`) so a future moving path PINS them. Empirically: bt16 OSR-enters `binaryTrees(I)J` and is correct under forced young GC. `CRATONVM_SHADOW_OSR_TRACK` (shadow-only, partial 68199090) is retained experimental/default-off (Step 6). See "Step 5 — OSR decision". |
| Full app-gauntlet GC-root regression with precise on | 🟡 **repro+bench lane + app lane LANDED & green; heavy container apps still CI-deferred** | `gc-root-lane.sh` (Step 4): 12 PASS / 1 FLAKY (MTRegex), 0 dev (the former A2 KNOWN-FAIL now PASSes after `6e3ddb05`; lane expectation updated to PASS). `gc-root-apps-lane.sh` (Step 7): BouncyCastle asn1+prng PASS (GC-invariant + corruption-free under 4 MB GC-stress), 0 dev. The named container apps (wildfly/kafka/h2/elasticsearch/…) are unbuilt on a dev checkout → SKIP-with-note; they need CI / a provisioned box. See "Step 7 — App GC-root gauntlet". |
| Perf acceptable as default | ✅ **inline frame-record landed (Steps 1+2)** | The per-invocation `jit_frame_record` CALL is now an inlined `mov gs:[disp], rbp` (default-on, Windows). fib44 inline-on ~11.8 s vs CALL-path ~19.8 s = **1.68× faster** (the no-frame-record floor is ~9 s, so inline cuts ~74 % of the frame-record overhead). bt16 also ~16 % faster. See "Inline frame-record (Steps 1+2)" below. Opt out: `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD`. |

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
- **A2 is FIXED (`6e3ddb05`), and it was never a precise-maps concern.** The
  premise that precise-on "surfaces more of A2" was based on the now-refuted
  register-resident-root theory; the real cause was a GC free-list double-serve,
  fixed independently. Re-verified clean 2026-06-29 (`ReflRepro` `bad=0` at both
  the standard and the harsher lane stress). No residual risk to the precise-maps
  default from A2. *(Historical note: precise-on retaining more live objects did
  make the corruption surface earlier — that was a symptom-timing effect, not a
  cause; the fix is in the sweep coalescer.)*
- **A4 is gated; non-fatal on the repro but the cross-thread gap is real.** On
  current dev `Fork6` is 26/26 ALL-OK (re-verified 2026-06-29) with the benign
  real-FJP `cas_long` retry noise still firing; the `scan_active_jit_frames`
  cross-thread STW JIT-root gap is still *exercised* (WARN) but covered by the
  peer's `root_snapshot`. The genuine residual is a register-only oop at a
  non-call safepoint — the deferred precise-reg-map / cross-thread-STW-scan work.
  Precise maps neither fix nor regress it on current evidence.
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
3. **No correctness regression across the GC-root lane** (`test-infra/
   regression-pool/gc-root-lane.sh`) with precise on, all probes == their
   HotSpot baselines (0 deviations). ✅ met 2026-06-22 for the repro+bench set
   (see "Step 4 baseline"). The multi-threaded full apps (cassandra, activemq,
   tomcat, wildfly, keycloak, spring-boot, jenkins, felix) still need container
   harnesses — outstanding.
4. **App-gauntlet GC-root family baseline recorded** (Steps 4 + 7): 🟡 partial —
   the GC-root *repro+bench* lane (Step 4) **and** the *app* lane (Step 7:
   `gc-root-apps-lane.sh`, BouncyCastle asn1+prng GC-invariant & corruption-free
   under forced GC) are recorded & green. The named container apps
   (cassandra/tomcat/wildfly/keycloak/spring-boot/jenkins/felix) are CI-deferred
   (unbuilt on a dev checkout) — the remaining bar.
5. **Perf within budget after inline frame-record:** call-heavy regression
   reduced from 2.5× toward parity (fib44), alloc/compute unchanged (≤~6% /
   ~0%).
6. **Scope statement accurate:** A4 is explicitly tracked as OPEN and NOT claimed
   closed by this work; A2 is now independently FIXED (`6e3ddb05`); the README
   family map matches reality (updated 2026-06-29).

"Validated default" does **not** require A4 to be fixed (it is a separate bug),
but it **does** require the docs to stop implying the family is retired. Retiring
the *family* is a superset goal that — now that A2 has landed — depends on A4's
cross-thread-STW JIT-root scan (+ the real-FJP CAS bug) and the container-app CI
run landing separately.

