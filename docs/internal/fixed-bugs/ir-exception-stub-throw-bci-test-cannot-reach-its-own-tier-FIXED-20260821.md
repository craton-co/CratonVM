# `ir_exception_stub_stamps_this_methods_throw_bci` reached its own tier again — FIXED 2026-08-21

**Status: FIXED.** The probe is admissible to the optimizing tier again, the
end-to-end assertion is live, and a unit test now holds the door open without
needing a binary or a JDK.

An admission rule took the tier away from a test's probe, the test said so, and
nothing read it for four days. Two dates are the whole story:

| | landed |
|---|---|
| `vm/tests/jit_ir_exception_stub_throw_bci.rs` | 2026-08-04 (`ae1299909`) |
| `jit::ir_unresumable_protected_trap` | **2026-08-17** (`d3db57c84`) |

## The rule is correct; the probe tripped it

`ir_unresumable_protected_trap` declines the IR tier any method whose protected
range contains BOTH a deopt-guarded opcode and a side effect. The IR tier lowers
array access to a deopt guard, `can_deopt_resume` is false on a production
artifact (it is only ever set under `CRATONVM_SCALAR_DEOPT` +
`CRATONVM_DEOPT_REAL`), and replaying a range that has already committed a store
is observably wrong — the interpreter refuses and raises a hard `InternalError`.
Nothing about that is a bug, and this page does not change it.

The probe's `body` was written before the rule existed and carried exactly the
shape it declines. `javap -c`, javac 25:

```
static void body(int, int[]);
   4: iaload         <- deopt-guarded (the reported trap)
   7: iastore        <- side effect
   9: invokestatic   <- side effect
  Exception table:  from 0  to 12  target 23  any
```

so `body` went to the single-pass backend — a tier that was always correct for
this defect — and the test's own anti-vacuity assertion fired:

```
`body` was never reported as compiled by the optimizing tier —
this run proves nothing about the IR exception stub.
```

**That guard did its job.** The file's header already warned about a different
way to get a green that proves nothing (*"On a machine without them this file
provides NO coverage, silently"*); this was the second way, and it reported
FAILED rather than passing. What failed was nobody reading it.

## The fix: hoist the increment out of the `try`

```java
 static void body(int i, int[] n) {
-    try {
-        n[0] = n[0] + 1;
+    n[0] = n[0] + 1;
+    try {
         thrower(i);
     } finally {
         n[0] = n[0] - 1;
     }
 }
```

The range becomes `from 8 to 12` — `iload_0` and the `invokestatic`, and an
invoke leaves through the `i64::MIN` sentinel and needs no resume, so the rule's
**first narrowing term** (only the deopt-guarded opcodes) excludes it. The tier
takes the method again.

### Why this does not weaken the probe

The direction of the change matters and was checked, not assumed:

* the defect needs a catch-all whose protected region does **not** span the
  method, because that is the only case `find_jit_exception_handler` refuses on
  an unknown pc. The range went from `0..12` to `8..12` of a 35-byte method — 
  **narrower**, so the unknown-pc rule is if anything easier to hit;
* the balance is unchanged: the increment always runs, the decrement runs only
  if the `finally` does;
* the three pre-existing constraints the probe's doc calls load-bearing (no
  `putstatic`, no `n[0]++`/`dup2`, handler reads only a parameter local) are
  untouched.

**Verified by breaking the thing under test**, which is the only evidence that
distinguishes a real fix from a probe admitted *because* it stopped testing
anything. With `jit_set_throw_bci` removed from `ir_lower::emit_call_exc_stub`:

```
caught=200000
leaked=397032
test result: FAILED
```

and the assertion fires with its own message about the shared stub returning the
sentinel unstamped. Restored, the test is green with the anti-vacuity assertion
satisfied — `body` is reported compiled by the optimizing backend.

(`leaked` exceeds `iters` because a skipped `finally` runs the increment twice:
once in the compiled frame, once when the interpreter replays from entry. A
detail of the failure, not of the probe — the assertion is `leaked == 0`. This
page originally predicted `leaked=200000`; the measured value is the one above.)

## The recurrence guard, and why the e2e test could not be it

The e2e test needs a built binary AND a JDK, so it is skippable and slow — it
cannot be the thing that catches the next admission rule. `jit::tests::`
`the_throw_bci_probe_body_stays_admissible_to_the_optimizing_tier` needs
neither and runs in `cargo test -p cratonvm-jit`. It pins **both** shapes
against the rule:

* the current range `8..12` must be `None` — admissible;
* the pre-2026-08-21 range `0..12` must still be `Some((4, 0x2e))` — declined.

The second half is not decoration. Without it the first passes just as well when
the rule has stopped recognising anything at all, which is the failure this
whole page is about.

## The residual this exposes, which is NOT a test problem

`ir_unresumable_protected_trap` declines every method whose `try` range holds an
array or field access next to a store or a call. That is ordinary Java —
`try { buf[i] = x; flush(); } finally { … }` — and all of it is off the
optimizing tier, at single-pass code quality, for as long as the IR backend
cannot deopt-resume.

The rule's own doc names the trade honestly (*"declining here is not 'stay
interpreted' — it is 'use the backend that handles this shape'"*), and it is the
right call while `can_deopt_resume` is false. But the underlying capability —
a precise resume on the IR path outside the narrow scalar-replacement case — is
what would give that code back to C2. **Nobody has measured what it costs.** No
workload number is attached to this refusal anywhere, including here; the shape
is common enough that the number is worth taking before the capability is
scoped.

## Related

* `offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md`
  — the defect the test exists for, and its 198 927 -> 0 measurement.
* `unresumable-unconditional-trap-mvmap-FIXED-20260802.md` — why the admission
  rule exists, and the *"do not apply the publish-side rule blind"* warning that
  gave it its two narrowing terms. This page is a case of the rule working and
  a probe not keeping up with it.
* `stub-ratchet-was-a-compile-error-and-is-three-over-baseline-20260820.md` —
  the same shape a day earlier: a gate that had stopped reporting, found while
  merging rather than by the gate itself.
