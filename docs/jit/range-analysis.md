# Integer range analysis for bounds-check elimination

Deep-research report P1, *"Add range analysis before specialized loop
kernels"*: the repository attributes prior sieve cost to bounds-check
elimination refusing inclusive loops and non-`array.length` limits, and a
specialized change closed most of that gap. This note describes the general
proof that replaces the specialized patterns.

Status: **analysis only**. `jit/src/scev.rs` and `jit/src/loop_analysis.rs`
carry the analysis and its tests; `jit/src/x64/bce.rs` is unchanged and still
runs its own pattern set. The consumer change is specified in
[What `bce.rs` must change](#what-bcers-must-change) and is deliberately a
separate step — every guard the analysis returns has to be *emitted* before any
check may be dropped on its authority.

## The pieces

### Lattice — `scev::IntRange`

An interval over the JVM `int` domain.

| element | meaning |
|---|---|
| `Empty` | bottom: no value. A contradiction, or a provably zero-trip loop. |
| `Range { lo, hi }` | the inclusive interval, `lo <= hi` by construction. |
| `unknown()` = `Range { i32::MIN, i32::MAX }` | top: proves nothing. |

There is no "probably" element. Anything unproven is top, and top never
discharges an obligation. `join` is the interval hull (control-flow merge,
and the post-tested loop's untested first iteration); `meet` is intersection
(two facts about one value), and a contradiction becomes `Empty` rather than a
choice between the two.

Arithmetic comes in two flavours, and they are never interchanged:

* `add_no_wrap` / `sub_no_wrap` / `scale_no_wrap` / `offset_no_wrap` /
  `neg_no_wrap` evaluate in `i64` and answer `None` when the exact result is
  not representable as an `int`. A proof that hits `None` refuses.
* `add` models `iadd` and falls back to `unknown()` on wrap. Sound — a wrapped
  `int` is still an `int` — and useless for a bounds proof, which is the point.

`min_with` / `max_with` are elementwise, giving `Math.min` / `Math.max` limits
for free. `array_length()` is `[0, i32::MAX]`.

### Induction variable — `scev::AffineIv`

`iv(k) = init + stride * k` for iteration `k = 0, 1, 2, …`.

* `init: IntRange` — the entry value. `unknown()` when it is a runtime
  quantity; the proof then asks for a pre-header guard on the local instead of
  inventing a start value.
* `stride: Stride` — `Const(i32)` (unit, non-unit, or negative) or
  `Variable(local)` for `iv += step` where the step's sign is a runtime fact.
* `direction()` follows the stride's sign; `Variable` is `Direction::Unknown`.

**Monotonicity is not a consequence of the stride's sign alone.** It follows
from the sign *and* the no-wrap obligation below. Nothing in the analysis
asserts monotone behaviour without having discharged that obligation.

### Limit — `scev::BoundSource`

`Const` | `Local` | `ArrayLength` | `Field { cp_index, receiver_local }` |
`Min(a, b)` | `Max(a, b)`.

`range_in(env)` gives whatever is statically known (an `arraylength` is
non-negative; `Min`/`Max` fold through the lattice; a bare local or a field is
top). `is_invariant(modified_locals, heap_stable)` is the staleness check: a
pre-header guard is evaluated once, so a limit the body can raise would let a
later iteration's exit test admit an index past the guarded length.

### Loop — `scev::CountedLoop`

IV + `ExitCmp` + limit + `LoopForm` + `modified_locals` + `heap_stable`.

`ExitCmp` keeps `if_icmp*` (exit-when-true) polarity, so `bound_addend()`
converts the limit into the extreme value an executed iteration can hold:

| continue test | opcode | extreme executed value | addend |
|---|---|---|---|
| `iv <  n` | `if_icmpge` | `n - 1` | `-1` |
| `iv <= n` | `if_icmpgt` | `n`     | `0`  |
| `iv >  n` | `if_icmple` | `n + 1` | `+1` |
| `iv >= n` | `if_icmplt` | `n`     | `0`  |

That single number is the whole inclusive-vs-exclusive question. `bce.rs`
spells it twice — as "refuse inclusive loops" on the static path and as "emit
`JBE` instead of `JB`" on the speculative one.

`LoopForm` records whether the exit test dominates the body. It cannot be
derived from a `(header, back_edge)` pair — javac's `goto cond` rotation makes
a back-branching test pre-tested — so it is an input, and `PostTested` is the
conservative answer. A post-tested loop folds its untested first iteration
into the span and is refused outright when the entry value is unbounded.

## Overflow model

Java `int` arithmetic wraps silently (JVMS 2.11.3). Every proof states its
assumption as `OverflowModel`:

* `NoWrapProven` — shown representable from compile-time facts alone.
* `NoWrapGuarded` — holds **provided the returned pre-header guards pass**.

There is no third option, and in particular no "assume it does not wrap".

The dangerous step is the IV's own advance. For an increasing loop the last
executed value is at most `bound + addend`; the step that follows it must not
carry the IV past `i32::MAX`, or it reappears at the bottom of the range with
the exit test still passing and every elided index walks below the array base.
The obligation is discharged in one of three ways:

1. statically, when `bound_range.hi + addend + stride <= i32::MAX`;
2. as `PreheaderGuard::AtMost { term: bound, limit: i32::MAX - addend - stride }`;
3. refused (`RefusalReason::IvMayWrap`) when even the smallest admissible limit
   wraps, so no runtime guard could ever pass.

For unit-stride inclusive loops case 2 evaluates to `bound <= i32::MAX - 1`,
i.e. exactly the `bound != Integer.MAX_VALUE` check `bce.rs` hard-codes. For
unit-stride exclusive loops it evaluates to "always true" and no guard is
emitted — also matching `bce.rs`. Decreasing loops are symmetric at
`i32::MIN` (`PreheaderGuard::AtLeast`).

Two further wrap sites:

* the index expression `scale * iv + offset` is evaluated through
  `scale_no_wrap`/`offset_no_wrap` and refuses (`IndexMayWrap`) rather than
  producing a wrapped range;
* the *guard* `length >= base + addend` is specified to be evaluated in 64-bit
  so materialising the endpoint cannot itself wrap. On x86-64 that is a
  sign-extended compare, not extra work.

A `Variable` stride carries both hazards in one obligation:
`PreheaderGuard::StrideInRange { local, headroom }` means
`0 <= step && step <= i32::MAX - headroom`. The lower half stops the IV walking
backwards below the array base; the upper half stops it wrapping past the exit
test. A runtime stride under a *decreasing* test, or in a post-tested loop, is
refused — there is no guard shape that covers those.

## Proof outputs

`CountedLoop::iv_span` / `index_span` return an `IvSpan`:

* `numeric: IntRange` — a sound hull, possibly top.
* `max_terms: Vec<SymBound>` — every executed value is `<= max(max_terms)`.
* `min_terms: Vec<SymBound>` — every executed value is `>= min(min_terms)`.

A `SymBound` is `base + addend` where `base` is a constant, the loop's limit,
or `IvEntry(local)` (the IV's value read in the pre-header — the term behind
`bce.rs`'s `iv >= 0` header check). A known-constant entry value collapses to
`Const` so the consumer can discharge it without emitting anything.

The witness lists are **sets**, not single endpoints: proving `index < L`
requires `L > t` for *every* `t` in `max_terms`. That is what makes a
post-tested loop expressible (`max(entry, bound + addend)`) without weakening
the pre-tested case.

`CountedLoop::prove_index_in_bounds[_of]` answers:

* `Static` — no check, no guard, no deopt;
* `Guarded(Vec<PreheaderGuard>)` — in range once every guard is discharged;
* `Refused(RefusalReason)` — keep the per-element check.

`prove_index_in_bounds_of` additionally takes the indexed array's local. When
the limit *is* that array's length, `length >= a.length` is a tautology and the
access needs nothing — this is `bce.rs`'s whole-method
`find_bound_arraylength_provenance` result, obtained from the limit's own shape.
Naming a *different* array changes nothing, which is exactly the multi-array
out-of-bounds store that provenance pass exists to prevent.

`CountedLoop::trip_count` bounds executions as a `{min, max}` pair (exact when
they coincide, `max == 0` for a provable zero-trip loop). Constant stride and
`PreTested` only.

## Recognition

`loop_analysis::analyze_counted_loop` turns bytecode into the claim above.

* `decode_bound_expr` accepts a constant push, `iload`, `aload;arraylength`,
  `aload;getfield` / `getstatic`, and `Math.min`/`Math.max` of any of those.
  The `Math` call is identified through a caller-supplied resolver
  (`&dyn Fn(u16) -> Option<MinMax>`), because this module carries no constant
  pool; a resolver that answers `None` loses the min/max shape rather than
  mis-decoding it.
* `find_iv_stride` accepts `iinc iv, c` for any `c`, and the compound forms
  `iload iv; <const>; iadd|isub; istore iv` and `iload iv; iload s; iadd;
  istore iv`. Two modifications in one body, the commuted `step + iv`, or any
  `wide`-indexed alias of the IV refuse the loop.
* `modified_locals_strict` refuses when the body writes a local `>= 64` instead
  of silently dropping it from the `u64` set, and marks `long`/`double` high
  halves.
* `body_is_heap_stable` is blunt on purpose: any store, call, allocation or
  monitor operation makes a `Field` limit unusable.
* `constant_iv_init` proves the entry value only from a single dominating
  constant store outside the body, with no branch landing on it. `None` means
  *unknown*, never zero.
* `loop_parents` gives the nesting; `RangeEnv::with_loop_iv` binds an enclosing
  loop's proven IV range so an inner limit that mentions the outer IV inherits
  it. That is not cosmetic: for `for (i…) for (j = 0; j <= i; j++)`, knowing
  `i ∈ [0, MAX-1]` discharges the inclusive loop's wrap obligation statically,
  where the bare inner loop needs a runtime guard.

## Patterns this subsumes in `x64/bce.rs`

| `bce.rs` | what it does | replaced by |
|---|---|---|
| `LoopBoundsInfo::inclusive` doc, `bce.rs:71-78` | states inclusive loops are refused | `bound_addend()` — inclusivity is a `-1`/`0`/`+1` |
| `find_safe_array_accesses`, `bce.rs:1046-1056` | early-returns for every inclusive loop | inclusive loops prove with `addend = 0` and a `length >= n+1` guard |
| `inclusive_spec_bce_enabled`, `bce.rs:27-53` | off-by-default env flag gating inclusive speculative BCE | no longer a correctness switch; keep it, if at all, as a performance switch |
| `SpeculativeBCEGuard::inclusive`, `bce.rs:110-117` | `JBE`-vs-`JB` plus a `bound != MAX_VALUE` check | `LengthAtLeast(bound + addend + 1)` and `AtMost { bound, MAX - addend - stride }` |
| `find_induction_variable`, `bce.rs:686-709` | `iinc +1` only, or an unidentified `iadd;istore` | `find_iv_stride` — any constant stride, plus the identified variable step |
| `find_iv_step_provenance` / `IvStep`, `bce.rs:131-223` | the two canonical step shapes | same, generalised to non-unit and negative constants and `isub` |
| `analyze_loop_bound`, `bce.rs:771-780` | limit must be `iload <local>` | `decode_bound_expr` — const, local, `arraylength`, field, `Math.min`/`max` |
| `find_bound_arraylength_provenance`, `bce.rs:1177-1249` | whole-method proof that the limit *is* `A.length` | `BoundSource::ArrayLength` + `prove_index_in_bounds_of` |
| `find_iv_nonneg_start`, `bce.rs:1261-1312` | whole-method proof of a non-negative constant start | `constant_iv_init` (any constant) or `PreheaderGuard::NonNegative(IvEntry)` |
| `find_safe_array_accesses`, `bce.rs:1066-1071` + `analyze_bounds_elimination`, `bce.rs:1428-1433` | bound-local invariance via a `modified` bitmask | `BoundSource::is_invariant`, which also covers field and `min`/`max` limits |
| `analyze_bounds_elimination`, `bce.rs:1423-1427` | `step_guard` for a variable stride | `PreheaderGuard::StrideInRange` |
| `find_speculative_array_accesses`, `bce.rs:1500` | index must be exactly the IV local | `IndexExpr { scale, offset }` |

**Not subsumed, and still needed:** `analyze_array_access_operands`
(`bce.rs:854-1012`) — the operand-stack producer walk that soundly identifies
`(array_local, index_local)` for each access. It is the *producer* of
`IndexExpr`, not a competitor to it, and its STOP-on-anything-unmodelled
discipline stays load-bearing.

## What `bce.rs` must change

1. Build a `scev::CountedLoop` per loop via
   `loop_analysis::analyze_counted_loop`, supplying a `LoopForm` it can justify.
   `analyze_bounds_elimination` (`bce.rs:1317`) is the natural seam: steps 1-3d
   collapse into that one call.
2. Establish `LoopForm` honestly. `analyze_loop_bound`'s "Pattern A" (exit test
   at the header, `bce.rs:795`) is pre-tested. "Pattern B" (`bce.rs:809`, the
   `if_icmplt` continue-branch) is pre-tested **only** because javac reaches it
   through a `goto cond`; the current code does not check that, and a hand-built
   `do { } while` with the same shape is post-tested. Until that entry edge is
   verified, Pattern B must be reported `PostTested`.
3. Feed each access from `analyze_array_access_operands` into
   `prove_index_in_bounds_of(idx, Some(array_local), IntRange::array_length(),
   env)`, and act on the verdict: `Static` → add to `bounds_safe_pcs`;
   `Guarded` → emit **every** guard and then add; `Refused` → keep the check.
   A partially-emitted guard set proves nothing.
4. Emit the four guard forms in the pre-header:
   * `NonNegative(term)` — for `IvEntry(l)` this is today's `iv >= 0` header
     check; for a symbolic limit it is a new `n >= 0` compare.
   * `LengthAtLeast(term)` — today's length compare, with the `addend`
     selecting `JB` vs `JBE`. **Must be evaluated in 64-bit** (sign-extend both
     operands) so `base + addend` cannot wrap.
   * `AtMost { term, limit }` / `AtLeast { term, limit }` — today's
     `bound != Integer.MAX_VALUE` check, generalised.
   * `StrideInRange { local, headroom }` — today's `0 <= step <= MAX - bound`.
5. Keep guards per array. The analysis is array-agnostic; a `LengthAtLeast`
   discharged against `a.length` says nothing about `out.length`. The existing
   one-guard-per-distinct-array-local structure and `covered_pcs` de-spec
   bookkeeping stay exactly as they are.
6. A `Field` limit additionally needs the loop body to be heap-stable
   (`body_is_heap_stable`) *and* the pre-header guard to reload the field, since
   the guarded value must be the value the exit test will read.

## To reconcile

* **`LoopForm` for Pattern B** is the one place where adopting this analysis
  could *change* today's behaviour rather than extend it. `bce.rs` currently
  treats the continue-branch shape as if the test always dominates the body. If
  that assumption is wrong for any admitted method, it is wrong today too — the
  analysis makes the assumption explicit and refusable instead of implicit.
* **Index-vs-test IV value.** The proof assumes the index uses the IV value the
  exit test compared. `bce.rs` inherits the same assumption from
  `find_speculative_array_accesses`; the difference is that `IndexExpr::offset`
  can now express an in-body advance rather than requiring the two to coincide.
* **`inclusive_spec_bce_enabled` default.** The flag is documented off because
  inclusive elision measured as a *net loss* on the Sieve OSR artifact
  (`bce.rs:30-38`) — a code-layout effect, not a correctness one. Generalising
  the proof does not change that measurement; whether to enable it stays a
  performance decision, and should be re-measured against register-homed loop
  bodies rather than inferred from the proof getting stronger.
* **Locals `>= 64`.** `modified_locals_strict` refuses them where
  `modified_locals_in_range` (and `bce.rs`'s `find_modified_locals`) silently
  drop them. `bce.rs` compensates with scattered `local < 64` checks at each use
  site; consolidating on the strict version removes a class of "forgot one call
  site" bugs, at the cost of refusing methods with very wide frames.
* **`x64::detect_loops` vs `loop_analysis::detect_loops`** remain two
  implementations of the same thing (noted in `loop_analysis.rs`'s own doc).
  The counted-loop entry point takes a `(header, back_edge)` pair so it works
  with either, but the convergence is still owed.
