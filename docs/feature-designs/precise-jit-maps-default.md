# Precise JIT Stack Maps as the Validated Default

Status: **Steps 1–8 of the delivery plan landed (2026-06-21/22). The
precise-maps default closes the A3 register-invisibility class and is green
across every GC-root vehicle runnable on this box (repros, benches, OSR, and the
BouncyCastle app suites). **A2 is now also FIXED on `dev`** (commit `6e3ddb05`,
GC-side overlapping-free-block coalescing — it was never the "register-resident
missed root" the title chased; see the A2 known-issue doc). So formally retiring
the *family* now needs only the A4 cross-thread-STW scan and the container-app
gauntlet on CI — see "Step 8 — GC-root family retirement status".**
The mechanism (`CRATONVM_PRECISE_JIT_MAPS`) is **default-on** on `dev` (opt out
`CRATONVM_NO_PRECISE_JIT_MAPS`). **Steps 1+2** closed the call-heavy perf
regression (frame-record CALL → inlined `mov gs:[disp], rbp`, default-on, fib44
1.68× faster, bintrees == HotSpot). **Step 3** added the default-off coverage/
verify GC oracles. **Step 4** recorded the GC-root acceptance lane baseline
(12 PASS / 1 KNOWN-FAIL[A2] / 1 FLAKY[MTRegex] — the A2 KNOWN-FAIL has since
flipped to PASS, below). **Step 5** accepted the conservative backstop for OSR
frames. **Step 6** retained the shadow stack as experimental, default-off
scaffolding (kept, not removed). What remains for a fully *validated* default:
the full named-app gauntlet, plus the one still-open family member (A4) it does
**not** fix. This doc separates what is done from what "validated default" still
requires.

> **Re-verification 2026-06-29** (fresh release build off `dev` HEAD `9928052c`,
> JDK-25 HotSpot oracle, this box). A2 ReflRepro `8000 @ GC_STRESS=65536` →
> `ok=8000 bad=0 rc=0`, and at the harsher lane params (`20000 @ GC_STRESS=524288`,
> `--Xmx 256m`) → `ok=20000 bad=0 rc=0`; A3 `VAAload 14 @ GC_STRESS=4096` =
> `3222190`; `bintrees16 = 14985902`, `bintrees18 = 68332206` (== HotSpot); A4
> `Fork6` under `CRATONVM_REAL_FORKJOINPOOL=1` = **26/26 ALL-OK** (8 sequential +
> 18 concurrent-stress), 0 corruption — the cross-thread STW JIT-root *gap* is
> still **exercised** (`scan_active_jit_frames` WARN, `cross_thread_jit_gap_hits`
> incrementing) but **non-fatal** (covered by the peer's last-published
> `root_snapshot`); the benign real-FJP `cas_long FAIL` retry noise still fires.

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
| Perf acceptable as default | ✅ **inline frame-record landed (Steps 1+2, 2026-06-21)** | The per-invocation `jit_frame_record` CALL is now an inlined `mov gs:[disp], rbp` (default-on, Windows). fib44 inline-on ~11.8 s vs CALL-path ~19.8 s = **1.68× faster** (the no-frame-record floor is ~9 s, so inline cuts ~74 % of the frame-record overhead). bt16 also ~16 % faster. See "Inline frame-record (Steps 1+2)" below. Opt out: `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD`. |

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
   precise maps fix the **register-invisibility class (A3)** but NOT A2 nor A4 —
   these are *distinct* bugs. **A2 turned out to be a GC free-list double-serve,
   fixed independently (`6e3ddb05`, 2026-06-23)**; A4 is multi-thread, gated, with
   a separate CAS bug. Neither was ever a precise-maps concern. They are tracked
   separately and are NOT blockers for the single-thread default; **A4 (only)**
   now remains a blocker for declaring the *family* retired.
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
- **Step 1 — Inline frame-record scaffolding (no behaviour change). ✅ DONE**
  (2026-06-21). Feature flag `CRATONVM_PRECISE_INLINE_FRAME_RECORD`; when on,
  the prologue emits a single `mov gs:[disp], rbp` instead of
  `call jit_frame_record`. Landed default-**off** first (byte-identical CALL
  path); see "Inline frame-record (Steps 1+2)" below for the design that
  *supersedes* the "chain-top cache slot" sketch — no frame slot is needed, the
  store is one instruction into a startup-probed Windows TLS slot. Build-green;
  A/B-able.
- **Step 2 — Validate + flip the inline frame-record. ✅ DONE** (2026-06-21).
  Validated on the inlined path: bintrees10/14/16/18 == HotSpot, fib44 **1.68×
  faster** than the CALL path, `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`
  self-check clean (0 mismatches). Flipped to **default-on**, opt-out
  `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD`. Removes the call-heavy regression.
  (The full app-gauntlet sweep — Step 4 — remains the final bar before the
  *family* is declared retired; this flip satisfies the doc's Step-2 gate.)
- **Step 3 — Coverage-gated pin (robustness, no behaviour change under
  non-moving sweep). ✅ DONE** (2026-06-21). Two default-OFF, read-only knobs in
  `scan_one_frame_precise` (`conservative_roots.rs`), byte-identical on the
  default path: (a) `CRATONVM_PRECISE_COVERAGE_PIN` surfaces
  `CompiledMethod::fully_oop_covered` at GC scan, counting precise frames that
  are NOT fully covered (the backstop already pins them, so this is visibility +
  the explicit scaffold for the future moving-path "pin-don't-relocate"
  policy); (b) `CRATONVM_DBG_VERIFY_OOP_MAPS` diffs the union of a method's oop
  maps against the conservative band and logs any in-band oop no map records —
  the completeness oracle. CAVEAT documented in-code: the chain-entry band spans
  nested JIT→JIT callees (whose oops are legitimately absent from the boundary
  method's maps), so the oracle is sharpest for leaf-ish compiled frames — the
  register/spill-resident root class, e.g. the new
  `gc-stress-bintrees ... jit-frame-stale-root` known-issue and `codePointAt`.
- **Step 4 — App-gauntlet GC-root acceptance sweep. 🟡 LANDED for the
  repro+bench set; full 50-app lane still future** (2026-06-22). Added
  `test-infra/regression-pool/gc-root-lane.sh` — runs the GC-root-coverage
  family repros (`gc-stress-bintrees-main-args` ×5, A2 `ReflRepro`,
  `family-a/MTRegex`) + the GC microbenchmarks with precise default-on, diffs
  CratonVM vs HotSpot, records a baseline TSV under `results/`, and **exits
  non-zero on any deviation from the per-item expected status** (so it is a
  regression gate, not just a report). No VM behaviour change — harness only.
  See "Step 4 baseline" below. The named full apps (cassandra/tomcat/wildfly/…)
  need their own container/classpath harnesses (many are env-blocked here) and
  remain the outstanding bar — this lane is the runnable GC-root-family subset.
- **Step 5 — OSR decision (separate, optional). ✅ DONE — option (b)**
  (2026-06-22). **Formally accept the conservative backstop for OSR frames** on
  the default path; **do not** build precise OSR-entry oop maps (option a), and
  **deprecate** the shadow-only `CRATONVM_SHADOW_OSR_TRACK` (removal deferred to
  Step 6 with the shadow stack). Rationale + evidence in "Step 5 — OSR decision"
  below. No code change — a documented decision.
- **Step 6 — Shadow-stack retention decision (separate). ✅ DONE — KEEP as
  experimental, default-off** (2026-06-22). Decision: **retain**
  `CRATONVM_SHADOW_STACK` and its sub-gate `CRATONVM_SHADOW_OSR_TRACK` as
  experimental, default-off moving-relocation scaffolding — **remove nothing**.
  `default-moving-young-gen.md` is still *design / not started* (and the bt18 gap
  was found to be ctor-dispatch, not GC), so no moving young gen is imminent; the
  shadow path is the only partially-exercised relocation-remap code, it is
  byte-identical/zero-cost when off, and it is the starting point if a moving gen
  is ever pursued. Rationale + caveats in "Step 6 — Shadow-stack retention"
  below; gate rustdocs annotated EXPERIMENTAL/retained.

## Inline frame-record (Steps 1+2) — landed 2026-06-21

**What shipped.** The per-invocation `call jit_frame_record` in the JIT prologue
(`x64.rs`) is replaced, when enabled, by a single instruction:

```text
mov gs:[disp], rbp      ; 65 48 89 2C 25 <disp32>  (9 bytes, no CALL)
```

storing RBP straight into the precise-maps innermost-RBP mirror. Gate:
`precise_inline_frame_record_enabled()` (`x64.rs`), **default-on**, opt out with
`CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD`.

**Design note — supersedes the "chain-top cache slot" sketch.** The scaffolding
section below proposed caching the chain-top *address* in a frame slot (by
analogy with the shadow path's thread-pointer cache). That only helps if
obtaining the address is cheap, but the frame-record runs **once per
invocation**, so "fetch address (CALL) + store" is no cheaper than the original
CALL. The shipped design instead bakes the address as an immediate: the mirror
lives in a **Windows OS TLS slot** (`TlsAlloc`), whose `gs:[0x1480 + slot*8]`
displacement is recovered and **sentinel-probed at startup**
(`inline_rbp_tls_disp()`). No frame slot, no per-call address fetch — just one
`mov`. The probe is the safety net the Risks section demands: a unique 64-bit
sentinel is written via the documented `TlsSetValue` and read back through the
candidate `gs:[disp]` (with a fallback scan of the 64-slot static band); on any
mismatch (slot ≥ 64, unexpected TEB layout) **or** non-Windows, it returns 0 and
the CALL path is used. A wrong layout assumption therefore degrades to the
existing safe behaviour, never to a silently mis-tracked mirror.

**Single source of truth.** `inline_rbp_tls_disp()` (jit crate) is consulted by
both the codegen (which bakes `gs:[disp]`) and the VM-side mirror accessor
`top_rbp_get/set` (`conservative_roots.rs`, which reads/writes the same slot when
active, else the legacy `thread_local! TOP_RBP`). Codegen and GC can never
disagree on slot-vs-thread-local. The change is **behaviour-equivalent to the
CALL path for the mirror value** — same RBP, same program point, same per-thread
mirror, consumed identically; only the mechanism and cost differ. (On the
default *non-moving* sweep the mirror is inert anyway — `remap_active_jit_frames`
early-returns — so the default-path blast radius is nil; the mirror is
load-bearing only for the moving/relocation path.)

**Self-check.** `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` (default-off) wires a
verify helper into the `frame_record` slot; the prologue emits the inline store
*and* calls it to assert the mirror reads back the RBP just stored — proving the
hand-rolled `gs:[disp]` encoding lands exactly where the GC reads.

**Validation (this build, quiet host, JDK-25 HotSpot oracle):**

| Check | inline-OFF (CALL) | inline-ON (`mov gs:[disp]`) | HotSpot |
|---|---|---|---|
| bintrees10 checksum | 135854 | 135854 | 135854 |
| bintrees14 checksum | 3222190 | 3222190 | 3222190 |
| bintrees16 checksum | 14985902 | 14985902 | 14985902 |
| bintrees18 checksum | 68332206 | 68332206 | 68332206 |
| fib44 wall (bench `ms=`) | ~19.8 s | **~11.8 s (1.68×)** | — |
| bintrees16 wall | ~19.1 s | ~16.0 s | — |
| self-check mismatches (fib44, bt16) | n/a | **0** | — |

The no-frame-record floor (`CRATONVM_NO_PRECISE_JIT_MAPS`) is ~9 s on fib44, so
inline cuts ~74 % of the frame-record overhead (CALL added ~10.8 s; inline adds
~2.8 s). The TLS probe selected slot 15 → `gs:[0x14f8]` on the primary candidate
(0x1480 TEB constant confirmed; the scan fallback was not needed).

**Scope / residuals (not regressions of this work):**

- **Windows-only.** The single-instruction store relies on the Windows TEB TLS
  layout. On non-Windows `inline_rbp_tls_disp()` returns 0 → CALL path, so the
  default flip is a safe no-op there. A Linux `fs:`-based path is future work.
- **Multi-thread.** The mirror is per-thread (TLS), identical to the CALL path's
  per-thread `TOP_RBP`. The `MTRegex` GC-root stress repro is **flaky on both
  inline-on and inline-off** — it exercises the *documented, pre-existing*
  cross-thread STW JIT-root gap (`scan_active_jit_frames` warns; see Risks
  "Multi-thread precise scan") and a separate `Thread.join`/IMSE hang, neither
  touched by this change. It is therefore not a usable inline-vs-CALL oracle.
- **jit unit tests:** 820 pass; the only 4 failures are pre-existing
  `aarch64::tests::*overflows` (panic at `aarch64.rs:2241` identically on clean
  `dev` with these changes stashed) — unrelated AArch64 backend tests.

## Step 4 baseline — GC-root acceptance lane (recorded 2026-06-22)

`test-infra/regression-pool/gc-root-lane.sh` against the precise-default dev
binary, JDK-25 HotSpot oracle. Result: **12 PASS, 1 expected KNOWN-FAIL, 1
expected FLAKY, 0 deviations** (lane exit 0).

| item | result | vs HotSpot | note |
|---|---|---|---|
| bench bintrees10/14/16/18 | PASS | == (135854 / 3222190 / 14985902 / 68332206) | precise default-on |
| bench fib44 | PASS | == 701408733 | slow (~166 s) — the separate `3a966cb9` long-call regression; checksum correct |
| bench sieve250k / matrix600 | PASS | == (22044 / 6479950792) | |
| a3 RHard / VStatic | PASS | == 3222190 (3/3) | `warned=3` — GC-array-guard fires (gap exercised, benign) |
| a3 VAAload / VArgLen / binarytrees | PASS | == 3222190 (3/3) | `warned=3` — **see below** |
| a2 ReflRepro | KNOWN-FAIL *(at recording; **FIXED `6e3ddb05` 2026-06-23**, now PASS)* | crash (markers=251) → since `ok=20000 bad=0` at the lane params | was a GC free-list double-serve, **not** a precise-maps gap; lane expectation updated to PASS |
| mt MTRegex | FLAKY | 1/3 completions | documented cross-thread STW JIT-root gap + Thread.join/IMSE hang |

**Honest findings (the value of recording a baseline):**

- **The `gc-stress-bintrees-main-args` repros now produce the CORRECT checksum
  on current dev** (VAAload 8/8 verified), whereas the
  `gc-stress-bintrees-…-jit-frame-stale-root` known-issue recorded CRASH/WRONG on
  an *older* dev. **But the GC-array-guard fires on every run** (`warned=3`): the
  stale-root corruption is still *exercised* — the guard catches and drops the
  bad write — it is simply **non-fatal at this depth/stress**. So the gap is
  **masked, not closed**. The lane's automated gate flags a deviation only on a
  **status** change (PASS → crash/wrong); the `warned` count is recorded in the
  TSV note but is **not** auto-gated, so a human reviews it to see the guard
  firing more (a fresh gap) or — on a real fix — stopping. Precise maps on/off
  make no difference to this (per the known-issue).
- **A2 (`ReflRepro`) has since been FIXED** (`6e3ddb05`, 2026-06-23) — at
  recording it was an expected KNOWN-FAIL, but the root was a GC-side free-list
  double-serve (overlapping free blocks not coalesced), **not** anything precise
  maps touch. The lane's A2 expectation is now PASS. **The multi-thread
  cross-thread gap (`MTRegex`) remains open** — precise maps do not fix it; it is
  tracked as expected non-PASS so the lane stays green while it is worked
  separately.
- **bench `fib44` is correct but ~166 s** due to the separate `3a966cb9` long-call
  dev regression (bisected; workaround `CRATONVM_JIT_IR_LONG=0`). The lane
  disables the VM's 120 s native-hang watchdog (`CRATONVM_DISABLE_DEFAULT_WATCHDOG`)
  so the slow-but-correct run completes and its checksum is verified rather than
  aborted; the harness's own `timeout` is the backstop.
- **bt18@8g can transiently fail to allocate under concurrent host memory load**
  (one empty result observed); the lane retries a bench once on an empty result
  to absorb that, distinguishing it from a wrong checksum (a real regression).
- **bench `sieve250k` is correct but slow → needs a longer timeout on slow boxes
  (added 2026-06-29).** `sieve250k` = `sieve(250000)` repeated **1000×**; the hot
  `BenchSuite.sieve(II)J` is invoked exactly once (the 1000-repeat is its own inner
  loop) so it never hits invocation-count tier-up, and OSR does not fire on its
  loops — it runs **interpreted** end-to-end, ~70–350× slower than HotSpot, so 1000
  reps ≈ **350 s** on this (slow) box (measured: nojit `ms=347868 checksum=22044`;
  per-rep time scales perfectly **linearly** ⇒ NOT a hang/infinite-loop, and the
  checksum is always correct). It exceeded the lane's default 300 s `TIMEOUT` (and
  the 120 s watchdog), showing as the one `gc-root-lane.sh` deviation on a slow box.
  Fix: `bench_item` now takes a per-bench timeout override and `sieve250k` gets
  `max(TIMEOUT, 600)` (override via `SIEVE_TIMEOUT`). This is a JIT-throughput gap
  (hot once-invoked method never compiled — same class as bug-01), **not** a
  GC-root-family bug and **not** touched by precise maps.

This is the runnable GC-root-family acceptance set. The full named-app gauntlet
(cassandra/tomcat/wildfly/keycloak/spring-boot/jenkins/felix) needs container
harnesses and is the remaining bar before the *family* is declared retired.

## Step 5 — OSR decision (resolved 2026-06-22): accept the conservative backstop

**Decision: option (b).** OSR-entered frames are covered by the conservative
backstop on the default (precise, non-moving) path; CratonVM does **not** build
precise oop maps at OSR entry points, and the shadow-only
`CRATONVM_SHADOW_OSR_TRACK` is **retained as experimental, default-off**
scaffolding (Step 6 — kept, not removed). This is a clarity/robustness decision —
not a correctness change.

**Why OSR frames are already safe without precise OSR maps:**

- **They are scanned.** OSR entry wraps the compiled call in
  `JitEntryGuard::enter_with_compiled` (`vm/src/runtime/interpreter.rs`, right
  before `osr_enter`), so the OSR frame gets a precise `JIT_ENTRY_CHAIN` entry
  and is walked by `scan_one_frame_precise` at GC time.
- **The backstop pins their oops.** An OSR frame is entered via the trampoline,
  *not* the normal prologue, so it does **not** call `jit_frame_record` (no exact
  RBP) — its precise oop-map reads (relative to the approximate guard
  `frame_base`) are unreliable. But `scan_one_frame_precise` *also* runs the
  conservative band sweep (`scan_one_frame(scanner_sp, frame_base)`), which finds
  and pins every oop-looking word in the OSR frame. So no root is missed on the
  non-moving sweep.
- **A future moving GC pins them too.** `fully_oop_covered` is computed with
  `&& !cm.compiled_via_osr` (`jit/src/x64.rs`), so OSR-compiled methods are never
  "fully covered" — a moving collector keyed on that flag would **pin** (never
  relocate) OSR frames. Safe on both the current and any future path.

**Evidence (current dev `49954653`, JDK-25 oracle):** `bintrees16` OSR-enters
`BenchSuite.binaryTrees(I)J` (verified via `CRATONVM_DBG_OSR`; its locals hold
live `TreeNode` refs) and produces the correct checksum `14985902` with **zero
corruption markers** under forced young GC (`CRATONVM_DBG_GC_STRESS=65536`);
`bintrees18` = `68332206`. The Step-4 lane corroborates across the bench + repro
set. (A minimal standalone OSR-GC probe was tried and discarded — this VM's OSR
trigger fires for hot loops in *called* methods like `binaryTrees`, not for a
trivial counted loop, so `binaryTrees` is the correct vehicle.)

**Why option (a) is deferred (not done):** building precise OSR-entry oop maps
would require recovering each OSR frame's exact RBP and emitting per-OSR-entry
maps — work the default path does not need (the backstop is correct, and the
moving path pins OSR frames anyway). Note the deopt-osr work already emits
*OSR-exit* maps, but for a different consumer (deopt resume at a loop bci), not
GC roots; it does not change this decision.

**`CRATONVM_SHADOW_OSR_TRACK` status:** a sub-gate of the superseded shadow stack
that makes an OSR frame replicate the shadow push/reload so a moving Cheney
collector could relocate its oops. It is **partial** (bt18 67674804 → 68199090,
still short of the golden 68332206) and **regresses bt18**, and the shadow path
itself is not the correctness path. **Retained as experimental, default-off
(Step 6 — kept, not removed).**

## Step 6 — Shadow-stack retention decision (resolved 2026-06-22): KEEP experimental

**Decision: RETAIN, do not remove.** `CRATONVM_SHADOW_STACK` and its OSR sub-gate
`CRATONVM_SHADOW_OSR_TRACK` are kept as **experimental, default-off**
moving-relocation scaffolding. Nothing is deleted; only the gate rustdocs are
annotated EXPERIMENTAL/retained (`x64.rs::shadow_stack_maps_enabled`,
`lib.rs::osr_shadow_track_enabled`, `conservative_roots::shadow_stack_enabled`)
and this doc records the decision.

**Dependency status.** Step 6 was scoped as contingent on the moving-default
project. `default-moving-young-gen.md` is still **design / not started**, and the
Binary-Trees-18 throughput gap was separately root-caused to **per-node ctor
dispatch, not GC** (moving young gen measured as ~0% of the gap — a dead end for
that goal). So **no moving young gen is imminent**, and the shadow stack's
relocation-remap path is **not** load-bearing for any current default.

**Why keep it (rather than remove):**

- It is the **only partially-exercised relocation-remap path** in the tree —
  per-safepoint shadow push/reload codegen, the multi-thread marking scan
  (`roots.rs`/`gc.rs`), the post-move remap, and the unwind reset
  (`helpers.rs::set_jit_thread`/`restore_jit_thread`). Removing it discards real,
  working (on the GC microbenchmarks) infrastructure.
- It is **safely default-off**: when the gate is unset the codegen and GC paths
  are byte-identical to the legacy path, so retaining it costs nothing on the
  validated default (precise maps + non-moving sweep).
- It is the **starting point** if a moving/compacting young gen is ever pursued
  (the only path that has produced a correct *moving* bt18 at adequate heap).

**Experimental status / caveats (unchanged):** opt-in, default-off, **not for
production**; **partial** (the moving Cheney path under-counts bt18 → 67674804;
the OSR sub-gate → 68199090, both short of the golden 68332206); and it **must
not be combined with `CRATONVM_PRECISE_JIT_MAPS`** — the two interfere and
reclaim. Before it could ever be promoted it needs the bt18 under-count closed
and the precise-maps interference resolved — tracked under
`default-moving-young-gen.md`, not here.

## Step 7 — App GC-root gauntlet (landed 2026-06-22): the runnable subset is green

`test-infra/regression-pool/gc-root-apps-lane.sh` is the named-app companion to
the Step-4 repro+bench lane. For each app suite it runs the workload under
CratonVM with **precise maps default-on**, at the default heap **and** under
forced young GC (`CRATONVM_DBG_GC_STRESS`), and classifies by **GC-invariance**
rather than feature parity:

- **PASS** = CratonVM's outcome is identical with vs. without forced GC (same rc,
  same result-summary signature) and shows **no HARD** (crash: SEGV / panic /
  fatal) corruption. A **SOFT** marker — the GC walker catching + re-syncing a
  stale/half-initialised access — is benign *iff* the result stays GC-invariant,
  and is reported as `guarded(soft=N)` (the "masked, not closed" state, same as
  the Step-4 repros). A feature gap vs HotSpot (locale/i18n) is GC-invariant, so
  it is a note (`cv!=hs`), never a GC-root failure.
- This criterion is the point: the register-invisibility family is a *GC-timing-
  dependent* fault, so "the result doesn't change when you force GC, and nothing
  crashes" is exactly the property precise maps must guarantee.

**Baseline (2026-06-22, current dev binary, JDK-25 oracle, GC_STRESS = 4 MB):**

| suite | result | note |
|---|---|---|
| BouncyCastle `asn1.test.RegressionTest` | **PASS** | GC-invariant + corruption-free; differs from HotSpot only by the `X500Name` Turkish dotless-i locale fold (a feature gap, GC-invariant). One earlier run tripped a *recoverable* `inconsistent header (inline-alloc race)` soft marker under 4 MB stress — walker re-synced, result unchanged; rare/flaky, orthogonal to precise *root* maps. |
| BouncyCastle `crypto.prng.test.RegressionTest` | **PASS** | GC-invariant + corruption-free; == HotSpot. |
| BouncyCastle `crypto.test.RegressionTest` | SKIP | runnable but too long for the lane timeout — `BC_CRYPTO=1` + large `TIMEOUT` to include. |
| h2 / wildfly / kafka / elasticsearch / hibernate / commons-math | SKIP | **env-blocked on a dev checkout** — these are gitignored build artifacts (need gradle/maven + network + disk, and kafka/ES need docker). Gated SKIP-with-note; the lane runs them on CI / a provisioned box where they are built. |

**Honest scope.** BouncyCastle (crypto / ASN.1 / PRNG / reflection-heavy, pure
Java) is the strongest register-invisibility vehicle that is actually runnable on
this box, and it is **green** (GC-invariant, no fatal corruption) with precise
maps default-on. The full named container gauntlet
(cassandra/tomcat/wildfly/keycloak/spring-boot/jenkins/felix) remains the
outstanding bar and is **CI-deferred** — the lane is written and gated for it,
but those classpaths are not built here. So the GC-root *family* is not yet
formally "retired" (that also needs A4's cross-thread-STW scan + FJP CAS fix;
A2's fix already landed `6e3ddb05`), but every GC-root vehicle runnable on this
box — repros, benches, and the BC app
suites — is green under the precise-maps default.

## Step 8 — GC-root family retirement status (refreshed 2026-06-29): A1/A2/A3 closed, A4 open, nothing removed

The "validated default" work closes the **A3 register-invisibility class**.
Since this doc was first written, **A2 has also been fixed** (`6e3ddb05`,
2026-06-23) — leaving **A4 as the only open family member**. The *family* is
**not yet formally retired** (A4 + the container-app CI run remain), but three of
four members are now closed. Per the retain directive, **nothing is removed**:
the conservative backstop, the GC guards, every `CRATONVM_DBG_*` diagnostic knob,
the shadow stack (Step 6), and all A1–A4 repros stay in the tree as
experimental / debugging tools.

**Current-dev status refresh (re-verified 2026-06-29, fresh release build off dev
HEAD `9928052c`, JDK-25 oracle):**

| member | status | current-dev evidence |
|---|---|---|
| A1 (reflection mirror-array pin) | ✅ FIXED | `pin_native_root` (long-landed) |
| **A2** (`ReflRepro` heap corruption) | ✅ **FIXED on dev** (`6e3ddb05`, 2026-06-23) | Was **not** a register/native-return missed root (the title's premise was refuted) — it was a GC-side free-list accounting bug: the non-moving young sweep's coalescer merged only *adjacent* free blocks, so *overlapping* free blocks both survived and `Arena::alloc` double-served the region → two live objects at overlapping addresses → linear-walk desync → the long-seen "implausible object size" / decayed `java.lang.Object` / SIGSEGV. Fix coalesces overlapping blocks too (`off <= last_end`). **Re-verified 2026-06-29:** `ReflRepro 8000 @ GC_STRESS=65536` → `ok=8000 bad=0 rc=0`, and `20000 @ GC_STRESS=524288 --Xmx 256m` → `ok=20000 bad=0 rc=0` (was rc=139). bt16/bt18 golden. Precise maps are orthogonal. A benign non-corrupting RE-SYNC residual (`bad=0`) is a perf/cleanliness follow-up. |
| A3 (register-invisibility) | ✅ CLOSED by precise maps | repro+bench lane (Step 4) + OSR (Step 5) + BouncyCastle apps (Step 7) all green; re-verified 2026-06-29 (`VAAload 14 @ GC_STRESS=4096` = `3222190`) |
| **A4** (`Fork6` FJP multi-thread) | 🟡 **OPEN but non-fatal on the repro here** | gated `CRATONVM_REAL_FORKJOINPOOL=1` → **26/26 ALL-OK** (8 sequential + 18 concurrent-stress, re-verified 2026-06-29), 0 corruption (the doc's older ~15% reclamation does **not** reproduce on current dev — plausibly also helped by the A2 free-list fix, which shares the non-moving-sweep path); the documented cross-thread STW JIT-root gap is still *exercised* (`scan_active_jit_frames` WARN, `cross_thread_jit_gap_hits` incrementing) but **non-fatal** (the parked thread's `root_snapshot` covered it). Benign real-FJP `cas_long FAIL` retry noise still fires. The cross-thread STW JIT-root scan remains the tracked follow-up — the genuine residual is a register-only oop at a non-call safepoint (see the A4 known-issue doc). |

**Container-app gauntlet — CI-complete, not removed.** `gc-root-apps-lane.sh`
now enumerates the named apps (tomcat / keycloak / spring-boot / cassandra /
activemq / jenkins / felix) via an env-driven `named_app` hook (set `<APP>_CP` +
`<APP>_MAIN` to run on a box where the app is built; SKIP-with-note otherwise) in
addition to the runnable BouncyCastle suites. So the harness is the complete
named-app gauntlet; the heavy apps are CI-deferred (unbuilt on a dev checkout),
not dropped.

**Formal retirement of the family is now gated on** (separate, dedicated work):
the A4 cross-thread-STW JIT-root scan (+ the real-FJP CAS bug) and a CI run of the
container apps. (A2's fix landed `6e3ddb05`.) Until then this doc claims only what
is proven: the precise-maps default closes A3, A1/A2 are independently fixed, and
the default is green across every GC-root vehicle runnable on this box.

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
  runs the A1–A4 repros (`../internal/fixed-suite-bugs/repros/A2-reflrepro`,
  `…/A4-fork6`, `MinRegexProbe`) + the named register-invisibility apps with
  precise on and diffs against HotSpot baselines. Harness/config only.
