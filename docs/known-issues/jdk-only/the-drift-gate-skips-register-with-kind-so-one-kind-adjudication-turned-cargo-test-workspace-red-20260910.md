# `cargo test --workspace` is red on `dev` tip: the drift scanner skips `register_with_kind(`, so adjudicating `Class.getModule` silently erased a drift row

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-10. **Pre-existing on `origin/dev`** — reproduced with an empty diff against dev. Not caused by, and not fixable inside, the Spring-residuals change that found it. |
| **Gate** | `cargo test -p cratonvm-native-builtins --test registrar_drift`, test `the_drift_baseline_has_no_stale_rows`. `ci.yml` line 252 runs it as part of `cargo test --workspace`, which is fail-fast, so **everything the workspace job would have tested after `registrar_drift` is currently untested**. |
| **Blast radius** | Any branch, any lane. The job stops at this test. |

## The symptom

```console
$ cargo test -p cratonvm-native-builtins --test registrar_drift
failures:
    the_drift_baseline_has_no_stale_rows
test result: FAILED. 6 passed; 1 failed
```

The regenerated baseline the test prints differs from the checked-in one by
exactly one row:

```diff
-const BASELINE_TOTAL_DRIFT: usize = 1224;
-const BASELINE_TOTAL_PAIRS: usize = 1357;
+const BASELINE_TOTAL_DRIFT: usize = 1223;
+const BASELINE_TOTAL_PAIRS: usize = 1356;
```

and the row that went missing is, under pass `register_p59_module`:

```rust
("java/lang/Class", "getModule", "()Ljava/lang/Module;"),
```

## It is pre-existing

Scored at two revisions with the same command, in the same worktree:

```console
$ git checkout origin/dev -- native-builtins/src/generics.rs   # revert the branch's only change
$ git diff --stat origin/dev -- native-builtins/               # (empty)
$ cargo test -p cratonvm-native-builtins --test registrar_drift
test result: FAILED. 6 passed; 1 failed
```

Same failure with an empty diff against `origin/dev`. A source-scanning gate
reads the tree at run time, so this pairing costs one `git checkout` and is the
cheapest possible attribution.

## The mechanism

`Class.getModule` is registered twice: by the synthetic-only pass
`register_p59_module` (`phases_late/reflect_invoke.rs`) and by the shipping
`register_essential_natives_with_shims` (`lib.rs`). Two registrations of one
triple is exactly what the gate calls drift, and the row has been in the
baseline accordingly — with a comment saying why: *"essential (lib.rs) caches
ONE canonical Module per module name … p59 has its own canonical-per-name
cache. Two caches for one identity invariant."*

Commit `0b2791ac7` (*"jdk-only: Class.getModule is a shadow the contract cannot
remedy, so review it"*) changed the shipping half from

```rust
registry.register(          "java/lang/Class", "getModule", "()Ljava/lang/Module;", …)
```

to

```rust
registry.register_with_kind("java/lang/Class", "getModule", "()Ljava/lang/Module;", …)
```

**Both registrations still exist. The drift is still there.** What changed is
that the scanner can no longer see one of them. `registrar_drift.rs` matches a
registration by finding `.` then `register` then — skipping whitespace — an
open paren:

```rust
let after = p + 8;                       // p + len("register")
let q = skip_ws(t, after);
if q >= n || t[q] != b'(' {
    // `register_with_kind(`, `registered_by`, …
    i += 1;
    continue;                            // not matched, and not counted either
}
```

`register_with_kind(` has `_` where the scanner requires `(`, so it takes the
`continue`. The comment shows the exclusion is deliberate — but its consequence
is that **a kind adjudication deletes a drift pair and reads as good news**,
which is the precise failure this gate exists to prevent. The pair is not
resolved; it is unwatched.

This is a recurrence, not a new species. A 2026-08 session found the same shape
across a much larger population (the scanner blind to `register_with_kind`
sites, every adjudication erasing a pair) and restored +54 pairs. Whatever was
fixed then does not cover this call site.

## Why this page does not simply re-take the baseline

Re-taking would turn the gate green by *agreeing* that `Class.getModule` no
longer drifts. It does drift; both bodies are still in the tree, still with two
independent canonical-Module caches for an identity invariant the JDK compares
by reference. `registrar_drift.rs`'s own instructions say it plainly:

> A re-take with no explanation is how a ratchet becomes a rubber stamp.

The honest fixes, in preference order:

1. **Teach the scanner `register_with_kind(`.** Correct, and the reason it has
   not been done here: there are on the order of 750 such sites, so the row
   count would move by a lot at once and every restored pair needs its own
   sentence in the baseline's re-take note. That is a jdk-only-lane change with
   its own review, not a footnote to an unrelated branch.
2. **Collapse the duplicate.** One canonical-Module cache, one registration,
   and the row disappears for a real reason.
3. Re-take with the explanation *"this triple is now kind-adjudicated and the
   scanner does not see such sites"* — which is honest about the row but leaves
   the gate blind for the next 750.

## Until then

`cargo test --workspace` cannot be used as a green/red signal on any branch.
Use `cargo test --workspace --no-fail-fast` and read past this one failure —
that is how the Spring-residuals branch established that its change broke
nothing else.
