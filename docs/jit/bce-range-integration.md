# Wiring `x64/bce.rs` onto the general range analysis

Companion to [`range-analysis.md`](range-analysis.md), which describes the
proof engine (`jit/src/scev.rs` + `jit/src/loop_analysis.rs`) and lists the
twelve `bce.rs` patterns it subsumes. That note ended with the engine landed
and **no consumer**. This note describes the consumer.

`analyze_bounds_elimination` now:

1. recognises each loop with **one** `loop_analysis::analyze_counted_loop`
   call, wrapped in a locally-verified exit test (`recognise_loop`);
2. proves each array access with **one**
   `CountedLoop::prove_index_in_bounds_of` call;
3. accepts a `Guarded` verdict only when **every** returned `PreheaderGuard`
   is discharged by the pre-header emitter's fixed repertoire
   (`GuardShape::covers`), and refuses the whole proof otherwise.

The per-array guard structure and the `covered_pcs` de-spec bookkeeping are
unchanged, and `analyze_array_access_operands` — the operand-stack producer
walk — is untouched: it is the *producer* of the index expression, not a
competitor to it.

## Deleted

| removed | replaced by |
|---|---|
| `find_safe_array_accesses` | `BoundsProof::Static` |
| `find_speculative_array_accesses` | `BoundsProof::Guarded` + `GuardShape::covers` |

Four more are now **test-only** (`#[allow(dead_code)]`, kept because the x64
suite pins them as the reference decoding): `LoopBoundsInfo`,
`analyze_loop_bound`, `IvStep`, `find_iv_step_provenance`.
`find_bound_arraylength_provenance` and `find_iv_nonneg_start` stay live —
the first because `recognise_loop` uses it to rewrite a `Local` limit into the
`ArrayLength` it provably is, both because `x64.rs`'s SIMD coverage gate calls
them directly.

## What the pre-header can actually prove

This is the whole reason the integration is not simply "call the proof and
believe it". The emitter in `x64.rs` (the `=== Speculative BCE ===` block) has
a **fixed** repertoire, keyed off the `SpeculativeBCEGuard` fields:

| emitted | proves | guard it discharges |
|---|---|---|
| `TEST iv,iv; JS deopt` | `iv_entry >= 0` | `NonNegative(IvEntry(iv_local))` |
| `CMP bound, MAX; JE deopt` — inclusive only | `bound <= i32::MAX - 1` | `AtMost { bound, limit >= MAX-1 }` |
| `TEST step,step; JS` + `CMP step, MAX-bound; JG` | `0 <= step <= MAX - bound` | `StrideInRange { step_local, headroom = bound + k }`, `k <= 0` |
| `CMP a.length, bound; JB`/`JBE` — per array | `a.length >= bound + addend + 1` | `LengthAtLeast(bound + k)`, `k <= addend + 1` |

Everything else refuses:

* a `NonNegative` on anything but the IV's entry value — in particular a
  decreasing loop's `bound + 1` endpoint;
* a `LengthAtLeast` against a limit the pre-header does not load (a different
  array's length, a constant, an `IvEntry`);
* `AtLeast` — the decreasing loop's `i32::MIN` obligation has no encoding at
  all;
* `AtMost` under the **exclusive** comparator, because the
  `bound != Integer.MAX_VALUE` test is only emitted for the inclusive one.

A limit with no local home (`BoundSource::Const`, `Field`, `Min`/`Max`, or an
`arraylength` read inline in the test) can still take a **static** elision; it
can never take a guarded one, because `SpeculativeBCEGuard::bound_local` is a
`usize` and the emitter has nothing to load.

## `LoopForm` for Pattern B: verified, not assumed

`analyze_loop_bound` treated the `if_icmplt <body>` continue-branch as though
the exit test always dominated the body. That is true for javac's / ecj's
`goto cond` rotation and **false** for a hand-built `do { } while` with the
same bytes, and the code never checked which it had.

`pattern_b_loop_form` checks it. The criterion is deliberately weaker than
"was the loop rotated", because the elision does not need rotation — it needs
*no array access to execute before the first test*:

1. enumerate every branch edge whose target is inside `[header, back_edge_end)`
   and whose source is outside it, plus a fall-through into the header from the
   instruction that linearly precedes it;
2. the loop is `PreTested` when each such entry lands at or after the
   comparison's first instruction, or lands earlier with no array-access opcode
   between the landing point and the comparison;
3. anything else — and any method with control flow the edge scan does not
   model (`tableswitch`, `lookupswitch`, `jsr`/`ret`, `goto_w`) — is
   `PostTested`.

A `do { } while` fails step 2 immediately: its only entry is the header and its
accesses precede the test. It is then reported `PostTested`, the proof folds the
untested first iteration into the span as an extra `max_terms` witness, and the
resulting `LengthAtLeast(Const(1))` has no emitter — so the loop refuses. That
is the intended outcome: today it would have been elided on an assumption
nobody checked.

Pattern A got the same treatment from the other direction. The old code
accepted a Pattern-A-shaped compare (`if_icmpge` with a target outside the
loop) **anywhere** in the body, which admits

```
header: a[i] = …          <- access
        iload i; iload n; if_icmpge exit
        iinc i,1; goto header
```

where the access runs before any test and `n <= 0` reads `a[0]` out of an empty
array. `locate_exit_test` requires a Pattern-A compare to start exactly at the
header, which the natural loop's own definition makes a dominator of every
access.

## `inclusive_spec_bce_enabled` stays OFF

The inclusive comparator is no longer a *correctness* refusal — the proof
handles it as `bound_addend() == 0`, a `length >= n + 1` guard (JBE) and a
`n != Integer.MAX_VALUE` entry test, and `GuardShape::covers` accepts both.

The flag stays default-off anyway. Its default was never about the proof: the
elision measured a ~2x **net loss** on the Sieve OSR artifact (6.4s → 12.7s), a
code-layout effect on memory-homed template bodies. Generalising the proof does
not change that measurement, and flipping the flag on the strength of a
stronger proof would be answering a performance question with a correctness
argument. Re-measure it against register-homed loop bodies; do not infer it.

## Out-of-file needs

`bce.rs` is the only file this change may touch. Three items belong elsewhere.

### 1. `LengthAtLeast` must be a 64-bit compare — `jit/src/x64.rs:13249-13261`

`PreheaderGuard::LengthAtLeast` is specified to be evaluated in 64 bits so
`base + addend` cannot wrap. The emitted compare is a 32-bit **unsigned** one:

```rust
// CMP R10D, ECX — compare array.length vs loop_bound
self.buf.emit(&[0x44, 0x3B, 0xD1]);
self.buf.emit(if guard.inclusive {
    &[0x0F, 0x86] // JBE rel32
} else {
    &[0x0F, 0x82] // JB rel32
});
```

That is sound for the two addends in use today — the endpoint is never
materialised (`JBE` expresses `>= bound + 1` without computing it) and a
negative bound reads as a huge unsigned and deopts — but it cannot express any
other addend, and the deopt-on-negative-bound is a needless bail (a negative
limit means a zero-trip loop). Exact replacement:

```rust
// CMP R10, RCX — array.length vs loop_bound, both sign-extended to 64 bits,
// so the inclusive form's `bound + 1` endpoint cannot wrap and a negative
// bound (a zero-trip loop) no longer reads as a huge unsigned.
self.buf.emit(&[0x4D, 0x63, 0xD2]); // MOVSXD R10, R10D
self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX
self.buf.emit(&[0x4C, 0x3B, 0xD1]); // CMP R10, RCX
self.buf.emit(if guard.inclusive {
    &[0x0F, 0x8E] // JLE rel32 — deopt unless length > bound
} else {
    &[0x0F, 0x8C] // JL rel32 — deopt unless length >= bound
});
```

Until that lands, `GuardShape::covers` admits only `t.addend <= addend + 1`,
which is exactly what the 32-bit encoding proves.

### 2. `analyze_counted_loop` does not verify its exit test — `jit/src/loop_analysis.rs:872`

```rust
if let Some(cmp) = ExitCmp::from_opcode(code[q]) {
```

The first `iload x; <limit>; if_icmp*` triple in the body is taken as the
loop's exit test with no check that the branch leaves the loop, and the opcode
is mapped straight through `ExitCmp::from_opcode`'s exit-when-true convention.
Two consequences:

* an ordinary in-body `if (i >= limit) …` is read as the loop's exit
  condition, and `iv_span` then claims every executed iteration satisfies
  `i < limit` — which is false. **Unsound for any consumer that trusts it.**
* a Pattern-B continue-branch decodes to the *opposite* comparison
  (`if_icmplt` → `ExitCmp::Lt`, i.e. decreasing and inclusive, for a loop that
  is increasing and exclusive), so every rotated loop would be refused for
  `DirectionMismatch`.

`recognise_loop` compensates: it locates and verifies the exit test itself,
requires `analyze_counted_loop`'s answer to name the same `(iv, limit)`, and
then overwrites `cmp` and `form` with the verified values. The proper fix is in
`loop_analysis.rs` — take the branch target, require it to leave the loop or be
the back edge, and negate in the latter case — after which `recognise_loop`'s
fixup can be deleted. Suggested guard at the same site:

```rust
// Only a branch that LEAVES the loop is an exit test. A branch back into
// [header, end) is a continue-branch and its comparison is the negation.
let off = i16::from_be_bytes([code[q + 1], code[q + 2]]) as i32;
let target = q as i32 + off;
let leaves = target < header_pc as i32 || target >= end as i32;
let cmp = if leaves { cmp } else { negate(cmp) };
```

Until then, `analyze_counted_loop`'s `form` parameter is not the only unchecked
producer obligation it carries, and its doc comment should say so.

### 3. `docs/jit/range-analysis.md` header is stale

It still reads *"Status: **analysis only**. … `jit/src/x64/bce.rs` is unchanged
and still runs its own pattern set."* That is no longer true. The "What
`bce.rs` must change" section should become a pointer here.

## Behaviour deltas

### Newly eliminated

* **`for (int i = 0; i < a.length; i++) a[i]`** with the length read inline in
  the exit test. `analyze_loop_bound` required the limit to be a bare `iload`,
  so this — the most common loop shape in Java — got no BCE at all. It now
  proves **statically**: the limit *is* the accessed array's length, so
  `prove_index_in_bounds_of` discharges it against its own tautology and emits
  nothing.
* **Provably zero-trip loops** (`for (i = 5; i < 3; i++)`). The index span is
  `IntRange::Empty`, the body never runs, and the verdict is `Static`.
* **`isub`-spelled unit steps** (`i = i - (-1)` and friends) — `find_iv_stride`
  accepts shapes `find_iv_step_provenance` refused.
* **Statically-negative IV starts** now reach the guarded path deterministically
  rather than depending on `find_iv_nonneg_start`'s single-store shape: the
  proof's `IndexMayBeNegative` refusal is retried once with the entry value
  widened to unknown, which is exactly what the emitted `iv >= 0` test re-checks
  at runtime.

### Newly refused

* **A Pattern-A compare that is not at the header.** Previously admitted
  anywhere in the body; see the shape above.
* **A `do { } while` with the Pattern-B shape**, and any Pattern-B loop whose
  entry edge lands before the comparison with an array access in between.
* **A method with `tableswitch` / `lookupswitch` / `jsr` / `goto_w`** loses
  Pattern-B pre-testing (the edge scan fails closed to `PostTested`). Pattern A
  is unaffected.
* **A loop body writing a local `>= 64`.** `modified_locals_strict` refuses the
  loop where `find_modified_locals` clamped the write onto bit 63.
* **A Pattern-B loop whose body contains an earlier `iload x; <limit>;
  if_icmp*` triple on a differently-named IV.** `analyze_counted_loop` returns
  that triple, `recognise_loop`'s cross-check rejects it, and the loop refuses.
  Fixing item 2 above removes this loss.

### Accepted by the proof, still refused by the emitter

These are the places where the analysis is now stronger than the code that has
to discharge it. Each is a guard-emission gap, not a proof gap.

* **Non-unit constant strides.** `iinc i, 2` needs
  `AtMost { bound, i32::MAX - 1 }` for its no-wrap obligation, and the
  `bound != Integer.MAX_VALUE` test is only emitted for the inclusive
  comparator. Cheapest fix: give `SpeculativeBCEGuard` a separate
  `check_bound_max: bool` instead of overloading `inclusive`, and set it for
  any stride whose static wrap check fails.
* **Decreasing loops.** The length obligation lands on the IV's *entry* value
  (`length >= i_entry + 1`) and the non-negativity obligation on `bound + 1`;
  neither has an encoding.
* **Constant and field limits.** Provable, but with no local for the pre-header
  to load.
* **`Math.min` / `Math.max` limits.** `decode_bound_expr` needs a constant-pool
  resolver to recognise the `invokestatic`; `bce.rs` carries no constant pool
  and passes a resolver that always answers `None`, so the shape is lost rather
  than mis-decoded.
* **Non-identity index expressions.** `IndexExpr` carries `scale` and `offset`,
  but `analyze_array_access_operands` reports only `(array_local,
  index_local)`, so every index that is not literally the IV keeps its check.

### Unchanged

* Guards stay attributed per array, one per distinct array local, deduplicated
  only *within* an array. `LengthAtLeast` names no array by contract, so three
  arrays produce three identical-looking guards; merging them would discharge
  the shortest array's obligation with the longest array's length, which is the
  multi-array out-of-bounds store
  (`docs/known-issues/jit-bce-multi-array-oob-store-20260711.md`).
* `covered_pcs` and the per-bci de-spec / bypassable-header drops in
  `compile_method_full`.
* `CRATONVM_JIT_NO_SPEC_BCE` still disables only the guarded path, keeping
  static elisions.
* The step local's loop-invariance and the array local's loop-invariance are
  still checked here. Both are producer obligations `scev` states and cannot
  enforce.

## Note on Pattern-B loops in production

`find_bypassable_loop_headers` already marks every rotated loop header as
bypassable — the entry `goto` has its source outside `[header, loop_end)` and
its target inside — so `compile_method_full` drops those headers' speculative
guards and restores their `covered_pcs` before codegen. Pattern-B loops
therefore reach the emitter with their **static** elisions only. The
`LoopForm` decision above still matters for exactly those static elisions, and
for the analysis-level contract; it is not, today, the difference between a
guard being emitted and not.
