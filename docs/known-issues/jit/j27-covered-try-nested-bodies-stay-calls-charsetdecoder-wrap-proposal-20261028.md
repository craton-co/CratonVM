# A spliced callee's covered `try` keeps every nested body as a call: `CharsetDecoder.decode` keeps `decodeLoop`, `ByteBuffer.wrap` cannot splice at all (proposal)

> **j27 wave 8 (lane W8SPL, 2026-10-28): REVIEWED, still PARTLY, PENDING HOST VERIFICATION (no code change).** Read `jit/src/ir/splice_covered_nested.rs` and its builder / lowering hooks. Sound by reading: a covered nested body is admitted only with no handler rows, no indy rows, unguarded, forward branches only, invokes only into rows admitted the same way (depth cap 8) or elided `Object.<init>`; the builder marks every instruction of such a body (`enter_covered_nested_instruction`, any depth under a covered `invoke`) and adds its span to `Graph::splice_handler_covered`, so an unarmed exit still refuses the compile. Refinement 1 collects catch types from the innermost body at the instruction and from each enclosing body at the `invoke` that opened the next, and refuses with no covering row at all (fail closed); the `java.*` whitelist is safe (boot-loader-only names, none an `Error`). Not built, unchanged: refinement 2 (a nested CALL inheriting the enclosing rows as its call-site edges) and guarded / virtual nested rows -- so `decodeLoop` stays a call; both need a builder change to `plant_call_throw_edges` for a depth-2 call and a host run to judge, out of this wave's no-build scope. Arms / flip rule unchanged (wave-7 block of `docs/internal/proposals/jit/tier1/jit-r23-w1-L1-proposals.md`).

> **j27 wave 7 (lane T1L1, 2026-10-28): PARTLY BUILT IN CODE (opt-in), PENDING HOST VERIFICATION.** The main change
> and refinement 1 are built behind three opt-in switches, in `jit/src/ir/splice_covered_nested.rs`:
> `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED` (a covered nested row stays spliced; every instruction of it is
> checked like a covered one and its span joins `Graph::splice_handler_covered` for the lowering),
> `CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN` (the lowering admits the field accesses / allocations the builder
> proved; before it, the lowering refused every `Load`/`Store` the builder's default-on non-null rule admitted), and
> `CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH` (refinement 1: `Op::New` under rows whose catch types are
> all whitelisted JDK `Exception`s, checked along the whole splice chain). Not built: refinement 2 and guarded
> (virtual) nested rows, so `decodeLoop` stays a call; the `wrap` shape needs all three plus
> `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER=1`. Probe `tools/probes/jit/j27/J27T1L1CoveredNestedRows.java`; arms, tests
> and the flip rule are in the j27 wave 7 block of `docs/internal/proposals/jit/tier1/jit-r23-w1-L1-proposals.md`.
> Verify: arms A-D of that block on `J27DecRows` (`wrap`, `dec` rows) and the probe, facts equal to HotSpot in every
> arm, then the `wrap` ns/op.

| | |
|---|---|
| **Status** | OPEN, proposal (perf), filed 2026-10-28 by j27 wave 5 lane DECLOOP |
| **Area** | JIT IR splice planner (`jit/src/lib.rs` `ir_splice_strip_handler_covered_nested`), lowering (`ir_lower.rs` `refuse_unarmed_splice_handler_exits`) |
| **Parent** | [small-jdk-method-call-overhead-charsetdecoder-decode-...](small-jdk-method-call-overhead-charsetdecoder-decode-25-80x-tomcat-methodperformance-and-http2-8-2-wall-20261025.md) |

## Evidence

The `dec` row (`tools/probes/DecLoop.java`, `tools/probes/jit/j27/J27DecRows.java`) runs 160-230 ns
against HotSpot's 10-18 ns. With `CRATONVM_DBG_JITC=1` the hot decode chain is spliced (44 bodies into
`CharsetDecoder.decode(ByteBuffer)`, 27 into the `dec3` row), and the RIP sampler (Windows, `dec3` row,
176 busy samples) puts the time in what is NOT spliced:

| Body | share | why it stays a call |
|---|---|---|
| `ISO_8859_1$Decoder.decodeLoop` (+ spliced `decodeArrayLoop`) | 24 % | inside `CharsetDecoder.decode(BB,CB,Z)`'s `try { cr = decodeLoop(in, out); } catch (RuntimeException \| Error x)`; `ir-splice-handler-covered-nested 1 row(s) ... kept as calls` |
| `ByteBuffer.wrap([BII)` + its `Buffer.limit/position` setters | 23 % + 18 % | handler shape refused (`callee-exception-table (handlers: not the try-call/catch-wrap-throw shape)`: the handler is the body's tail); with that admitted (`CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER=1`) the planner refuses one step later, `ir-splice-callee-handler-covered-nested-body` (the `HeapByteBuffer.<init>` inside the covered `try` must stay a call and a constructor row has no surviving call form unless `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR=1`) |
| `System$1.uncheckedInflateBytesToChars` | 9 % | `JLA.uncheckedInflateBytesToChars` is an `invokeinterface` on a `static final` of interface type; `ir-splice-target-not-provably-monomorphic` (no constant-receiver / single-implementor proof for an interface site) |

With `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER=1 CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR=1`, `wrap` is spliced into
all four callers but `<init>` is a call behind it, and the rows do not move (interleaved medians, see the
parent page's j27 wave 5 block). The gain needs the nested bodies themselves.

## What would fix it

Keep a nested body that sits inside a covered `try` spliced when every way it can leave is a deopt into
its own frame (cold throw arms are already trapped), and widen the lowering's covered-range check
(`Graph::splice_handler_covered`) over the nested body's relocated pc span, so an unarmed exit there
still refuses the compile. Today the nested span is outside the covered range, which is exactly why
`ir_splice_strip_handler_covered_nested` strips it. Two refinements make the common JDK shapes pass:

1. an `Op::New` of an initialized class raises only `Error`s (OOME, linkage / init errors); under rows
   whose catch types cannot catch one (anything but `Throwable`, `Error`, `VirtualMachineError`,
   `OutOfMemoryError`, `LinkageError` and its init / ICCE subclasses) it needs no armed edge;
2. a nested call in the span gets the enclosing callee's rows as its call-site edges (the
   `jit_local_handler_lookup` chain already resolves spliced scopes).

`decodeLoop`'s case is harder (the catch includes `Error`), so the first target is `wrap`
(`IllegalArgumentException` only), which also needs `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER` default ON.
Separately, an exact-class proof for an `invokeinterface` whose receiver is a trusted JDK `static final`
(`JLA`, `SharedSecrets` holders) would splice the inflate call.
