# `InternalError: refusing side-effecting replay` — the optimizing tier hoisted a trapping division above the branch that guards it

## Status
**FIXED 2026-08-02** (`fix/h2-testdiskfull-livelock-20260802`). Retired from
`docs/known-issues/jit/`.

`org.h2.test.db.TestMultiThread` died in **under 3 seconds** in 6 of 6 runs on
`origin/dev@86a01abf90`; it now runs the full 60 s clean in 8 of 8, with the
method that caused it still compiled by the optimizing tier.

**The original write-up's diagnosis was wrong** and is preserved below as a
worked example of how a misattributed deopt frame lies about which method is at
fault. It blamed two gates in `jit/src/x64.rs` (`can_deopt_resume` at publish vs
the `unresumable_trap` predicate at the invokedynamic trap site). Neither is
involved. The single tell that should have stopped that reading:
`MVMap.evaluateMemoryForKey` is **31 bytes of bytecode**, and the error names
**bci 123**.

## What it actually was

Two independent defects, one masking the other.

### 1. An optimizing-tier deopt frame carries no method identity

`ir_lower` records `method_key: String::new()` in every `FrameState` it builds,
and its own doc comment says the VM caller fills it in. Nobody ever did.

`try_resume_trapped_callee` (`vm/src/jit/helpers.rs`) refuses an identity-less
stash outright — `[cratonvm-deopt] callee-resume refused (no usable stash
identity): key="" bci=123` — so the `i64::MIN` deopt sentinel kept travelling up
through the compiled callers until some **unrelated** outer method's first-call
tier-up sink did `take_last_deopt()`, failed its own
`deopt_frame_matches_method` check and raised

```
java/lang/InternalError: JIT dispatch into
  org/h2/mvstore/MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I failed:
  precise deoptimization unavailable … at bci 123
  (can_deopt_resume=false …, stashed key "", …, reason UnreachedCode);
  refusing side-effecting replay
```

Every field of that message is misleading. `reason UnreachedCode` is the
`unwrap_or` default the sink falls back to when no deopt point matches the bci —
the real reason was `DivByZero`. The method named is the one that happened to be
on the JIT-dispatch boundary, not the one that trapped: the same run also blamed
`org/h2/mvstore/Page$Leaf.insertKey`, at the same impossible bci 123.

Fixed by `CompiledMethod::stamp_deopt_method_key`, called on the optimizing
tier's success path in `jit/src/lib.rs` where the name is known.

### 2. A floating `Op::Div` may be scheduled above the branch that guards it

With the identity stamped, `try_resume_trapped_callee` resumed the real
trapping method — `org.h2.util.MemoryEstimator.estimateMemory` — precisely at
its bci 123, and the failure changed to a faithful, correct-looking
`java.lang.ArithmeticException: / by zero` from `MemoryEstimator.java:77`.

That line is

```java
sum = (sum * counter + delta + (counter >> 1)) / counter;
```

and it is unreachable with `counter == 0`. It sits inside
`if (initialized == 0)`, and `counter` can only be `-1` on the arm where
`initialized != 0` decremented it. Reaching the division with a zero divisor
requires the two reads of the same `initialized` local — at bci 40 and bci 86 —
to disagree, which no correct execution can do.

The reconstructed frame proved they did:

```
locals=[…, Long(106369176847616), Undefined, Int(0), Int(69),
        Long(2199054169088), …]   stack=[Long(9026), Long(0)]
```

`statsData = 106369176847616` gives `counter = 0`, `skipSum = 69`,
`sum = 24766` — all matching their locals exactly — but `initialized` must then
be `statsData & (1 << 24) == 16777216`, and local 7 held `2199054169088`
(`(2 << 40) | 30913536`, an H2 undo-log key). The dividend, `9026`, is
`sum * 0 + delta + 0`, i.e. the arithmetic really did run with `counter == 0`.

**Root cause.** `Op::Div` / `Op::Rem` are *floating* data nodes: `add_data`
gives them two value inputs and no control edge, so
`ir_schedule::find_best_block` places them in the deepest block their operands
are available in. Disassembly of the compiled body confirmed the `IDIV RCX` at
`+0xda0` executes **before** the branch on `initialized != 0` at `+0xe01` —
i.e. unconditionally. Hoisting the *arithmetic* is fine (the result is dead on
paths the source would not have computed it). Hoisting the *trap* is not:
`ir_lower::emit_div_zero_guard` deopted at the division's bci on a path the
interpreter never reaches, and the interpreter then faithfully re-executed the
division it was parked on.

Fixed by giving the trap a control anchor the scheduler respects:
`IrBuilder::add_div_zero_guard` emits an `Op::Guard { bci }` over
`divisor != 0`, whose `[ctrl, cond]` edges pin it to the block the division
really belongs to. With the guard present the lowerer no longer traps on the
floating copy — it materialises a placeholder and jumps past the `IDIV`, exactly
as it already does for the JVMS `MIN / -1` overflow case. A graph built without
one (hand-built optimizer fixtures) keeps the old deopt, so nothing silently
loses its `ArithmeticException`. `Op::Guard` also became a DCE root; it produces
no value, so nothing else rooted it.

## Why it appeared when it did

Bisected (correctly) in the original write-up to `ebf1cf3131`, which fixed the
**optimizing tier's code-buffer estimate**. Both defects predate it. What it
changed is that `MemoryEstimator.estimateMemory` — a long-arithmetic method that
used to bail with `code buffer estimate too small` — started compiling. It is an
exposure, exactly as the original suspected.

## Validation

* `TestMultiThread`, JDK 25, `--Xmx 1g`: **8 of 8 clean for 60 s**, against
  6 of 6 dying in under 3 s on `origin/dev`. Matches the two control arms
  (`CRATONVM_JIT_DENY=MemoryEstimator.estimateMemory`,
  `CRATONVM_JIT_IR_LONG=0`) with the method still compiled by C2.
* `probes/DivProbe.java` — `idiv`/`irem`/`ldiv`/`lrem` over
  `{0, 1, -1, 7, -7, 256, MIN, MAX}²` plus the speculative shape, 200 000
  rounds: identical to stock HotSpot on the checksum **and** on the 6 400 000
  `ArithmeticException`s thrown, under the default tier and under
  `CRATONVM_JIT_FORCE_C2=1`. A trap that had been moved rather than deleted is
  exactly what that second number pins.
* `cargo test -p cratonvm-jit`: 1809 lib + 196 integration, 0 failures, with
  three new regression tests
  (`idiv_zero_divisor_trap_is_anchored_to_the_branch_that_reaches_it`,
  `every_integer_division_opcode_anchors_its_zero_divisor_trap`,
  `a_guarded_divisions_trap_comes_from_the_guard_not_the_floating_div`).

## Known trade

`ir_optimize`'s unroller already declines a loop containing a control-pinned
`Op::Guard`, so a loop containing an integer division no longer unrolls. Taken
deliberately for the correctness; revisit if a division-in-loop workload shows
up in a profile.

## Lessons worth keeping

* **A deopt frame that names an impossible bci is not a deopt frame from that
  method.** Check the method's `code_length` against the bci before reading
  anything else in the message.
* **`reason` in that message can be a default**, not an observation — the sink
  falls back to `UnreachedCode` when no deopt point matches the bci, which is
  precisely the foreign-frame case.
* **Denying the named method is not a diagnosis.** Denying
  `MVMap.evaluateMemoryFor{Key,Value}` moved the failure and looked like "a
  shape, not a method"; the real culprit was one static method neither name
  mentions, three frames down.

---

## The original write-up, as filed (diagnosis section is wrong)

Kept verbatim: its bisect and its symptom catalogue are sound and were
what made the real cause findable.

<details>
<summary>original text</summary>

# `InternalError: refusing side-effecting replay` — a compiled method carries an unconditional trap it cannot resume from

## Status
**OPEN, REGRESSION.** Bisected 2026-08-02 to `ebf1cf3131`
(*Merge perf/ir-code-buffer-estimate-20260801: measure the optimizing tier's
code-buffer estimate*). `org.h2.test.db.TestMultiThread` died in **under 3
seconds** in 7 of 8 runs on dev tip; twelve hours earlier the same class ran for
6-12 minutes and passed 5 of 6.

Very likely an **exposure, not a new defect** — see *What actually changed*.

## Severity
**HIGH.** A hard `java.lang.InternalError` out of a JIT dispatch, on a workload
that has no JIT-visible fault of its own. Any method whose compiled body carries
an unconditional trap and whose artifact publishes `can_deopt_resume == false`
is a landmine that fires on its first compiled call past a side effect.

## Symptom

```
Caused by: java/lang/InternalError: JIT dispatch into
  org/h2/mvstore/MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I failed:
  internal error: precise deoptimization unavailable for
  org/h2/mvstore/MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I at bci 123
  (can_deopt_resume=false (no deopt points, or an elided monitor),
   stashed key "", inline callers 0, reason UnreachedCode);
  refusing side-effecting replay
    at org/h2/mvstore/MVMap.operate(MVMap.java:1897)
```

H2 wraps it as `JdbcSQLNonTransientException: GeneralError`, so the visible
failure is a plain SQL error and the InternalError is three `Caused by` levels
down. Which test method dies varies with timing —
`testConcurrentLobAdd:133`, `testConcurrentAlter:154` — because the trap fires
wherever the method first warms up.

Denying the named method just moves it to its twin:

| `CRATONVM_JIT_DENY` | method that then fails |
| --- | --- |
| *(none)* | `MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I` |
| `MVMap.evaluateMemoryForKey` | `MVMap.evaluateMemoryForValue(Ljava/lang/Object;)I` |

so this is a **shape**, not one method.

## Bisect

Six runs per commit, `--Xmx 1g`, `timeout 100`; "fast fail" = exited non-zero in
under 100 s, "survived" = still running at 100 s.

| commit | fast fail | survived |
| --- | --- | --- |
| `750a95f8e3` (12 h earlier) | **0** | 6 |
| `ebf1cf3131` *merge perf/ir-code-buffer-estimate* | **4** | 2 |
| `103d8328f6` | **6** | 0 |
| `6cf267a21a` | 2 | 1 |
| `5a18a9db1c` (dev tip) | 7 | 1 |

Between `750a95f8e3` and `ebf1cf3131` there is one non-docs commit — that merge.

## What actually changed, and why this is probably an exposure

`ebf1cf3131` improves the **optimizing tier's code-buffer estimate**. Before it,
methods routinely bailed with

```
WARN JIT compile bailed: code buffer estimate too small; method stays interpreted
     method="org/h2/…" code_len=… capacity=… wanted=…
```

`MVMap.evaluateMemoryFor{Key,Value}` are one-line delegations
(`return keyType.getMemory(key);`) that plausibly sat behind that bail. Sizing
the buffer correctly lets them compile — and the artifact they compile into is
the broken thing. The estimate fix is almost certainly right; what it uncovered
is not.

## The defect: two gates asking different questions

Both live in `jit/src/x64.rs`.

**At publish** (~25570) the artifact's resume capability is:

```rust
cm.can_deopt_resume = !cm.deopt_points.is_empty() && !compiler.has_elided_monitor;
```

**At the trap emission site** (~22315) the compiler already tries to refuse to
compile such a method — its own comment says so: *"if the snapshot just built
cannot be materialised back into an interpreter frame, the method is guaranteed
to fail on its first compiled call … Compiling such a method is strictly worse
than interpreting it, so bail the whole compile."* But the predicate is:

```rust
let unresumable_trap = self
    .deopt_points
    .last()
    .is_some_and(|p| !crate::deopt::frame_state_is_resumable(&p.frame_state));
```

That asks **one** of the two questions `can_deopt_resume` asks, and gets the
other one backwards:

* `is_some_and` on an **empty** `deopt_points` yields `false` → *no bail* — yet
  an empty `deopt_points` is precisely the first disjunct that makes
  `can_deopt_resume` false. The runtime message says which one fired here:
  `can_deopt_resume=false (no deopt points, or an elided monitor)`;
* `has_elided_monitor` is not consulted at all.

So a method can pass the compile-time guard and still publish an artifact that
carries an unconditional `UnreachedCode` trap it can never resume from. The
runtime then has only the re-run-from-entry fallback, which it correctly refuses
once side effects have happened — `bci 123` in a method reached through
`MVMap.operate` is well past that point.

This is the codebase's recurring two-gates-two-questions shape; compare
`docs/known-issues/jit/` on the relocation gates.

**Whoever fixes this should decide where.** The narrow fix is to make the
emission-site guard mirror `can_deopt_resume` exactly (bail when
`deopt_points.is_empty()` or `has_elided_monitor`). The complete one is at
publish, where both `can_deopt_resume` and `has_indy_trap` are already computed
a few lines apart: an artifact with `has_indy_trap && !can_deopt_resume` is
strictly worse than no artifact and should not be published. **Do not apply the
publish-side rule blind** — `deopt_points` is empty on the default path unless
`deopt_real_enabled()`, so the naive form would refuse every trap-carrying
artifact, including the many whose re-run-from-entry fallback works fine. The
distinguishing condition is whether a side effect can precede the trap bci.

## Reproducing

```bash
cd <fresh writable dir>          # H2 writes ./data
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

~75 % of runs fail in under 3 seconds. A run that gets past ~10 s will run for
minutes — the signal is entirely in the first few seconds, which makes this a
cheap bisect target (`timeout 100`, six runs per commit).

Workaround for anyone who needs the class meanwhile — **verified 0 fast
failures in 4 runs on dev tip, against 7 of 8 unflagged**:

```bash
CRATONVM_JIT_DENY=MVMap.evaluateMemoryForKey,MVMap.evaluateMemoryForValue
```

That both methods together are sufficient is also the confirmation that the
diagnosis is complete for this class: nothing else in it carries the shape.

## Related

* `h2-update-path-throughput-20260802.md` (and the retired
  `bug-h2-testmultithread-concurrent-update-timeout` write-up) — the same class's
  throughput problem, which this now hides: the class cannot reach
  `testConcurrentUpdate` at all on an affected binary.
* `jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`, `jit-ir-tier-code-buffer-*` —
  the code-buffer sizing work this fell out of.

</details>
