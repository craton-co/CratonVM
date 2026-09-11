# `cargo test --workspace` is red on `dev` tip in seven places, and the fail-fast job only ever shows the first

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-10, re-taken 2026-09-11 at `237dc6fb1`. Every failure reproduces on **pristine `origin/dev`**. One is fixed here; the rest belong to the lanes that moved them. |
| **Gate** | `cargo test --workspace` — `ci.yml` line 252. |
| **Found by** | `claude/spring-residuals-20260910`, as the regression check on an unrelated reflection fix. |

## Summary

`cargo test --workspace` is **fail-fast**: it stops at the first failing test
binary. On `dev` tip that is `registrar_drift`, so everything after it in the
job has been unmeasured. Running `--no-fail-fast` instead, at `237dc6fb1`
(2026-09-11 04:00 UTC):

```text
suites=347   FAILED=7   (plus 4 more that only fail under host memory pressure)
```

**Every one of the seven reproduces on pristine `origin/dev`.** Six of them are
source-scanning gates, which read the tree at run time — so scoring them at a
second revision costs one `git reset --hard origin/dev` and no rebuild.

| # | failing test | cause | whose |
|---|---|---|---|
| 1 | `the_drift_baseline_has_no_stale_rows` | the scanner skips `register_with_kind(`, so kind-adjudicating `Class.getModule` deleted a drift row — see below | jdk-only (`0b2791ac7`) |
| 2 | `a_warmed_up_stack_trace_keeps_every_frame_and_every_line` | two JIT kill-switch arms; 10/10 failures across two binaries, and *which* arm trips varies run to run — see below | unattributed |
| 3 | `every_cratonvm_literal_is_declared_or_explicitly_exempt` | 3 `CRATONVM_*` variables read but declared nowhere, incl. `CRATONVM_JIT_IR_CARRY_2ND`, `CRATONVM_JIT_IR_CMP_IN_PLACE` (`jit/src/ir_lower.rs`) | c2 JIT lane |
| 4 | `flag_inventory_surface_counts_are_current` | `docs/config/flag-inventory.md` "Where the surface stands" says 1 382, the tree has 1 387 | (accumulated) |
| 5 | `the_flag_inventory_table_matches_the_declared_surface` | `CRATONVM_DBG_JIT_LOCALS_FLOOR`, `CRATONVM_DBG_ZIPIMMUNE` declared with no inventory row | jit-locals-floor lane; `bb92afbb4` |
| 6 | `the_flag_token_reference_matches_the_inventory` | `docs/flag-tokens.md` missing the same two tokens | as above |
| 7 | `synthetic_stub_count_does_not_regress` | stub ratchet: 1 907 registrations against a 1 894 baseline, slack 0 | (accumulated) |

Rows 3–7 each print the exact command that fixes them
(`tools/flag-census/render-inventory.sh`, `render-tokens.sh`, a re-take of
`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`) — but 3 and 7 need a *decision* from the
lane that moved them (which token an undeclared flag should reuse; why the stub
count grew by 13), and regenerating 4–6 while 3 is open just moves the numbers
again. They are listed here so the next branch does not re-derive them.

### Four more that are the host, not the tree

A `--no-fail-fast` run taken while the box was at load 214 with all 15 GB of
swap consumed reported **ten** failures. The extra four —
`a_forbidden_acquisition_waits_for_the_moving_cycle_to_end`,
`vthread_gc_stress_completes`,
`a_compiled_null_array_deref_carries_hotspots_message_only_when_the_gate_is_on`,
`every_exit_restores_exactly_what_the_prologue_saved` — all pass on **both**
`origin/dev` and the branch once load falls to ~35. The same run also had
`rustc` OOM-killed (`signal: 9`) compiling the `native-builtins` lib test, which
is the tell: read the signal before reading the failure list.

### One that is fixed here

`native-builtins/src/lang_class.rs`'s `REFLECTION_INTERNAL_EXCEPTIONS` doc
comment did not compile as a doctest. Fenced as `text`; details at the bottom
of this page.

---

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
