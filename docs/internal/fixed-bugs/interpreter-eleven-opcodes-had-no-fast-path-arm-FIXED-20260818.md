# Eleven constant-pool and object opcodes had no fast-path arm — FIXED 2026-08-18

**Status:** FIXED.

**Reproducers:** `probes/InterpDecodedOpcodeCostProbe.java` (cost),
`difftest/seeds/InterpCpOpcodeParity.java` (parity).

## What was wrong

The raw-bytecode fast path in `execute_frame_from_index` had no arm for
`getstatic`, `putstatic`, `getfield`, `putfield`, `new`, `checkcast`,
`instanceof`, `monitorenter`, `monitorexit`, `ldc`, `ldc_w` or `ldc2_w` — which
includes the most frequent opcodes in ordinary OO bytecode. Each therefore paid,
on every execution, before its body ran:

* the quickened stream's `resolve(pc)` (instruction-start bitmap + per-block
  popcount),
* the frame-pointer hoist and its `code_ptr`/`quick_code_ptr` compare,
* a non-inlined call into `execute_instruction`,
* a match over a ~200-variant `Instruction`, and
* the post-call diagnostic and error-conversion checks.

## The measurement that set the order of work — and changed it

This began as "add the missing arms". The first step was sizing the overhead
rather than assuming it. Marginal ns per opcode against an `iadd` control that
already HAS an arm, `--nojit`, on dev `24a5d4528`:

| opcode | ns/op | | opcode | ns/op |
|---|---:|---|---|---:|
| `iadd` (has an arm) | 10.8 | | `getfield` | 471 |
| `ldc` | 90 | | `putfield` | 461 |
| `tableswitch` | 48 | | `getstatic` | 400 |
| `instanceof` | 308 | | `putstatic` | 375 |
| `monitorenter`+`exit` | 444 | | `checkcast` | 821 |

A `getfield` at 471 ns is ~44x an `iadd`. That is a body problem, not a dispatch
problem, and asking the field site cache's own counter why produced `hit=0` over
38 million getfields — a cache switched off since it landed. **That** was the
2.5x, and it is recorded separately in
fixed-bugs/interpreter-the-resolved-field-site-cache-was-switched-off-FIXED-20260818.md.
The arms are the smaller half, done second, and measured against the corrected
baseline rather than the inflated one.

## How the arms were built: one implementation, two callers

The nine multi-hundred-line arm bodies (`getfield` 450 lines, `putfield` 487,
`checkcast` 607) were **moved verbatim** into `opcodes::op_*` helpers that BOTH
paths call. Nothing is duplicated.

That is deliberate and is the whole design. `difftest`'s `interp-decoded` axis
exists because the interpreter already had two implementations of the same
opcodes, and its own documentation says "every fix to those 122 opcodes has to
land twice, and a divergence between them is a bug that appears under one flag
only". Copying eleven more opcodes into a second implementation to buy ~25 ns
each would have traded a maintenance hazard for a rounding error.

Two details the move had to get right, both observable:

* **`pc` is advanced BEFORE the call**, because the decoded path writes
  `next_pc` ahead of dispatch and several bodies read it back —
  `monitorenter`/`monitorexit` snapshot `frame.pc`, and the diagnostic blocks
  print it. Advancing afterwards would change an exception's reported bci.
* **Errors route through `classify_fastpath_invoke_error`**, the classifier the
  invoke fast paths already use, which performs the Runtime and Linkage
  conversions the slow path's per-opcode guard would otherwise do. That step is
  the one the invoke arms once skipped, which sent every `LinkageError` past
  every handler in every frame and killed the process.

Four `return Ok(InstructionResult::Continue)` inside `checkcast` and `new`
became `return Ok(())`; the caller's `?` reaches the same `Ok(Continue)` tail,
so the behaviour is identical.

## Measurements

Two release binaries from the same tree, differing only in these arms (the site
cache is default-ON in both), run alternately, `--nojit`, three interleaved
runs, medians of marginal ns per opcode:

| opcode | cache only | + arms | delta |
|---|---:|---:|---|
| `iadd` **(control)** | 10.9 | 9.8 | flat |
| `ldc` | 85.8 | **57.6** | −33% |
| `checkcast` | 535.6 | **439.2** | −18% |
| `instanceof` | 316.4 | **262.6** | −17% |
| `getfield` | 209.5 | **177.8** | −15% |
| `putfield` | 206.0 | **178.1** | −14% |
| `monitorenter`+`exit` | 458.0 | **389.5** | −15% |
| `getstatic` | 171.8 | 167.0 | −3%, and noisy (167/142/169) |

**The removable overhead is ~20-30 ns per opcode, not the ~12 ns predicted.**
That prediction was inferred from a kernel with ~8 opcodes per iteration during
the back-edge audit; measured per-opcode it is larger, because an arm also skips
the dispatch preamble's frame re-indexing rather than only the `resolve` and the
match. `ldc` shows the largest proportional win because it has the cheapest
body, which is the expected shape.

Across both changes, baseline dev → both: `getfield` 340 → 178, `putfield`
327 → 178, `getstatic` 316 → 167 — about **1.8-1.9x**, of which the site cache
is the large majority.

## Verification

**Engagement first, because a parity pass proves nothing if the arms never
ran.** Decoded-path dispatches on the same probe:

| binary | decoded_instr |
|---|---|
| cache only (no arms) | 3,528,091 |
| + arms | **321,334** (91% eliminated) |
| + arms, `--noverify` | 20,132,588 (arms bypassed by design) |

That is what makes `interp-decoded` a comparison here rather than a self-check:
it takes the decoded arms while `jit-on` and `nojit` take the new ones.

`difftest/seeds/InterpCpOpcodeParity.java` targets where shared bodies can still
diverge — the plumbing. Null receivers on every field shape and width, failing
`checkcast` on classes and arrays, an exception unwinding through
`monitorexit` and the lock still being reusable afterwards, `monitorenter` on
null, `ExceptionInInitializerError` on first touch of a poisoned class followed
by `NoClassDefFoundError` on re-touch (via both `getstatic` and `new`), and ldc
constant interning identity. **0/5 seeds diverged** across `jit-on`, `nojit` and
`interp-decoded`.

Regression suite **63/64** against HotSpot; the one failure,
`RImmutableFactoryTypes`, fails identically on the baseline binary.

## A defect fixed on the way

The decoded conversion normalizes `NotImplemented { feature: "operand stack
overflow" }` to `RuntimeError::StackOverflowError` before throwing, and its
comment calls itself "the only point that converts runtime errors into Java
exceptions". It is not, and has not been for as long as the invoke fast paths
have existed: the `pending_runtime_error` arm at the top of the dispatch loop
converts too, and skipped that normalization. `throw_runtime_error` maps the
un-normalized form to an *uncatchable* internal error, so an operand-stack
overflow arriving through any fast-path arm hard-unwound the whole call stack
instead of surfacing as a `java.lang.StackOverflowError` that an in-method
`catch (StackOverflowError)` or `catch (Throwable)` can observe.

Both conversion points now agree. Merging them into one is the real fix and is
left open.

## What this leaves

`tableswitch` and `lookupswitch` still have no arm. They were measured at 48 ns
and are the two whose operands are variable-length and 4-byte aligned, so an arm
must parse the padding and table from the raw bytes rather than read two operand
bytes. Worth doing, not worth bundling with this.

With the cache on and the arms in, a `getfield` costs ~178 ns against a ~10 ns
`iadd`. The remaining mass is the opcode body itself — field resolution
bookkeeping, the heap access, the barriers — not dispatch, and not resolution.
