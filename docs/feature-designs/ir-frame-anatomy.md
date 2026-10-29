# Optimizing-Tier Stack Frame Anatomy and Claim Architecture

## Status

**Layout and baseline clearing shipped (default on); fine-grained claim and cover optimizations shipped (opt-in).**

In CratonVM's optimizing Sea-of-Nodes JIT compiler (`ir_lower.rs`), compiled methods establish a unified stack frame layout. Frame layout directly balances execution overhead, safepoint deoptimization metadata, and garbage collection scanning performance.

---

## 1. Stack Frame Layout

From the base pointer (`rbp`) downward toward the stack pointer (`rsp`), an optimizing-tier compiled frame is organized into structured regions:

```text
       High Memory
       +-----------------------------------------------+  <- rbp
       | Return address                                |
       +-----------------------------------------------+
       | Saved rbp                                     |
       +-----------------------------------------------+  <- [rbp - 0x00]
       | Java locals (8 * max_locals)                  |  (Parameters & local slots)
       +-----------------------------------------------+
       | Context word (8 bytes, if body calls runtime) |
       +-----------------------------------------------+
       | Bookkeeping area (40 bytes)                   |  (sp-id, thread, save base/top, phi scratch)
       +-----------------------------------------------+
       | Value colours / Spill slots (8 * num_slots)   |  (Prim, Pinned Prim, Ref, Pinned Ref)
       +-----------------------------------------------+
       | Callee-saved XMM band (32B Win64, 0B SysV)    |
       +-----------------------------------------------+
       | Callee-saved GPR band (40 bytes: rbx, r12-r15)|
       +-----------------------------------------------+
       | Deopt SavedRegisters: XMM half (128 bytes)    |  (Written by deopt stub; skipped in clear)
       +-----------------------------------------------+
       | Deopt SavedRegisters: GPR half (128 bytes)    |  (Spilled registers during deopt)
       +-----------------------------------------------+
       | Outgoing argument staging (8 * widest_call)   |  (Arguments staged for runtime helpers/calls)
       +-----------------------------------------------+
       | Stack-argument reserve + ABI shadow (48 bytes)|  (System V / Win64 calling convention reserve)
       +-----------------------------------------------+  <- rsp
       Low Memory
```

### Detailed Region Descriptions

1. **Java Locals**: Holds method arguments passed via stack/registers and local variable slots. In standard layout, space for all declared locals is reserved.
2. **Context and Bookkeeping**: Stores VM thread references, safepoint activation identifiers, and frame-block management headers.
3. **Value Colouring Band (`SlotPlan`)**: Storage allocated by the slot allocator for SSA values that require stack homes. Divided into:
   - **Primitive Colours (`Prim`)**: Slots holding numeric primitives (`int`, `long`, `float`, `double`).
   - **Pinned Primitive Colours (`Pinned Prim`)**: Primitive values bound across safepoints, phis, or scalar replacements.
   - **Reference Colours (`Ref`)**: Slots holding object references (`ObjectRef`).
   - **Pinned Reference Colours (`Pinned Ref`)**: Object references named in safepoint snapshots.
4. **Callee-Saved Registers**: GPRs (`rbx`, `r12-r15`) and XMM registers preserved across method calls per platform ABI.
5. **Deoptimization Image Area**: Pre-allocated space used by the deoptimization trampoline stub to spill all register state into `SavedRegisters` upon a guard failure.
6. **Argument Staging & ABI Shadow Area**: Fixed scratch area for preparing calls into runtime helpers and callee methods without dynamic stack adjustments.

---

## 2. Fresh-Frame Clearing and Memory Hygiene

When a compiled method is entered, any slot that may be visible to a garbage collection safepoint must contain a valid value (zero or a legitimate object reference). Uninitialized stack words containing residual bit patterns from previous activations could be misidentified as live references or create invalid pointer retention.

The JIT emitter clears necessary frame regions during the method prologue:
- Bookkeeping words (e.g., `sp-id` and frame publication headers) must be zeroed immediately.
- Reference slots (`Ref` and `Pinned Ref`) are published to the shadow stack or GC stack maps and must read zero until initialized.
- Unused local slots and staging words are cleared to prevent conservative root retention.

---

## 3. The Claim Architecture (Reducing Frame Clear Overhead)

Clearing hundreds of bytes on every method entry incurs measurable instruction overhead. The **Claim Architecture** eliminates redundant zeroing by proving at compile time that specific regions will never hold live object references:

### Per-Body Claim Switches

| Switch | Default | Impact on Frame Construction and Clearing |
|---|---|---|
| `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` | **On** | Non-reference primitive colours join the method's claim (`cm.ir_prim_colour_claim`) and are omitted from prologue zeroing. |
| `CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM` | Opt-in | Pinned primitive colours (loop indices, counters, flags) join the claim and are excluded from zeroing. |
| `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM` | Opt-in | Deoptimization GPR save words (128 bytes) join the claim. Because the image is written only during deopt and never read across a GC safepoint, clearing it on entry is unnecessary. |
| `CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY` | Opt-in | Stack frame allocates space only for incoming parameters rather than all `max_locals`, reducing both frame size and clear bounds. |
| `CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS` | Opt-in | Ranged lifetime analysis replaces global pinning for snapshot-named primitives, allowing them to share primitive colour slots. |
| `CRATONVM_JIT_IR_HELPER_AREA_CLAIM` | Opt-in | On System V ABIs, claims the 48-byte ABI reserve and helper argument area, omitting it from the clear. |
| `CRATONVM_JIT_IR_FRESH_CLEAR_COVER` | Opt-in | Replaces individual run clearing loops with an optimal set of SIMD stores (`VMOVDQU` / `VPXOR`) covering required zeroing windows in minimal instructions. |

---

## 4. Garbage Collector Coordination

Omitting stack regions from prologue clearing requires strict cooperation with the garbage collector:

- **Word Ignoring (`ir_body_claim_ignores_word`)**: During stack scanning in `vm/src/jit/conservative_roots.rs`, the collector consults the method's `ir_prim_colour_claim`. Claimed stack offsets are ignored during reference tracing.
- **Preventing Retention (G1 / Generational)**: On collectors with region pinning (like G1), an uninitialized slot holding residual data might mimic a valid pointer into a heap region, causing accidental region pinning. The claim architecture ensures that claimed slots are completely bypassed during root and pin identification, maintaining clean reclaim rates.
- **Minted Handle Protection**: Even within claimed primitive words, specialized minted handles in 64-bit integer words (`smuggled_longs`) are safely verified and preserved where required.
