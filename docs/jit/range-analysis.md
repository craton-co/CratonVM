# Integer range analysis for bounds-check elimination

Deep-research report P1, *"Add range analysis before specialized loop
kernels"*, claims the JIT has no range analysis. **That claim was already false
when it was written, and is more false now.** This note records what exists,
what was added, and what each part is allowed to prove.

## Status, by piece

| piece | file | consumed by | state |
|---|---|---|---|
| `IntRange` — `int` interval lattice | `jit/src/scev.rs` | the counted-loop proof engine | landed, tested |
| `AffineIv` / `CountedLoop` / `prove_index_in_bounds_of` | `jit/src/scev.rs` | `x64/bce.rs` | landed, tested |
| bytecode → `CountedLoop` recognition | `jit/src/loop_analysis.rs` | `x64/bce.rs` | landed, tested |
| counted-loop BCE | `x64/bce.rs::analyze_counted_loop_bce` | `x64.rs::emit_bounds_check` | **live in production** |
| `Range` — width-carrying machine-integer lattice | `jit/src/range_analysis.rs` | below | landed, tested |
| sea-of-nodes node ranges | `jit/src/range_analysis.rs::analyze_graph` | nothing yet | landed, tested |
| guard-dominated BCE | `x64/bce.rs::range_safe_pcs` | `analyze_bounds_elimination_with_handlers` | landed, tested, **not yet wired** — see [Owed cross-file change](#owed-cross-file-change) |

The two BCE reasons are independent, independently gated, and answer different
questions. Keeping them separate is deliberate: a bounds-check regression must
be attributable to one of them without a rebuild.

| | counted-loop reason | guard-dominated reason |
|---|---|---|
| question | "is this index the IV of a loop whose exit test bounds it?" | "does a dominating comparison prove `0 <= i < a.length` here?" |
| default | ON | **OFF** — opt in with `CRATONVM_JIT_RANGE_BCE=1` |
| switch | `CRATONVM_JIT_NO_SPEC_BCE=1` (speculative half only) | `CRATONVM_JIT_RANGE_BCE=1` turns it on |
| both | `CRATONVM_JIT_NO_BCE=1` kills every reason | |
| needs the exception table | no | **yes** |

## The lattices

### `scev::IntRange` — the `int` interval

`Empty` (bottom) or `Range { lo, hi }` with `lo <= hi`; top is
`[i32::MIN, i32::MAX]`. `join` is the hull, `meet` the intersection, and a
contradiction is `Empty` rather than a choice between the two facts. Arithmetic
comes in two flavours that are never interchanged: `*_no_wrap` answers `None`
when the exact result is unrepresentable, and `add` falls back to top. There is
no "probably" element.

### `range_analysis::Range` — the machine-integer lattice

The wider sibling. Same shape, plus:

* it carries an **`IntWidth`** (`W32` for `int`/`boolean`/`byte`/`char`/`short`,
  `W64` for `long`), so a 64-bit interval can never be silently read as an
  `int` one. A cross-width `join` or arithmetic op degrades to top; a
  cross-width `meet` narrows nothing; `to_scev` on a `W64` range answers `None`
  rather than truncating;
* `mul` / `div` / `rem` / `and` / `or` / `xor` / `shl` / `shr` / `ushr`, and the
  JVMS narrowing conversions `i2b` / `i2c` / `i2s` / `i2l` / `l2i`;
* `widen`, the operator an iterating fixpoint needs.

`Range::from_scev` / `Range::to_scev` convert, so the two never become
divergent notions of "unknown".

## The overflow argument

This is the whole reason the module exists, so it is stated first and tested
first (`range_analysis::tests::add_one_to_top_is_top_not_a_shifted_interval`).

Java `int` arithmetic wraps silently (JVMS 2.11.3). An analysis that reasons in
mathematical integers concludes `i + 1 > i`. That is false exactly once — at
`Integer.MAX_VALUE` — and that single wrong inequality is enough to delete a
real bounds check and turn an array store into an out-of-bounds heap write.

Every binary transfer function therefore:

1. computes the **exact** result interval in `i128` (wide enough that no
   intermediate can itself overflow: the extreme product of two `i64`
   endpoints is `2^126`);
2. narrows it back to the declared width — **exactly** if it fits, **top** if
   it does not, because the machine operation wraps and a wrapped value can be
   anything in the width.

Top is sound (a wrapped `int` is still an `int`) and useless for a proof, which
is the intent. `add_no_wrap` / `sub_no_wrap` / `neg_no_wrap` / `offset_no_wrap`
expose the same computation but answer `None`, for callers that must *refuse*
rather than degrade.

Specific traps the tests pin:

* `unknown + 1` is top, and it **admits `Integer.MIN_VALUE`** — the value the
  wrap actually produces.
* `-Integer.MIN_VALUE` is `Integer.MIN_VALUE`; `neg_no_wrap` refuses it.
* `Integer.MIN_VALUE / -1` overflows (top); `Integer.MIN_VALUE % -1` is `0`
  and does not.
* `x << 32` shifts an `int` by **0**, not 32 — JVMS masks the shift count to
  the low 5 (`int`) / 6 (`long`) bits. Getting this wrong is not a precision
  bug.
* `(-1) >>> 1` is `0x7FFFFFFF`, so `ushr` of a possibly-negative value is
  non-negative — but only for a *known* count, since count `0` is the identity.
* `i2b` **truncates**, it does not intersect: `(byte) 300` is `44`, so a source
  range of `[0, 300]` converts to the whole of `[-128, 127]` and not to
  `[0, 127]`.

The counted-loop engine states its own assumption as an `OverflowModel`
(`NoWrapProven` or `NoWrapGuarded`, with no third option), and discharges the
IV's advance either statically, or as
`PreheaderGuard::AtMost { bound, i32::MAX - addend - stride }`, or by refusing
with `RefusalReason::IvMayWrap`. For unit-stride inclusive loops that guard
evaluates to `bound <= i32::MAX - 1`, i.e. exactly the
`bound != Integer.MAX_VALUE` check the emitter has always hard-coded.

## Facts the guard-dominated pass collects

`x64/bce.rs::range_safe_pcs` runs a flow-sensitive analysis over the method.
Its abstract state has two halves, and **both are required**:

* a `Range` per JVM local — the numeric half;
* a set of *symbolic* facts `i < a.length` — the half an interval cannot
  express, because `a.length` is a runtime value.

`i >= 0` alone proves nothing (the emitted check is an *unsigned* compare, so
it already catches negatives). `i < a.length` alone permits a negative index,
which is an access *below* the array base — strictly worse than the overrun the
check normally catches.

Facts enter the state from:

| source | example | what it yields |
|---|---|---|
| a dominating length comparison | `iload i; aload a; arraylength; if_icmpge L` | `i < a.length` on the fall-through edge |
| the reversed spelling | `aload a; arraylength; iload i; if_icmple L` | the same fact on the fall-through edge |
| a dominating constant comparison | `iload i; iflt L` | `i ∈ [0, MAX]` on the fall-through edge |
| a constant store | `iconst_0; istore i` | `i ∈ [0, 0]` |
| `iinc` | `iinc i, -1` | `add_no_wrap`; the `< length` fact survives a **non-positive** step and dies on a positive one |
| `arraylength` itself | — | `[0, Integer.MAX_VALUE]` |

Facts leave the state on any write to either local they mention, on an `iinc`
whose step cannot be proved not to wrap, and at every control-flow merge where
some predecessor does not carry them (merge is **intersection** for facts and
**hull** for ranges).

Operand decoding is positional, not stack-simulating: a run of contiguous
single-push instructions ending at a comparison or an array access leaves
exactly those values on top of whatever the stack already held. That is only
valid if no branch can land inside the run, which `range_span_is_atomic`
checks against `collect_i16_branch_targets`.

### Fail-closed rules

Every one of these returns the **empty** set, never a partial answer:

* unmodelled control flow anywhere in the method — `tableswitch`,
  `lookupswitch`, `jsr`/`ret`, `goto_w`, `jsr_w`. Their successors are not
  walked, so a fact could survive an edge the pass never saw;
* a frame wider than 64 locals, a method longer than 8192 bytes, or a decode
  that does not advance;
* an exhausted iteration budget (`code_len * 64 + 64` worklist pops). A partial
  fixpoint of a *must*-analysis claims facts it has not finished intersecting
  away, so it is worse than no answer.

Termination is by widening: after three merges into a PC, any range endpoint
that moves outward is thrown to the extreme of the width, so each endpoint can
move at most twice more. The fact set only ever shrinks. The budget is a
backstop, not the mechanism — an unbounded interval walk would pin the compiler
thread, which this VM's watchdog reports as a *VM hang* rather than as a
compiler bug.

### Exception handlers, and why the pass refuses without the table

A flow-sensitive fact is a claim about **every** way control can arrive at a
program point. An exception edge is one of those ways: it can originate at any
throwing instruction inside a protected range, and it lands at `handler_pc`
with the operand stack reset to `[throwable]`.

The pass therefore seeds every `handler_pc` with the top state (no facts), so
the handler and everything downstream of it inherit nothing a guard did not
re-establish.

`analyze_bounds_elimination` — the signature `x64.rs` calls today — has no
exception table, and the range reason is **switched off entirely** on that
path. It is not recoverable from the bytecode:

* a handler entry need not be a branch target;
* its only structural signature is "entry stack depth exactly one, holding a
  reference", and by the verifier's stack-map consistency rule any normal-flow
  entry to that PC has the same shape — which is also the shape of the middle
  of every ordinary two-operand expression (`aload a; ← here; iload i; iaload`).
  Treating every depth-one PC as a possible handler kills the facts at exactly
  the points the analysis needs them.

**The old note that handler-bearing methods are never JIT-compiled is stale.**
`jit/src/lib.rs` (the `!cached.exception_table.is_empty()` block, ~12169)
relaxed that gate: such methods now compile through the single-pass backend
unless `local_handler_reads_unsafe_local` refuses them. The exception edges are
real.

## What BCE now proves

### Counted-loop reason (live)

`analyze_counted_loop_bce` recognises each loop once
(`recognise_loop` → `loop_analysis::analyze_counted_loop`) and proves each
access once (`CountedLoop::prove_index_in_bounds_of`). Verdicts map onto the
existing output exactly: `Static` → the PC joins `safe_pcs` with no guard;
`Guarded` → **every** returned `PreheaderGuard` must be covered by
`GuardShape::covers` or the whole proof is refused; `Refused` → the per-element
check stays.

`GuardShape::covers` is the explicit statement of the pre-header emitter's
fixed repertoire (`iv >= 0`; `bound != Integer.MAX_VALUE`;
`0 <= step <= MAX - bound`; one `array.length` compare per guarded array). An
elision resting on an obligation nobody discharges is a silent out-of-bounds
access, so a partially-emitted guard set proves nothing.

Guards stay attributed **per array**. `LengthAtLeast` names no array by
contract, so three arrays produce three identical-looking guards and merging
them would discharge the shortest array's obligation with the longest array's
length — the multi-array out-of-bounds store of
`docs/known-issues/jit-bce-multi-array-oob-store-20260711.md`.

### Guard-dominated reason (landed, unwired)

Eliminates the per-element check at an array **load or store** when, on every
path reaching it:

* the array and index operands are bare locals in the positional pattern
  `aload a; iload i; xaload` (or `aload a; iload i; <one push>; xastore`);
* `i`'s range is non-empty and non-negative; **and**
* the fact `i < a.length` holds, for **that** array local.

It contributes no guards and no deopts — it only ever adds PCs to
`bounds_safe_pcs`, which the existing per-bci de-spec bookkeeping leaves alone
(a PC proven by both reasons is dropped from the set if the speculative guard
is later de-spec'd; that is conservative, and costs an elision rather than
soundness).

### Sea-of-nodes node ranges (landed, unconsumed)

`range_analysis::analyze_graph` assigns a `Range` to every `Int`/`Long`-typed
node of an `ir::Graph`: constants, `ArrayLength` (`[0, MAX]`), sub-word
`Load`/`ArrayLoad` (`byte`/`char`/`short` narrow), all the integer arithmetic
and bitwise ops, the conversions, `Cmp` (`[0, 1]`), `LCmp`/`FCmp` (`[-1, 1]`),
and `Phi` (join). Everything else is top. Cycles run only through `Phi`, which
widens from round 4; if the fixpoint has not settled by round 16 **every entry
is reset to top** and `converged()` answers `false`, because a partial
ascending fixpoint has nodes still at bottom claiming "this value cannot
occur".

Nothing consumes it yet — the IR path has no bounds-check node of its own
(`Op::ArrayLoad`/`ArrayStore` carry the check implicitly and `ir_lower` emits
it). It is the substrate for an IR-level BCE, which is the successor to the
bytecode pass, not a competitor to it.

## Owed cross-file change

One line, in `jit/src/x64.rs` (~23567):

```rust
// today
analyze_bounds_elimination(code, code_len, &loops)
// wanted
analyze_bounds_elimination_with_handlers(code, code_len, &loops, Some(&exception_ranges))
```

`exception_ranges` is already in scope at that point (bound at ~23367 from
`PENDING_EXCEPTION_RANGES`), and this is the same "bce.rs function takes the
handler table, x64.rs passes it" shape `refine_ambiguous_local_kinds` already
uses. Until it lands the guard-dominated reason is inert in production, by
design: `None` refuses rather than assuming the method has no handlers.

## What remains unvalidated

* **No end-to-end measurement.** The guard-dominated reason has unit tests and
  no benchmark. Whether removing a predicted-never-taken branch and a
  cache-hit length load is a *win* on the memory-homed template bodies this
  backend emits is an open question — the inclusive counted-loop elision
  measured a ~2x **net loss** on the Sieve OSR artifact for exactly that
  reason, which is why `CRATONVM_JIT_INCLUSIVE_BCE` is still default-off.
  Expect the same question here, and answer it with a measurement.
* **No differential run.** The pass has not been exercised against the Spring /
  H2 / Tomcat suites, nor against a control run. This is why the reason was
  flipped to **default-off** at merge: a new reason for deleting a bounds check
  does not get to be on by default before a differential run, because a wrong
  elision is an out-of-bounds heap write.
* **`analyze_graph` has no consumer**, so its transfer functions are validated
  only by their own unit tests and not by any downstream proof.
* **The positional operand decode is narrower than the loop path's.**
  `analyze_array_access_operands` simulates the operand stack and handles
  patterns the positional decode refuses; the two have not been reconciled.
* **Derived indices are not proved by either reason.** The counted-loop path
  still passes `IndexExpr::identity` (`x64/bce.rs`, the `idx_local !=
  loop_.iv.local` refusal) even though `prove_index_in_bounds_of` accepts a
  `scale`/`offset`; the guard-dominated path only reads bare locals. `a[i+1]`
  and `a[2*i]` keep their checks.
* **`x64::detect_loops` vs `loop_analysis::detect_loops`** remain two
  implementations of the same thing. The convergence is still owed.

## Patterns the counted-loop engine subsumed in `x64/bce.rs`

Retained for the record; the superseded helpers are still present because the
x64 test suite pins them as reference decodings.

| superseded helper | what it did | replaced by |
|---|---|---|
| `LoopBoundsInfo::inclusive` | states inclusive loops are refused | `bound_addend()` — inclusivity is a `-1`/`0`/`+1` |
| `find_induction_variable` | `iinc +1` only, or an unidentified `iadd;istore` | `find_iv_stride` — any constant stride, plus an identified variable step |
| `find_iv_step_provenance` / `IvStep` | the two canonical step shapes | same, generalised to non-unit/negative constants and `isub` |
| `analyze_loop_bound` | limit must be `iload <local>` | `decode_bound_expr` — const, local, `arraylength`, field, `Math.min`/`max` |
| `find_bound_arraylength_provenance` | whole-method proof that the limit *is* `A.length` | `BoundSource::ArrayLength` + `prove_index_in_bounds_of` |
| `find_iv_nonneg_start` | whole-method proof of a non-negative constant start | `constant_iv_init`, or `PreheaderGuard::NonNegative(IvEntry)` |

**Not subsumed, still load-bearing:** `analyze_array_access_operands` — the
operand-stack producer walk that soundly identifies `(array_local,
index_local)` for each in-loop access. It is the *producer* of an `IndexExpr`,
not a competitor to it, and its STOP-on-anything-unmodelled discipline is what
prevents the scatter-store misattribution that
`docs/known-issues/jit-bce-multi-array-oob-store-20260711.md` records.
