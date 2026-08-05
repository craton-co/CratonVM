# COV-02 — a `float[]` element can be lowered and an `int[]` element cannot

**Status:** not started. **Independent of every other lane.**
**Owns:** the `0x2e` / `0x32` / `0x33` / `0x34` / `0x54` / `0x5a` / `0xbe` arms
of `IrBuilder::build`'s opcode match in `../../../jit/src/ir.rs`, and their lowering in
`../../../jit/src/ir_lower.rs`. It does **not** own the existing `0x30` / `0x31` /
`0x51` / `0x52` arms except to read them.

## The finding

`IrBuilder::build` has arms for `faload`, `daload`, `fastore` and `dastore`.
It has an arm for **no** integral or reference array access:

| opcode | mnemonic | events | has an arm? |
|---|---|---:|---|
| `0xbe` | `arraylength` | 43 | no |
| `0x32` | `aaload` | 18 | no |
| `0x5a` | `dup_x1` | 6 | no |
| `0x33` | `baload` | 6 | no |
| `0x34` | `caload` | 5 | no |
| `0x2e` | `iaload` | 3 | no |
| `0x54` | `bastore` | 2 | no |
| `0x30` | `faload` | — | **yes** |
| `0x31` | `daload` | — | **yes** |
| `0x51` | `fastore` | — | **yes** |
| `0x52` | `dastore` | — | **yes** |

77 events, second-largest opcode bucket. But the number is not the argument
here — the *shape* is. Nobody chose to support `float[]` and not `int[]`. The
arms that exist are the arms the FP benchmark kernels needed, and this
directory's own closeout rule for exactly this failure is already written down:
**"Do not size a lane from a fixture's node mix."** It happened anyway, in the
code rather than in a plan, and `meas-02` explains why nothing caught it.

`arraylength` at 43 is the single biggest item and is the cheapest: a load at a
fixed offset with a null check, no element type, no bounds check, no store
barrier.

## The first increment

`arraylength`. One node, one offset, one null check, and it is 43 of the 77
events. It also has no interaction with anything else in this lane, so it can
land while the rest is still being argued about.

Then the loads — `iaload`, `baload`, `caload`, `aaload` — as one shape with an
element width parameter, because that is what they are. Read the `faload` arm
first and copy its structure; if the four integral loads cannot be expressed the
same way, that difference is the finding and belongs in this doc before any
code.

Then `bastore`, and only then `dup_x1`, which is a stack shuffle rather than an
access and is here only because it shows up in the same method bodies.

## The three questions each load has to answer

Say the answers out loud in the PR, because the single-pass backend answers
all three and any divergence is a wrong-code bug rather than a slow one:

1. **The bounds check.** Does the IR path emit one, and if a `cov-*` lane
   later wants it elided, what proves the index in range? `../../../jit/src/x64/bce.rs`
   is the single-pass answer and is not reusable as-is.
2. **The null check.** Same question, and `../../../jit/src/x64/null_check_elim.rs` is
   the single-pass dataflow.
3. **`aaload` is a reference load.** The result is a root at every safepoint
   after it. `aastore` is deliberately *not* in this lane's scope for the
   mirror-image reason: it needs a store barrier, and a missing barrier is
   invisible until a concurrent collection.

## How to verify

* `CRATONVM_DBG=ir-compiles` before/after on `AutoConfigurationSorterTests`
  (43 of the 77 events are there): `no lowering for opcode 0xbe` to zero,
  `optimizing backend produced a body` up.
* `../../../jit/tests/ir_vs_singlepass.rs` gains a case per arm — same method, both
  backends, same answer, **including the exception cases**: a negative index,
  an index at `length`, and a null array must produce the same exception with
  the same bci from both backends.
* For `aaload`, a moving-GC test. `CRATONVM_MOVING_YOUNG` is what makes an
  unpublished root observable; without it the test passes with the bug in.

## What to refuse

An array access whose bounds or null check this lane cannot place. Emitting the
access without the check is not a faster answer, it is a SIGSEGV in generated
code — and this VM has already shipped one
(`emit_null_check_array_store`'s doc records the `iastore`/`bastore`/`aastore`
inline path dereferencing null while the helper's `process::abort()` was
"fail loudly theatre because the helpers were never reached").

`aastore` and the store barrier: out of scope, and say so in the code rather
than leaving the next reader to infer it from an absence.

## Ownership note

Shares `../../../jit/src/ir.rs`'s opcode match with `cov-01`, `cov-03` and `cov-04`.
Disjoint arms, one 365 KB file — rebase daily, land increments.

Both this lane and `cov-04` **closed 2026-08-03**, in that order. Measured on
the merged tree every row this lane owns is **zero**; in the window between the
two landings `cov-04` had moved two of them (`aaload` 18 → 19, `dup_x1` 6 → 7),
which is now moot. Details in
`cov-04-the-invoke-arms-RETIRED-20260803.md`.
