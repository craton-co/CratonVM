# Deoptimization Frame State Representation and Interning

| | |
|---|---|
| **Status** | Architectural Feature Design |
| **Normative baseline** | [`real-frame-deopt.md`](real-frame-deopt.md), [`deopt-osr.md`](deopt-osr.md) |
| **Related** | [`jit-osr-exit-and-recompile.md`](jit-osr-exit-and-recompile.md), [`jit-osr-entry-metadata.md`](jit-osr-entry-metadata.md) |

This document defines the architectural design for immutable, structurally shared, and interned deoptimization frame states. It provides the scaling foundation for JIT safepoint metadata as methods, inlining depths, and speculative optimizations grow.

---

## 1. Problem & Motivation

CratonVM's baseline deoptimization infrastructure reconstructs active interpreter frames at guard failure points, as specified in [`real-frame-deopt.md`](real-frame-deopt.md) and [`deopt-osr.md`](deopt-osr.md). The runtime data model centers on `FrameState`:

```rust
pub struct FrameState {
    pub method_key: String,
    pub bci: u32,
    pub locals: Vec<FrameValue>,
    pub stack: Vec<FrameValue>,
    pub monitors: Vec<MonitorInfo>,
    pub caller: Option<Box<FrameState>>,
}
```

While functional for basic compilation, an owned, non-interned representation exhibits steep scaling bottlenecks under optimizing compilation:

1. **Safepoint Density and Duplication:**
   Compilers emit safepoints at calls, loop polls, allocation sites, and speculative guards. Between adjacent bytecode instructions, only one or two operand stack slots or local variables typically change. Storing full owned vectors (`Vec<FrameValue>`) at each point forces massive redundant heap allocations.

2. **Per-Method Footprint Overhead:**
   Each `FrameValue` is a 56-byte tagged union (accounting for constants, registers, stack spill offsets, and scalar-replaced `VirtualObjectState`). For a method with 64 locals and 100 safepoint sites, storing uncompressed owned states consumes over 400 KB of duplicate metadata for a single compiled artifact.

3. **Multiplicative Inlining Cost:**
   Inlining nested callees compounds metadata size geometrically. An inlined callee with 40 safepoints within a 16-local caller re-copies the caller's frame 40 distinct times via `caller: Option<Box<FrameState>>`. Across multiple inlined call tiers, metadata growth quickly overtakes compiled machine code size.

---

## 2. Architectural Design

The interning architecture replaces owned vectors with compact, content-addressed handles referencing immutable shared data structures managed by a `FrameStateInterner`.

### 2.1 The Shared Frame State Descriptor

```rust
pub struct SharedFrameState {
    pub method_key: MethodKeyId,          // Interned method identifier
    pub bci: u32,                         // Bytecode index
    pub locals: ValuesId,                 // Interned local slot spine
    pub stack: ValuesId,                  // Interned operand stack spine
    pub monitors: MonitorsId,             // Interned monitor vector
    pub caller: Option<FrameStateId>,     // Shared parent scope link
    pub semantics: ResumeSemantics,       // Resume vs re-execute semantics
}
```

`SharedFrameState` is 32 bytes, implements `Copy`, and contains no owned heap allocations. All fields are shallow handles into content-addressed tables:
- Handles provide structural equality: `state_a == state_b` is an O(1) integer comparison.
- Content-addressed indexing guarantees that identical frame states map to the exact same `FrameStateId`.

### 2.2 Structural Sharing: Chunked Value Arrays

To avoid allocating new arrays when consecutive safepoints differ by only a few values, slot sequences are partitioned into fixed-size **chunks** (e.g. `FRAME_VALUE_CHUNK = 8`).

A `ValuesId` points to an interned **spine** of chunk identifiers:

```text
Safepoint k   : spine = [chunk_0, chunk_1, chunk_2, chunk_3]  (32 slots)
Safepoint k+1 : spine = [chunk_0, chunk_1, chunk_2, chunk_4]  (slot 26 changed)
                                                    ^^^^^^^
                                            only new chunk allocated
```

- If slot 26 changes, chunks 0, 1, and 2 are shared verbatim.
- The storage cost for deriving a new state is one 8-slot chunk plus an interned slice of chunk identifiers (`ValuesId`), reducing incremental slot overhead by ~85–88%.

### 2.3 Persistent Derivation API

Rather than constructing states from scratch, compiler lowering derives successive states functionally:

```rust
// Persistent transformations return new handles without mutating predecessors
let state_1 = interner.with_local(state_0, slot_idx, new_value); // Shares unmodified chunks
let state_2 = interner.with_bci(state_1, next_bci);               // Shares all slot chunks
let state_3 = interner.with_caller(state_2, Some(caller_id));    // Links shared parent scope
```

Derivations that produce an identical state configuration automatically resolve to the existing `FrameStateId`.

---

## 3. Resume Semantics Representation

Inlined deoptimization requires precise semantics distinguishing how each frame in a scope chain resumes. This is captured by `ResumeSemantics`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResumeSemantics {
    pub reexecute: bool,
    pub rethrow_exception: bool,
}
```

The runtime interprets three operational states:

| Semantic State | `reexecute` | `rethrow_exception` | Description |
|---|---|---|---|
| `RESUME` | `false` | `false` | The instruction at `bci` completed; resume execution at the subsequent bytecode instruction. |
| `REEXECUTE` | `true` | `false` | The guard failed before the instruction took effect; re-execute bytecode at `bci` from the top. |
| `RETHROW` | `false` | `true` | Exceptional trap point; pass the active exception to the method's exception table. |

### Importance for Inlining

When deoptimizing an inlined callee:
- The **innermost (trapping) scope** typically uses `REEXECUTE` (re-running the bytecode whose speculative guard failed).
- Every **caller scope** in the chain is suspended mid-invoke. It must use `RESUME` (`ResumeSemantics::for_caller_scope()`), parking right after the `invoke` bytecode. Re-executing a caller scope would erroneously trigger duplicate method invocations.

In the interned representation, `ResumeSemantics` is embedded directly into `SharedFrameState`, ensuring that scopes with differing execution requirements remain structurally distinct.

---

## 4. Integration and Materialization Layer

To integrate with runtime execution paths described in [`real-frame-deopt.md`](real-frame-deopt.md), the system provides bidirectional bridging:

```text
[ Compiler Frontends (IR / Codegen) ]
                 │
                 ▼  (intern_scope / persistent derivation)
      [ FrameStateInterner ]  ◄── Compact storage in CompiledMethod
                 │
                 ▼  (materialize / on-demand expansion)
[ Deoptimization Resume & Verification Sinks ]
```

1. **Compilation Phase:**
   The compiler records `InternedDeoptPoint { native_offset, bci, reason, state: FrameStateId }` into the `CompiledMethod`. Redundant states across guards and call sites collapse into shared handles.

2. **Deopt Trapping & Resume Phase:**
   When a machine guard trips:
   - The VM deopt trampoline locates the `InternedDeoptPoint` by faulting native offset.
   - For fast path inspection, accessors read slot values directly from chunks via `interner.get_local(state_id, idx)` without full materialization.
   - When transferring into the interpreter (`deopt_resume.rs`), the target frame is materialized into an execution frame on the thread stack.

3. **Depth Bounds and Verification:**
   Scope chain depth is strictly bounded by `MAX_SCOPE_CHAIN = 256`. Chains exceeding this limit fail closed during installation verification, preventing stack overflow during recursive unwinding.

---

## 5. Architectural Alignment

- **Baseline Contract:** The actual execution, machine register extraction, and OSR transitions adhere to [`real-frame-deopt.md`](real-frame-deopt.md) and [`deopt-osr.md`](deopt-osr.md).
- **Metadata Deduplication:** Interning acts as the compression and deduplication layer behind `DeoptimizationPoint`, preventing metadata explosion while preserving exact semantic equivalence with HotSpot frame materialization.
