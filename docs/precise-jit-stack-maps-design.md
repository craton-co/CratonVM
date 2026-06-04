# Precise JIT stack maps — design & staged implementation plan

## ⚠ CORRECTED FINDING (supersedes the "mechanism proven" claim below)

Instrumentation (`CRATONVM_DBG_PRECISE`, `CRATONVM_SP_VERIFY`) showed the
precise RELOCATION path is **never exercised** under the gate:
- `remap_active_jit_frames` is **never called with a non-empty pointer_map**
  (0 `[PRECISE]` lines) on bt16 *or* bt18.
- selective promotion's evac telemetry prints **0 `[sp-verify]` lines** under
  the precise gate → `evac_map` is **empty** → it evacuates **nothing** when
  `CRATONVM_PRECISE_JIT_MAPS` is on.

So the `68332206 → 68199090` change is **NOT** from precise relocation. Since
"no evacuation" ≈ pure non-moving sweep (which is golden 67674804), bt18
precise+selective = 68199090 (wrong) is an **unresolved interaction** of the
precise-gate codegen / pin behavior with the selective-promote path — **not a
demonstrated fix**. bt16 is golden in every gate config (codegen sound there).

Open questions (need a quiet machine — bt18 currently OOMs under a parallel
session): (1) is `evac_map` truly empty, or non-empty-but-not-routed to
`remap`? — print `pointer_map.len()` at the top of `update_all_roots`. (2) Why
does the precise gate suppress evacuation? Leading theory: Stage-2 precise
local-slot map entries are read by `scan_one_frame_precise` via the **still-
imprecise `frame_base`** (the conservative backstop uses the guard SP, not the
exact RBP that only `remap` uses) and over-pin everything. (3) Does precise-ONLY
bt18 (no selective) stay golden? The relocation machinery (Stages 3–5) is
implemented and bt16-sound but **unproven on bt18 because it isn't engaging**.

---

## (earlier) RESULT (Stages 1–4 landed): mechanism *appeared* proven

bt18 A/B (8g, deterministic, reproduced):
- golden = **67674804**
- selective-promote ALONE (the bug) = **68332206** (error 657402)
- selective-promote + `CRATONVM_PRECISE_JIT_MAPS=1` (Stages 1–4) = **68199090**
  (error 524286)

The precise maps **deterministically reduce the register-invisibility error
~20%** → the mechanism (Stages 1–4: oop maps, exact RBP, sp-id, slot-rewrite,
register reload) demonstrably engages and relocates real oops. bt16 stays
14985902 in every config (gate-off, gate-on, gate-on+selective) → no
regression / no corruption.

**Residual root cause — innermost-frame-only remap.** Recursive `make()→make()`
is a JIT→JIT (direct machine-code) call, so it does NOT go through the
interpreter's `enter_with_compiled` → **no new `JIT_ENTRY_CHAIN` entry per
recursion**. There is ONE chain entry for the whole JIT call tree. Each nested
prologue's `set_top_frame_base` overwrites `frame_base` to the *deepest* RBP
(and leaves `info.cm` as the *outermost* method — a cm/RBP mismatch). So
`remap_active_jit_frames` rewrites only one frame; every ancestor `make()`
frame's `n` stays stale → partial fix.

**Stage 5 core fix — walk the RBP chain.** JIT frames use `push rbp; mov
rbp,rsp`, so within the contiguous JIT region `[frame_base, entry_sp)` the
saved-RBP chain (`[rbp]` = caller rbp, `[rbp+8]` = return addr) is walkable.
`remap_active_jit_frames` (and the reload/coverage logic) must walk it and
remap EVERY frame, resolving each frame's `CompiledMethod` from its code
address (a code-addr→cm interval lookup; or store the cm/sp-id per frame). Also
covers the entry-method frame (e.g. `binaryTrees`'s `longLived` local).

---

Status: **Stages 1–3 landed (gated-OFF), Stages 1–2 validated** on branch
`feat/precise-jit-stack-maps`. Work continues in an isolated git worktree
(`C:\craton\CratonVM-pjsm`) after a shared-checkout collision (a parallel
session `git checkout dev`-ed the shared tree mid-edit; commits were safe).

Stage 3 (`d6bf810`) — exact-RBP frame registration + safepoint-id + precise
JIT-frame relocation, gated `CRATONVM_PRECISE_JIT_MAPS` (default-OFF →
byte-identical default path). Compiles clean. Gate-ON moving validation is
Stage 5 (needs a quiet machine for bt18). It resolves the two correctness
items below: `frame_record` records the EXACT RBP (item #2), and
`remap_active_jit_frames` rewrites slots at `[rbp - off]` (correct sign, item
#1). Inert by default (`sp_id_slot_off == 0` for every method).

Progress log:
- Stage 1 (`427b474`): operand-stack `stack_oop_marks` desync eliminated +
  lockstep debug assertion. Validated: bt16=14985902, sieve250k=22044 (rc=0),
  behaviour-neutral.
- Stage 2 (implemented): forward "must be oop" local-variable dataflow
  (`compute_local_oop_masks` / `oop_dataflow_{successors,transfer}`) feeds
  `emit_oop_map_for_safepoint`, which now records the canonical frame slots of
  oop register/memory locals (e.g. bt18's `n` in local 1) in addition to
  operand-stack temporaries. Behaviour-neutral on the default path
  (`scan_one_frame_precise` already sweeps the whole frame and re-validates via
  `is_object_address`, so the extra precise entries are redundant there).
  Params seeded conservatively non-oop for now (TODO before Stage 5).

### Correctness items discovered, deferred to Stage 3 (exact relocation)

The current precise path is *non-load-bearing* (a pure additive optimisation on
top of the conservative sweep), which masks two issues that MUST be fixed before
the map can drive relocation:

1. **Offset sign convention.** `OopMapEntry::frame_slot_offsets` are documented
   as RBP-relative with *negative* = below RBP, but `emit_oop_map_for_safepoint`
   stores `StackSlot::Frame(off)`/`local_offset(k)` as *positive* values (the
   slot is physically at `[rbp - off]`). `scan_oop_slots` does `frame_base +
   offset`. Today this resolves to wrong addresses that `is_object_address`
   filters out — harmless only because the conservative sweep finds the real
   oops. Stage 3 must make the stored offset + walker arithmetic agree on
   `[rbp - off]`.
2. **Imprecise `frame_base`.** `PreciseFrameInfo::frame_base` is the Rust-side
   guard SP, not the JIT frame's real RBP (release builds omit frame pointers,
   so it can't be recovered by walking). Stage 3 must register the real RBP via
   the prologue/epilogue (explicit frame registration) so slot addresses are
   exact.

## Why

`docs/bintrees18-selective-promotion-investigation.md` proved that
`bintrees18`'s remaining wrong checksum (selective promotion: `68332206` vs
golden `67674804`) is **register-invisibility**: under CratonVM's *conservative*
JIT root scan, an object whose only live reference lives in a JIT register that
isn't reloaded after a move goes stale when a moving collector (selective
promotion) relocates it. The verdict was "unfixable without precise JIT stack
maps." This document is the plan to build exactly that.

The user explicitly chose this path (the hard root-cause fix) over the
alternatives (Cheney-style non-reuse; re-challenging the verdict; quarantining
the dead path).

## The current architecture (as-is)

Three mechanisms exist; the moving path is missing one whole half.

1. **Marking (liveness).** `vm/src/memory/roots.rs::collect_roots` →
   `jit::conservative_roots::scan_active_jit_frames` walks each active JIT
   frame's native-stack spill region `[scanner_sp, entry_sp)` in 8-byte strides
   and reports every qword that `heap.is_object_address` validates as a root.
   It **copies values** into a `Vec<ObjectRef>`. Good enough for liveness; a
   value copy can never be used to *rewrite* the slot.

2. **Relocation update (remap).** `vm/src/memory/gc.rs::update_all_roots`
   rewrites roots **in place** via the `pointer_map: {old_addr → new_addr}`
   produced by the moving collector. It covers interpreter frames, statics,
   mirrors, caches, overlays, etc. — but has **no JIT-frame branch**. JIT frame
   slots are never remapped.

3. **Defer.** Because conservative roots can't be safely rewritten (a non-oop
   qword that coincidentally equals an object address must not be clobbered),
   the moving collector **defers compaction** whenever any thread is in JIT
   (`gc_quiescence` / `any_thread_in_jit`). This is why the *non-moving* young
   sweep is the only collector that runs while JIT frames are live — and why
   `bintrees18` can't drain young.

Partial precise infra already present (NEW-12, T1.1.a):
- `cratonvm_jit::OopMapEntry { native_pc_offset, frame_slot_offsets: Vec<i16> }`
  and `CompiledMethod::{oop_maps, push_oop_map, find_oop_map_for_pc,
  has_precise_oop_maps}`.
- `JitEntryGuard::enter_with_compiled` + `PreciseFrameInfo` +
  `scan_one_frame_precise` (currently **additive/union** with a conservative
  backstop — produces a *superset*, never used for rewriting).
- In `jit/src/x64.rs`: `stack_oop_marks` (parallel oop-typing of the operand
  stack), `emit_oop_map_for_safepoint` (records **operand-stack** oop Frame
  slots only), `emit_pre_safepoint_spill` (spills **all** register-locals to
  canonical frame slots before every GC-capable call — conservative, not typed).

## The gap (what a *moving*-safe precise GC additionally needs)

At every GC-capable safepoint, for the moving path to be correct:

- **G1. Exact per-slot oop typing, incl. locals.** The map must list *exactly*
  the oop slots (no false positives → we'd rewrite an int; no false negatives →
  we'd miss an oop → stale). Today only operand-stack slots are mapped, and the
  `stack_oop_marks` vec **desyncs** from `self.stack` (the documented
  load-bearing bug, `docs/jit-safepoint-revert.md`).
- **G2. Exact-PC map recovery.** At GC time we must find the *exact* map for
  each frame's current PC. The union-of-all-maps over-approximation is unsafe
  for *rewriting* (a slot that's an oop at PC-A but a live int at PC-B).
- **G3. Exact frame base (RBP).** Slot offsets are RBP-relative; the GC must
  know each active JIT frame's RBP to address its slots. Rust release builds
  omit frame pointers, so the RBP chain can't be walked through helper frames.
  → **explicit frame registration** in the prologue/epilogue.
- **G4. JIT-frame slot rewrite.** `update_all_roots` must rewrite each mapped
  oop slot via `pointer_map`.
- **G5. Register reload after safepoint.** A callee-saved register holding an
  oop **local** survives the call in the register; GC updates the *frame slot*
  but not the register. The JIT must **reload** oop register-locals from their
  (GC-updated) canonical slots after the call. This is the missing half of
  `emit_pre_safepoint_spill` and the direct closer of register-invisibility.
- **G6. Coverage-gated defer-lift.** Only lift the compaction defer for a frame
  once it is *totally* precisely covered. A single un-mapped safepoint, inlined
  callee, or OSR frame breaks relocation → pin its referents conservatively
  instead of moving them.

## bt18 grounding

```java
static Node make(int depth) {
    Node n = new Node();                                   // n → local 1 (callee-saved reg)
    if (depth > 0) { n.l = make(depth-1); n.r = make(depth-1); }   // recursive call = safepoint
    return n;
}
```
The live oop across the recursive `make()` safepoint is **`n` in local 1** (a
callee-saved register). `emit_pre_safepoint_spill` writes it to its frame slot
(so the conservative scan pins it under the non-moving path), but there is **no
reload-after** and the **map omits the local slot**, so under selective
promotion the moving path has no precise, updatable record of it. G5 is the
crux for bt18.

## Staged plan (each stage builds + runs; default behavior byte-identical until Stage 5)

- **Stage 1 — eliminate the `stack_oop_marks`/`stack` desync (G1, operand
  stack).** Route every `self.stack.push` through a `stack_push(slot, is_oop)`
  helper that keeps both vecs in lockstep; oop-ness is opcode-driven
  (`aload`/`aaload`/`aconst_null`/`new`/`anewarray`/`newarray`/`multianewarray`/
  ldc-String/ldc-Class/`checkcast`/`getfield`-L/`getstatic`-L/invoke-returning-L
  → oop; all else → non-oop; `dup*` propagate). `debug_assert_eq!(stack.len(),
  marks.len())` at every safepoint. Pure metadata; no codegen change.
- **Stage 2 — per-local oop typing + locals in the map (G1, locals).** JIT-
  internal forward dataflow over store opcodes (`astore`→oop,
  `istore`/`lstore`/`fstore`/`dstore`/`iinc`→non-oop; merge conservatively, the
  verifier guarantees per-PC consistency). `emit_oop_map_for_safepoint` also
  emits the canonical slots of oop register-locals spilled by
  `emit_pre_safepoint_spill`. Verifier gate `CRATONVM_DBG_VERIFY_OOP_MAPS`: at
  each GC, assert the precise map covers every conservatively-found oop in the
  frame (proves completeness with moving still deferred — zero behavior change).
- **Stage 3 — exact frame base + exact PC + slot rewrite (G2, G3, G4).**
  Prologue stores `(rbp, compiled_method_id)` into a thread-local JIT frame
  shadow-stack; epilogue pops (gated emission). Before each safepoint call,
  store a `safepoint_id` (map index) into a fixed frame slot so the GC recovers
  the exact map. Add `conservative_roots::remap_active_jit_frames(pointer_map)`
  and call it from `update_all_roots`.
- **Stage 4 — reload register-locals after safepoint (G5).** After each GC-
  capable call, reload oop register-locals from their canonical slots. Pairs
  with `emit_pre_safepoint_spill`; only oop-typed locals (Stage 2) reload.
- **Stage 5 — gate + coverage-gated defer-lift + validate (G6).** Gate
  `CRATONVM_PRECISE_JIT_MAPS` (default-OFF). When on + selective promotion,
  evacuate objects rooted only in precisely-covered JIT frames; pin
  conservatively-covered referents. Validate bt18 golden `67674804`, then
  bt10/14/16, regression pool, perf. Default path stays byte-identical.

## Key risks (from prior reverts — `docs/jit-safepoint-revert.md`)

- **Perf.** The reverted putfield spill cost +40% on JVM-boot probes. Mitigation:
  spill/reload only *oop* register-locals (Stage 2 typing), and only at real
  GC-capable safepoints; keep maps off the default path.
- **Desync false positives.** The prior "free" metadata fixes SEGV'd because
  the marks vec was shorter than the stack. Stage 1's lockstep invariant +
  debug assertion is the explicit precondition for everything after.
- **Shared-checkout / `.exe`-lock build traps.** Verify binary mtime + a unique
  string literal after every `Finished`; kill stray `cratonvm.exe` first.

## Reproducer / measurement

```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 --Xmx 8g -cp bench BenchSuite bintrees18
```
Default → `67674804` (~33s). `CRATONVM_SELECTIVE_PROMOTE=1` → `68332206` (wrong).
Golden checksums: bt10=135854, bt14=3222190, bt16=14985902, bt18=67674804.
