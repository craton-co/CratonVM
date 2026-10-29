# Deoptimization Frame State Representation and Interning

## Status

**Owned representation active in production; immutable/interned representation retired / not planned.**

The production compiler backends emit an owned `FrameState` representation at deoptimization points. An experimental immutable, content-addressed interned representation (`SharedFrameState`, `FrameStateInterner`) was evaluated and retired because compiler producers never generated inlined caller chains, making the metadata structural deduplication dormant in production, while construction-only measurements could not justify maintaining duplicate metadata representations. The explicit `ResumeSemantics` flag designed alongside the interner was successfully retained and graduated to `DeoptimizationPoint` in production.

---

## 1. The Owned Representation and Its Trade-Offs

In the CratonVM JIT compiler, safepoint metadata is recorded at every call, poll, and guard. The owned representation defines `FrameState` as:

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

Every safepoint carries its own copy of the operand stack, locals, and monitors. This design has specific characteristics:

1. **Simplicity and Directness**: Construction is decoupled across compilation passes. Each deoptimization point owns its complete state independently, without needing a shared arena or interner lifetime management that outlives the compiled artifact.
2. **Per-Method Footprint**: Consecutive safepoints differ in only one or two slots (as JVM bytecode pushes or pops operand stack entries), yet each safepoint allocates separate vectors. With `FrameValue` being 56 bytes (to accommodate `VirtualObject`), a 64-local method with 100 deoptimization points can carry ~400 KB of near-duplicate slot metadata.
3. **Inlining Multiplier**: In an inlined call tree, `FrameState::caller` is a `Box<FrameState>`, requiring a deep copy of caller frames at every callee safepoint. If a callee with 40 safepoints is inlined into a caller with 16 locals, those 16 locals are replicated 40 times.

---

## 2. The Interned Representation Architecture

To evaluate structural sharing and deduplication, an interned design was developed with the following core components:

### Structural Sharing with Chunked Spines

```rust
pub struct SharedFrameState {
    pub method_key: MethodKeyId,          // Content-addressed interned string
    pub bci: u32,
    pub locals: ValuesId,                 // Interned value spine handle
    pub stack: ValuesId,
    pub monitors: MonitorsId,             // Interned monitor array handle
    pub caller: Option<FrameStateId>,     // Parent-linked shared handle
    pub semantics: ResumeSemantics,       // Explicit reexecute / rethrow semantics
}
```

Instead of owning vectors, `SharedFrameState` is 32 bytes and `Copy`. All handles index into a central `FrameStateInterner`. Value arrays are broken into fixed chunks (`FRAME_VALUE_CHUNK` = 8 values):

```text
Snapshot k    Spine = [chunk_0, chunk_1, chunk_2, chunk_3]   (32 slots)
Snapshot k+1  Spine = [chunk_0, chunk_1, chunk_2, chunk_4]   (one slot changed)
                                                 ^^^^^^ only new storage
```

When a single slot changes between safepoints, only one chunk (8 values) and a lightweight spine are allocated. Identical states collapse via content hashing, yielding structural equality checks (`==` on handles) and deduplicating identical frames across guards and calls.

### Persistent Derivations

State transitions are modeled through non-mutating updates:

```rust
let next = interner.with_local(prev, index, value);   // O(1 chunk)
let next = interner.with_bci(next, bci);              // O(0 slots)
let next = interner.with_caller(next, Some(caller));  // O(0 slots)
```

If the written value is already present or identical, the existing handle is returned without allocating.

---

## 3. Resume Semantics (Graduated to Production)

A crucial insight from the interning design was replacing implicit prose conventions regarding whether a deoptimization trap re-executes or resumes after an instruction.

`ResumeSemantics` provides two orthogonal boolean flags:

| State | `reexecute` | `rethrow_exception` | Description |
|---|---|---|---|
| `RESUME` | `false` | `false` | Bytecode at `bci` completed; continue execution after it. |
| `REEXECUTE` | `true` | `false` | Guard fired before bytecode took effect; re-execute bytecode from top. |
| `RETHROW` | `false` | `true` | Not a resume point; route pending exception through exception table. |

### Importance for Inlined Call Chains

When deoptimizing inside an inlined method:
- The innermost (trapping) scope re-executes its bytecode (`REEXECUTE`).
- Every caller scope in the chain is parked mid-invoke (`RESUME`); re-executing would erroneously trigger the callee a second time.

### Current Production Status of Semantics

`ResumeSemantics` is fully incorporated into production:
- `DeoptimizationPoint` carries `semantics: ResumeSemantics` in `jit/src/deopt.rs`.
- Both backends (`x64` and `ir_lower`) stamp semantics explicitly via `ResumeSemantics::for_reason(reason)`.
- The runtime resume engine (`vm/src/runtime/interpreter/deopt_resume.rs`) inspects `point.semantics` to route exceptions and calculate resume PCs.
- `ReconstructedFrame` records per-frame resume semantics during stack unwinding.

---

## 4. Current Architecture and Status

The full interner (`FrameStateInterner`, `SharedFrameState`, `InternedDeoptPoint`) is retired and not active in the codebase:

1. **No Live Inlined Caller Chains in Metadata**: Both JIT backends currently lower inlined code using caller invoke semantics rather than generating multi-level `caller` scope chains in published artifacts. Without deep inlining chains in compiled artifacts, the primary scaling bottleneck that interning solves is absent.
2. **Construction vs. Production Sharing**: While unit tests deriving sequential states demonstrated >85% slot sharing by construction, real-world backends independently construct snapshots from register allocation provenance and IR state. Deduplicating ad-hoc reconstructed frames requires an active producer architecture that derives states rather than re-building them.
3. **Sound Single Path**: To maintain verification integrity, `DeoptVerifier` validates owned `FrameState` structures at installation time. Maintaining dual verification lanes (`verify` vs `verify_interned`) risked subtle drift without runtime benefit.
4. **Memory Accounting**: The owned representation cost remains actively monitored via `metrics.rs::frame_state_heap_bytes`. Should future inlining pipelines generate deep caller scope chains, the chunked spine architecture documented here provides the reference design for re-introduction.
