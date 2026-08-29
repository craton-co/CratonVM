# IR-tier inlining

`CRATONVM_JIT_IR_INLINE=1`. Default OFF.

## What it is for

`IrBuilder::build` walks one method's bytecode. So an accessor that returns a
freshly-allocated wrapper always shows escape analysis an object leaving through
`areturn`, and `scalar-replaced 0/N` is the only answer per-method EA can give —
at any tier, however good it is. That is the whole of
`docs/known-issues/jit/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-20260827.md`:
kfusion's per-voxel `Short2` was 94% of the cost of a voxel read, and no gate fix
moved it, because the blocker was not a gate.

HotSpot deletes the same object by inlining the accessor chain into its consuming
loop first and running EA on the merged graph. This is that missing half. Escape
analysis is the reason it exists; it is not yet the reason it pays (see
[Measured](#measured)).

## How the splice works

Bytecode-domain, not a second walker.

`lib.rs` hands `build` a **combined buffer**: the compiling method's code, then
one copy of each admitted callee body appended after it. Every pc-keyed side
table the builder consumes (`field_info`, `invoke_info`, `new_info`,
`object_init_pcs`) gets the callee's rows rebased into that buffer's
coordinates. `build` walks the caller exactly as before and, at an admitted
`invoke`, installs the callee's locals and jumps `pc` into the body; the
callee's return restores the caller's frame, pushes the result and resumes at
`pc + instr_len`.

The ~2000-line opcode `match` is reused verbatim. That is the point: a second
walker would be a second model of every opcode, and the two would drift.

Relocation is free because JVM branch offsets are relative to the branching
instruction — a whole body moves without rewriting. `tableswitch` /
`lookupswitch` are the exception (their padding is measured from method start)
and are refused, along with everything else in [Admission](#admission).

## Two bcis

A node's `bytecode_pc` is read for two unrelated purposes, and a spliced node
needs a different answer for each. Getting this wrong is not a missed
optimisation; it was measured as both a wrong-code hazard and a ~230x
slowdown, so it is worth stating plainly.

**As a site key.** `ir_lower` looks up the compact field offset, the direct-call
entry and the MIC/PIC pair by `bytecode_pc`. The answer has to be unique per
site, so a spliced node keeps its **combined-buffer pc** and `lib.rs` rebases
the callee's rows to match — including a fresh MIC/PIC for every virtual call a
spliced body leaves behind.

An earlier draft gave every node in a region the caller's `invoke` pc instead.
Consequences, both real:

* the region shares the metadata of the call the splice **replaced**, so an
  inline-cache hit on a matching receiver class jumps to the wrong method;
* with no compact-offset row every spliced field read falls back to the checked
  `jit_getfield` helper, and with no cache every surviving call falls back to a
  blind name resolution. On the `VoxelAlloc2` probe that took the control arm
  from 47 ns to 11 000 ns per element.

**As an interpreter program point.** The throw-site bci the exception table's
`[start_pc, end_pc)` check uses (`jit_set_throw_bci` →
`route_jit_exception_through_method`), and the bci a null/bounds guard's deopt
point resumes at. A combined-buffer pc names no instruction in this method, so
`Lowerer::resume_bci` maps it back to the enclosing `invoke` through
`spliced_ranges`. "The `invoke` at this bci has not taken effect; re-execute it"
is true of the whole region and needs no frame identity.

That last clause is why there is **no `InlineScopeTable` entry**. A real
inlined-scope chain needs the innermost frame's own method key, and
`ir_lower::resolve_frame_state` deliberately leaves that for the VM to fill from
the running `CompiledMethod` — i.e. it would name the caller. Re-execution needs
no identity at all. No safepoint snapshot is pushed inside a splice, so a
combined-buffer pc never reaches `build_deopt_points`.

## What re-execution requires, and how it is proved

Re-executing the `invoke` re-runs everything the spliced prefix did. That is
harmless for a write whose target is an object the splice itself allocated: the
re-run allocates a fresh one and writes that instead, and the first is
unreachable garbage.

`IrBuilder` proves exactly that, with a reachability set rather than an alias
analysis (`splice_store_is_local`):

* an `Op::New` / `Op::NewArray` built inside the splice is local;
* a `Load` whose base is local is local — a field of an object this splice
  allocated holds either `null` or something this splice stored there — **unless**
  what the splice stored there was itself non-local, in which case the base is
  tainted and locality stops. Without that second half the rule is wrong in an
  obvious way: `new W(); w.f = callerObject; w.f.x = 1` would read `w.f` back as
  local and admit a write straight into the caller's object.

A store that fails the test bails the compile. The rule admits `Short2.setX` →
`Short2.set` → `storage[0] = v` (`storage` is read from a `Short2` the splice
allocated three levels up) and correctly refuses `VolumeShort2.set`, whose
`storage` is read from the receiver the caller passed in.

The one thing it does not cover is a CALL the spliced body makes: the builder
cannot see whether it writes. See [Open](#open).

## Admission

`resolve_ir_inline_site` in `vm/src/runtime/interpreter/jit_bridge.rs`. Wider
than the single-pass resolver in what it admits and narrower in what it demands.

**Wider.** `new`, array load/store and `arraylength` are refused for the
single-pass emitter because `try_emit_inline_body` has no arm for them — an
emitter limit, not a resolution one. `IrBuilder` lowers all three, and `new` is
the entire point.

**Narrower.**

| refused | why |
|---|---|
| any branch | a relocated body's merges and loop headers would have to be computed over the caller's code; there is no second walker |
| `idiv`/`irem`/`ldiv`/`lrem` | the div-zero guard is the only guard the builder emits, and a spliced region should carry none of its own |
| `ldc` / `ldc2_w` | `InlineSite` records a raw `i64` where the builder needs the value **and** its float/double discriminator; inventing that bit is how a `long` constant becomes a `double` |
| `getstatic` / `putstatic` | no static-field rows are rebased, and `putstatic` is a side effect re-execution cannot undo |
| more than one return, or a return that is not last | the walk goes straight through from the body's first byte to its return |
| a target that is not provably monomorphic | the IR tier has no class-id guard node, so a speculative splice has nothing to fall back to |

"Provably monomorphic" is `static`, `private`, `final`, `<init>`, or a `final`
declaring class. Deliberately **not** a CHA-style "no loaded subclass overrides
it": that is true until the next class loads, so it needs an invalidation
dependency this path does not record. `final` is monotone.

Nesting is bounded by `MAX_IR_INLINE_NEST_DEPTH` (6), deeper than the
single-pass `MAX_INLINE_NEST_DEPTH` (3). A partly-inlined chain is worth
nothing — the moment the object reaches a call the graph cannot see through it
escapes, and EA reports `0/N` exactly as before. kfusion's chain is four levels
(`VolumeShort2.get` → `loadFromArray` → `Short2.<init>()V` →
`Short2.<init>([S)V`), so a budget of three would have measured as no change.

Bounded by `IR_INLINE_MAX_TOTAL_BYTES` (4 × `MAX_INLINE_BYTECODE_SIZE`) and
`IR_INLINE_MAX_SITES` (24). The byte budget is not a code-size budget: every
appended byte is bytecode the builder turns into nodes, and the node count is
what `ir_optimize`'s GVN and the escape-analysis connection graph scale in.

## Measured

`probes`-shaped scratch `VoxelAlloc2.java`: 64³ voxels, each arm's loop in its
own method (a loop in `main` is OSR-only — see [Open](#open)), warmed on a small
volume so compilation is not inside the timed window. Three arms with the same
checksum by construction, so a transform that breaks the read shows up as a
wrong number rather than as a fast one. Steady state (reps 2-5), one binary,
flag A/B:

| arm | inlining OFF | inlining ON | HotSpot |
|---|---|---|---|
| `volume` — `VolumeShort2.get`, i.e. two segment reads plus the `Short2` pair | 2.1-2.7 µs | **171-313 ns** | 1.3-1.4 ns |
| `rawseg` — the same two reads, no wrapper (the CONTROL) | 39-47 ns | **24-38 ns** | 1.1 ns |
| `array` — a plain `short[]` (the floor) | 2.2-3.7 ns | 2.1-2.9 ns | 0.4 ns |

Checksum `8589672448` in every row of every arm.

**~10x on the target arm, and the allocation is still there**: the same run
reports `scalar-replaced 0/2 alloc(s)` for the spliced method. The win is
removing five dispatch frames per voxel and giving the spliced bodies compact
field offsets and inline caches — not deleting the object. See [Open](#open) for
what deleting it still needs.

Ranges, not points, because the `volume` arm is allocation-dominated and
therefore GC-noisy: across runs the OFF arm sat between 1.9 and 2.7 µs and the
ON arm between 171 and 313 ns. The OFF arm also DEGRADES rep over rep (past 3 µs
by rep 5, and the allocation-free `array` arm degrades with it, which is what
says the cause is heap pressure and not the arm). The ON arm does not degrade —
itself a signal, and the reason the honest comparison is steady-state and not
first-rep.

## Open

1. ~~**Escape analysis still reports `0/2`.**~~ **CLOSED.** Both
   allocations are deleted and the `volume` arm has converged onto its own
   control (24.9 ns/voxel against `rawseg`'s 23.8). It took four things, none of
   which was the one this note guessed at — a per-allocation dominance proof
   from the basic block the IR already knew, array scalar replacement, an
   escape-analysis fixed point (replacing the wrapper is what frees its storage
   array), and a deopt descriptor that can spell an array and a nested object.
   Requires `CRATONVM_SCALAR_DEOPT=1` alongside this flag; see
   `fixed-bugs/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-FIXED-20260827.md`,
   which is also where the "why is it still opt-in" argument lives.

2. **A call left inside a spliced body is re-executed on a deopt.** The store
   rule proves nothing about it. The bodies this admits are accessors, but that
   is an observation about today's callers, not a proof.

3. **An OSR-only hot loop gets nothing.** `VoxelAlloc`'s loops live in `main`,
   which never reaches the IR admission gate at all, and that probe measures
   787-914 ns with the flag in either position. The win requires the hot loop to
   be in a method the optimizing tier compiles.

4. **Priced on one probe.** The flag is opt-in until the gauntlet has an
   opinion. The trades it makes — more nodes per compile, a spliced body's
   surviving calls losing their profile seed, compile time — are all real and
   none of them are measured beyond this one workload.

## Instruments

```bash
CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_IR_COMPILES=1 ...
```

* `[ir] inline-plan <method>: N site(s), M spliced bodies, B bytes appended` —
  what the planner admitted.
* `[ir] spliced M callee bodies into <method>` — what the builder actually
  walked. A plan with no matching `spliced` line means the build bailed.
* `[cratonvm-jitc] inline-resolve REFUSED <callee> depth=N: <reason>` (needs
  `CRATONVM_DBG_JITC=1`) — every refusal is named.
* `CRATONVM_DBG_SCALAR_NEW=1` → `scalar-replaced N/M alloc(s)`, which is the
  only number that says whether the point of the exercise was reached.
