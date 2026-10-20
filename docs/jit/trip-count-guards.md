# Minimum trip counts as pre-header guards

`scev::PreheaderGuard::TripCountAtLeast` and
`scev::CountedLoop::prove_trip_count_at_least`.

**Why this exists.** Every loop transform with a profitability floor —
vectorization (lane count), unrolling (unroll factor), peeling (peel count) —
has to answer "does this loop run at least N times?". Before this shape the only
answer available was `scev::trip_count`, which returns a *compile-time*
interval, and for the commonest loop in Java:

```java
for (int i = 0; i < n; i++) { ... }   // n is a runtime value
```

that interval is `[0, i32::MAX]`. `trip.min == 0`, so every floor refused.
`docs/jit/vectorization-gate.md` names this as the single largest source of
refusals on real loops (`VecRefusal::TripCountTooSmall { min_trips: 0, .. }`),
and the same shape blocks any other transform that needs a minimum.

The fix is not a better static analysis — there is nothing static to know. It is
one pre-header compare.

---

## The contract

```rust
match loop.prove_trip_count_at_least(lanes as u64, env) {
    TripCountProof::Static      => { /* proved; emit nothing */ }
    TripCountProof::Guarded(gs) => { /* emit every guard, or treat as refused */ }
    TripCountProof::Refused(_)  => { /* the loop may run fewer times */ }
}
```

`TripCountProof` is shaped like `BoundsProof` on purpose: `Static` is a
compile-time fact with nothing to emit, `Guarded` is an obligation, `Refused` is
a refusal. **A consumer that cannot emit the guards must treat the whole verdict
as refused** — a partially-emitted guard set proves nothing.

The guard is

```rust
PreheaderGuard::TripCountAtLeast { term: SymBound, minimum: u64 }
```

and the check is literally `term >= minimum`, **evaluated in 64-bit**. `term` is
a *trip-count witness*: a runtime expression that is a lower bound on the number
of executed iterations. It is self-contained — the producer has already folded
the loop's entry value and comparator into `term.addend`, so an emitter needs to
know nothing about the loop to discharge it.

| loop | witness | `trip >= 4` becomes |
|---|---|---|
| `for (i = 0; i < n; i++)` | `n + 0` | `n >= 4` |
| `for (i = 3; i <= n; i++)` | `n - 2` | `n - 2 >= 4` |
| `for (i = e; i < n; i++)`, `e ∈ [0,5]` | `n - 5` | `n - 5 >= 4` |

The last row is the conservatism rule: when the entry value is a *range*, the
witness uses its highest admissible value — the fewest iterations the loop can
run. The guard is never optimistic.

## What is proved, and what is not

The result does **not** depend on, and deliberately does not carry, the no-wrap
obligations `iv_span` mints. A wrapping induction variable makes the loop run
*longer*, never shorter, so a lower bound on the trip count survives a wrap. A
caller that also needs the index proved in range must still take those guards
from the bounds proof.

Refused, with the same `RefusalReason` values the other proofs use:

| case | reason | why |
|---|---|---|
| post-tested loop | `UnsupportedLoopForm` | the first iteration is unconditional; `trip_count` refuses it too |
| `\|stride\| != 1` | `UnsupportedLoopForm` | the count is `floor((bound + addend - entry) / stride) + 1` and `SymBound` has no division, so the witness would not be trip-count-valued |
| decreasing loop | `UnsupportedLoopForm` | the count is `entry - (bound + addend) + 1` and `SymBound` cannot negate its base term |
| unknown entry value | `UnboundedEntry` | nothing to subtract the limit from |
| runtime / zero stride, direction mismatch, mutable limit | `UnknownStride` / `ZeroStride` / `DirectionMismatch` / `BoundNotInvariant` | identical to `iv_span`, so the two proofs never disagree about the cause |
| a minimum no admissible limit could reach | `UnusableBound` | the module's standing rule: a guard that can never pass is a refusal, not an obligation |

`minimum == 0` is `Static` for every loop, counted or not.

**Nothing else in `scev` mints this shape.** `iv_span`, `index_span` and the
bounds proofs are byte-identical to what they returned before it existed;
attaching a trip-count obligation there would silently make every existing proof
conditional on a fact it does not use. `the_new_guard_shape_never_leaks_into_an_existing_proof`
is the test that keeps it that way.

## Naming the guarded array without a JVM slot

`prove_index_in_bounds_of(idx, array_local: Option<usize>, ..)` keys the
`length >= a.length` tautology shortcut on a JVM local slot. That `usize` is
**not** opaque: `scev` also feeds it to `BoundSource::is_invariant` (as a
`modified_locals` bit, so anything `>= 64` refuses every proof) and to
`RangeEnv::array_length`. An IR-level caller holding a `NodeId` therefore cannot
substitute it — passing a node id there would not merely miss the shortcut, it
would corrupt the invariance test — which is why the vectorization gate passes
`None` and pays one `LengthAtLeast` guard per access.

`prove_index_in_bounds_of_array(idx, array_length: Option<&BoundSource>, ..)`
takes the *expression that denotes the guarded array's length* instead. Two
callers need it:

* an IR-level caller hands over the `BoundSource` its producer already built for
  `CountedLoop::bound`;
* a caller whose limit **is** this array's length spelled differently — a local
  proved to hold `a.length`, or a cached length field. That is exactly what
  `bce.rs`'s whole-method `find_bound_arraylength_provenance` pass establishes,
  and its answer can now be handed over instead of re-derived.

`prove_index_in_bounds_of` delegates to it with
`array_local.map(BoundSource::ArrayLength)`, so every existing caller is
byte-identical.

**Caller obligation:** `array_length` must denote the length of the array `idx`
indexes, on every path that reaches the loop. Naming a *different* array is the
multi-array out-of-bounds store the provenance pass exists to prevent — `scev`
cannot check this and does not try. `None` is always safe and costs one guard.

## Related

* `jit/src/scev.rs` — the implementation and its tests.
* `docs/jit/vectorization-gate.md` — the consumer whose "what an emitter still
  needs" item 2 and "out of scope" list this closed. It now **asks** for the
  witness, and its refusal-rate figure has been re-measured against a corpus
  that contains this class (12 of 29).
* the loop peeling-and-versioning design — the second
  consumer. `x64::licm::plan_loop_version` emits this guard as *bytecode*
  (`encode_preheader_guard`), which is why the addend is folded into a
  compile-time threshold rather than materialised: there is no 64-bit compare to
  emit and no constant pool to put one in.
* `docs/jit/range-analysis.md` — the surrounding proof model.
