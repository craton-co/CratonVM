# A thread-scoped flag override, memoized process-wide, switches a pass off for the whole test binary

| | |
|---|---|
| **Status** | **FIXED** 2026-09-07. The `OnceLock` is compiled out of test builds in both places that had this shape. |
| **Symptom** | Six `ir_check_elim` tests fail *together*, all saying a check that should have been eliminated was not. Passes on a re-run. |
| **Trigger** | Machine load. Nothing else. |

## What was seen

One `cargo test -p cratonvm-jit --release` run failed six tests at once:

```
a_different_index_keeps_its_bounds_check
a_fresh_allocation_needs_no_null_check_and_still_needs_bounds
the_bound_is_read_off_the_false_edge_of_a_negated_test
the_classic_counted_loop_needs_no_bounds_check
the_header_op_the_builder_actually_emits_is_recognised
the_second_access_to_the_same_element_needs_no_checks
```

Every message was of the form "this check should have been eliminated". The
lib suite took **7.37 s** against its usual **0.13 s**, because a release build
and a JVM workload were sharing the box. Three later runs — one with the tree
stashed, two idle — passed 2286/2286.

That last fact is what makes this worth a page: the natural reading is "flaky
test, move on", and the natural first guess (mine) was that some *other* test
was poisoning the kill-switch test. It is the exact opposite.

## The mechanism

```rust
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !matches!(runtime_var("CRATONVM_JIT_IR_CHECK_ELIM")…))
}
```

`flags::runtime_var` honours `flags::with_thread_overrides`, which is
**THREAD**-scoped. The `OnceLock` is **PROCESS**-wide. So whichever test thread
calls `enabled()` first decides the answer for every other test in the binary —
and `the_kill_switch_elides_nothing` called it from inside
`with_thread_overrides(&[("CRATONVM_JIT_IR_CHECK_ELIM", Some("0"))])`.

When that test won the race, `false` was latched for the whole run and every
test asserting that a check IS elided failed at once. Load changed the
interleaving, and that changed who won.

The test already carried a guard, but only for the *other* direction:

```rust
if enabled() {
    // The gate is a process-wide `OnceLock`; another test won the race to
    // initialise it. Assert nothing rather than assert something false.
    return;
}
```

That protects the kill-switch test from its neighbours. Nothing protected the
neighbours from it. It also means the kill switch was **untested on most runs** —
on a parallel run some other test almost always got there first, and this test
returned having asserted nothing.

## Reproduced deterministically

libtest runs `--test-threads=1` in name order. Adding a probe named to sort
first (`a_aaa_…`, because `a_` sorts before `aa`) that does nothing but call
`enabled()` inside the override:

```
[repro] enabled() inside the override = false
[repro] enabled() after the override  = false
… a_different_index_keeps_its_bounds_check                  FAILED
… a_fresh_allocation_needs_no_null_check_and_still_needs_…  FAILED
… the_bound_is_read_off_the_false_edge_of_a_negated_test    FAILED
… the_classic_counted_loop_needs_no_bounds_check            FAILED
… the_header_op_the_builder_actually_emits_is_recognised    FAILED
… the_second_access_to_the_same_element_needs_no_checks     FAILED
```

The same six, in the same run, from a probe that touches nothing but the gate.
After the fix the same probe prints `= true` and all 17 pass.

## The fix

The cache is compiled out of test builds:

```rust
fn read_check_elim_flag() -> bool { !matches!(runtime_var(…)…) }

pub fn enabled() -> bool {
    #[cfg(test)]
    { if let Some(f) = check_elim_forced() { return f; } return read_check_elim_flag(); }
    #[cfg(not(test))]
    { static ON: OnceLock<bool> = OnceLock::new(); *ON.get_or_init(read_check_elim_flag) }
}
```

Production is unchanged — same predicate, same latch, one read per process.

Structural rather than conventional, and that is the point: a thread-scoped
override now yields a thread-scoped answer, so the **next** test to reach a gate
through `with_thread_overrides` is correct without needing to know any of this.
A `CheckElimForce` RAII guard is added alongside (matching `ir_lower::ls_forced`,
`lib.rs::box_unbox_forced`, `OsrEntryForce`) because it says at the call site
*what* is being forced where an env-var name says only *which knob*.

`the_kill_switch_elides_nothing` loses its early return and now asserts on every
execution. `RangeForce` gives `CRATONVM_JIT_IR_BCE_RANGE` the same treatment, so
the two separately-switchable halves of the pass can be tested apart.

## The rule already existed

`x64::osr::osr_empty_stack_entry_enabled` states it, in a doc comment, as the
reason it deliberately does *not* latch:

> Not a `OnceLock`: … a process-wide latch would also put this out of reach of
> `flags::with_thread_overrides`, which is how a declared flag is arranged in a
> test.

So this was not an unknown hazard. It was a known one that two gates did not
follow.

## How many others

A scan for flags that are BOTH named in a `with_thread_overrides` edit list AND
read through a real `static … OnceLock<…>` + `get_or_init` (not merely near the
word in prose — the first cut of the scan produced four false positives that
way, including `osr_empty_stack_entry_enabled` itself) found exactly **two**
across `jit/src`, `vm/src`, `gc/src`, `types/src`:

| flag | gate | state |
|---|---|---|
| `CRATONVM_JIT_IR_CHECK_ELIM` | `ir_check_elim::enabled` | fixed here |
| `CRATONVM_JIT_DEFERRED_NEW_LOOKS` | `lib.rs::deferred_new_look_budget` | fixed here |

The second carried a near-identical early return and the identical comment
("only bites when this test wins the race"), so it had both symptoms too:
`the_zero_budget_restores_the_unbounded_behaviour` asserted nothing on most
runs, and latched an unbounded budget process-wide on the runs where it won. It
gets the same treatment.

Both scans are in the session scratch; the tightened one is worth re-running
after any batch of new gates.

## Tests

* `the_kill_switch_force_does_not_escape_its_own_thread` — the force applies on
  its own thread, is gone once dropped, and was never visible on another.
* `a_thread_override_of_the_gate_stays_on_its_own_thread` — the same for a
  `with_thread_overrides` caller. This is the one that tests the CLASS: it fails
  if anyone reinstates a process memo reachable from a thread override, which is
  the edit that would bring the whole thing back.
* `the_range_switch_drops_only_the_range_proof` — the range half is separately
  forceable, and forcing it leaves the dominating-redundancy half on.

`cargo test -p cratonvm-jit --release`: 2289 passed (2286 before, +3 new; two
previously-vacuous tests now assert).

## What this does not claim

The pass was never wrong. No production behaviour changes — the flag is read
once per process in a real build exactly as before. What changed is that the
test binary can no longer have a pass switched off underneath it by whichever
thread got there first.
