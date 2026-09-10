# `cargo test --workspace` is red on `dev` tip in three places, and the fail-fast job only ever showed the first

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-10. All three failures reproduce on **pristine `origin/dev`**. One of the three is fixed here; the other two are not this branch's to fix. |
| **Gate** | `cargo test --workspace` — `ci.yml` line 252. |
| **Found by** | `claude/spring-residuals-20260910`, as the regression check on an unrelated reflection fix. |

## Summary

`cargo test --workspace` is **fail-fast**: it stops at the first failing test
binary. On `dev` tip that is `registrar_drift`, so everything after it in the
job has been unmeasured. Running `--no-fail-fast` instead:

```text
suites=347   FAILED=3   passed=18292
    the_drift_baseline_has_no_stale_rows                        (registrar_drift)
    a_warmed_up_stack_trace_keeps_every_frame_and_every_line    (stack_trace_across_tiers)
    native-builtins/src/lang_class.rs - REFLECTION_INTERNAL_EXCEPTIONS (line 1259)   (doctest)
```

| # | failure | attributed | status |
|---|---|---|---|
| 1 | drift baseline has a stale row | `0b2791ac7`, jdk-only lane | open — see below |
| 2 | warmed-up stack trace | 5/5 failures on a `dev`-tip binary | open — see below |
| 3 | doctest does not compile | `bffd90ec5`, jdk-only lane | **FIXED** on this branch |

---

## 1. The drift scanner skips `register_with_kind(`

The regenerated baseline the test prints differs from the checked-in one by
exactly one row:

```diff
-const BASELINE_TOTAL_DRIFT: usize = 1224;
-const BASELINE_TOTAL_PAIRS: usize = 1357;
+const BASELINE_TOTAL_DRIFT: usize = 1223;
+const BASELINE_TOTAL_PAIRS: usize = 1356;
```

and the row that vanished, under pass `register_p59_module`, is

```rust
("java/lang/Class", "getModule", "()Ljava/lang/Module;"),
```

### It is pre-existing

Scored at two revisions in one worktree, with the same command:

```console
$ git checkout origin/dev -- native-builtins/src/generics.rs   # revert the branch's only change
$ git diff --stat origin/dev -- native-builtins/               # (empty)
$ cargo test -p cratonvm-native-builtins --test registrar_drift
test result: FAILED. 6 passed; 1 failed
```

A source-scanning gate reads the tree at run time, so this attribution costs
one `git checkout` and no rebuild of the thing under test.

### The mechanism

`Class.getModule` is registered twice — by the synthetic-only pass
`register_p59_module` (`phases_late/reflect_invoke.rs`) and by the shipping
`register_essential_natives_with_shims` (`lib.rs`) — and the baseline carries
the row with its reason: *"essential (lib.rs) caches ONE canonical Module per
module name … p59 has its own canonical-per-name cache. Two caches for one
identity invariant."*

Commit `0b2791ac7` (*"jdk-only: Class.getModule is a shadow the contract cannot
remedy, so review it"*) changed the shipping half from `registry.register(` to
`registry.register_with_kind(`.

**Both registrations still exist. The drift is still there.** What changed is
that the scanner can no longer see one of them:

```rust
let after = p + 8;                       // p + len("register")
let q = skip_ws(t, after);
if q >= n || t[q] != b'(' {
    // `register_with_kind(`, `registered_by`, …
    i += 1;
    continue;                            // not matched, and not counted either
}
```

`register_with_kind(` has `_` where the scanner requires `(`. The exclusion is
deliberate — the comment names it — but its consequence is that **a kind
adjudication deletes a drift pair and reads as good news**, which is the exact
failure this gate exists to catch. The pair is not resolved; it is unwatched.
This is a recurrence: an earlier session found the same shape across a much
larger population and restored +54 pairs, and whatever was fixed then does not
cover this call site.

### Why not simply re-take the baseline

Re-taking turns the gate green by *agreeing* that `Class.getModule` no longer
drifts. It does drift. `registrar_drift.rs` says it in its own words:

> A re-take with no explanation is how a ratchet becomes a rubber stamp.

Honest fixes, in preference order:

1. **Teach the scanner `register_with_kind(`.** Correct, and the reason it is
   not done here: there are on the order of 750 such sites, so the count moves
   a long way at once and each restored pair needs its own sentence in the
   re-take note. That is a jdk-only-lane change with its own review.
2. **Collapse the duplicate.** One canonical-Module cache, one registration,
   and the row disappears for a real reason.
3. Re-take with the explanation *"this triple is now kind-adjudicated and the
   scanner does not see such sites"* — honest about the row, but leaves the
   gate blind for the next 750.

---

## 2. `a_warmed_up_stack_trace_keeps_every_frame_and_every_line`

Fails **5/5 on a `dev`-tip binary** and 5/5 on this branch's binary — ten runs,
same result, so it is neither this branch's doing nor an occasional flake:

```console
$ for i in 1 2 3 4 5; do
    CRATONVM_BIN=/data/cratonvm/target/release/cratonvm \
      cargo test -q -p cratonvm-vm --test stack_trace_across_tiers
  done
FAILED ×5     # and ×5 again with the branch binary
```

What *is* unstable is **which arm trips**, run to run, on the same binary:

```text
run 1  vm/tests/stack_trace_across_tiers.rs:654
       CRATONVM_JIT_NO_INLINE_FRAME_MAP=1 still shows `mid` in after_main_osr …
       got:         len=5 [leaf:-1 mid:26 outer:27 probe:42 main:66 ]
       default arm: len=5 [leaf:25 mid:26 outer:27 probe:42 main:66 ]

run 2  vm/tests/stack_trace_across_tiers.rs:628
       CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1 no longer restores the historical `-1` …
       got:         len=5 [leaf:25 mid:26 outer:27 probe:42 main:62 ]
       default arm: len=5 [leaf:25 mid:26 outer:27 probe:42 main:66 ]

run 3  vm/tests/stack_trace_across_tiers.rs:654   (as run 1, but got: leaf:25 not leaf:-1)
```

Both arms are *kill-switch* checks: the test sets a switch that is supposed to
revert a JIT feature and asserts the trace changes back. Each message already
states its own two hypotheses — the switch stopped reverting, or the feature
never engaged in that run so there was nothing to revert. The run-to-run
variation in `leaf:-1` vs `leaf:25` under the *same* switch says the second
hypothesis is live: what got compiled and inlined is not the same on every run,
and the assertions read as if it were. Establishing engagement before asserting
the revert is the first move here, not chasing the emitter.

This page does not attempt that: it records the measurement that the failure is
`dev`'s, so that the next branch to hit it does not spend the same afternoon.

---

## 3. The doctest that does not compile — fixed here

`REFLECTION_INTERNAL_EXCEPTIONS`'s doc comment (added by `bffd90ec5`,
2026-09-09) illustrates a skipped frame chain with a four-space-indented block.
rustdoc reads an indented block inside `///` as a **code** block, compiles it,
and fails:

```text
error: expected one of `!` or `::`, found `.`
    --> native-builtins/src/lang_class.rs:1260:9
1260 | SerTrace.show                    <- was resolved as the caller
```

Fenced as ```` ```text ````, which is what the block always was. No behaviour
change, so it is fixed on this branch rather than filed.

---

## Until 1 and 2 are fixed

`cargo test --workspace` cannot be read as a green/red signal on any branch.
Use `cargo test --workspace --no-fail-fast` and check that the failure set is
exactly these — that is how `claude/spring-residuals-20260910` established that
its reflection change broke none of the other 18 292 tests.
