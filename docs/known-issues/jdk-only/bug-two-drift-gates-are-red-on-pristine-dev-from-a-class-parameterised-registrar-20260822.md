# FOUR gates are RED on pristine `dev` — three from one refactor, one from GPU work

**Status: OPEN, MEASURED, NOT THIS BRANCH'S.** 2026-08-22.

## What is failing

`cargo test -p cratonvm-native-builtins --test registrar_drift` on a detached
worktree at **pristine `origin/dev` = `a7c22ddc1`**, carrying none of
`claude/jdk-only-mode-handoff-09b48c`:

```text
test the_drift_scanner_is_not_vacuous ................... FAILED
test the_two_gates_agree_on_the_synthetic_only_population  FAILED
test the_drift_baseline_has_no_stale_rows ............... FAILED
```

Identical numbers on the integration branch, which is how the attribution was
settled rather than argued:

```text
1027 register sites could not be resolved and are NOT the proven-benign
`arity<4` receivers (ceiling 1000, measured 950 on 2026-08-17).
Breakdown: {"arity<4": 499, "expression": 13, "format!": 18,
            "no-enclosing-fn": 16, "no-registry-owner": 14,
            "unbound-identifier": 966}

register_pe2_string_marshaling_on: synthetic-only HERE, absent from
tests/registrar_reachability.rs's closure
(defined at native-builtins/src/panama.rs:6478)
```

The third one, the two stale `MemorySegment` pairs, **is already fixed** on the
integration branch by re-taking `DRIFT_TRIPLES` (`getUtf8String(J)` and
`reinterpret(J)`, both collapsed onto `register_p67_foreign_memory`). Only the
two above are outstanding.

## One refactor causes both

`native-builtins/src/panama.rs:6472`:

```rust
fn register_pe2_string_marshaling(r: &mut NativeMethodRegistry) {
    for ms in [PE_SEGMENT_INTERFACE, CRATON_SEGMENT_CLASS] {
        register_pe2_string_marshaling_on(r, ms);
    }
}

fn register_pe2_string_marshaling_on(r: &mut NativeMethodRegistry, ms: &str) {
    r.register(ms, "getUtf8String", "(J)Ljava/lang/String;", ...)
```

The class is now the parameter `ms`, not a literal. That is exactly the
**class-parameterised registrar** the vacuity control names as a form drift can
hide inside, and it explains both failures at once:

* every `r.register(ms, …)` in the new helper lands in `unbound-identifier`,
  which is 966 of the 1027 and pushes the blind region past its 1000 ceiling;
* one scanner follows the call into the helper and the other does not, so
  `register_pe2_string_marshaling_on` is synthetic-only to one gate and unknown
  to the other;
* and it reads as a NEWLY ORPHANED pass to a third gate in the sibling file,
  `registrar_reachability.rs::no_registrar_silently_orphaned_into_the_synthetic_arm`:

  ```text
  1 registration pass(es) became synthetic-only:
      register_pe2_string_marshaling_on
          defined at native-builtins/src/panama.rs:6478
          called from register_pe2_string_marshaling @ panama.rs:6472
  ```

  That gate exists for the case where a pass HAD a shipping call site and lost
  it — "the `register_pe_panama` shape: one call site, inside
  `register_synthetic_overrides`, and two modes running different code with
  every test on the wrong one". Here it is a new name rather than a lost call
  site, but the gate cannot tell those apart, which is the third symptom of the
  same missing resolver form.

**Provenance, checked rather than inferred:** this branch never touches
`panama.rs` (`git log origin/dev..HEAD -- native-builtins/src/panama.rs` is
empty), and `register_pe2_string_marshaling_on` arrives with `aecae7e51`
*fix(ffm): every synthetic MemorySegment carried the INTERFACE as its class*.

## A FOURTH, unrelated: two GPU flags are read but declared nowhere

`cargo test -p cratonvm-types` → `flag_declaration_guard::
every_cratonvm_literal_is_declared_or_explicitly_exempt`:

```text
2 CRATONVM_* variable(s) are read by code but declared nowhere.
  CRATONVM_GPU_APPROX_MATH     first read at jit-cuda/src/analyzer.rs:245
  CRATONVM_GPU_TIME_DISPATCH   first read at native-builtins/src/craton_gpu.rs:2517
```

Also dev's, also measured rather than inferred: `CRATONVM_GPU_APPROX_MATH`
appears three times in `origin/dev:jit-cuda/src/analyzer.rs`, and
`8b813b8dd` *perf(gpu): GPULlama3 on the GPU at 1.76x HotSpot* is an ancestor
of `origin/dev`. This branch touches neither file.

**Why it matters beyond a red board**, in the guard's own words: an undeclared
flag is served by a live `getenv` rather than the latched `VmFlags` snapshot, so
`CRATONVM_<GROUP>=token` cannot reach it and `with_thread_overrides` cannot
arrange it in a test — "which is how a flag-dependent test ends up silently
measuring the developer's ambient environment". Two GPU knobs are currently in
that state.

Not fixed here for the same reason as the three above: minting a token means
choosing its group and spelling, which is the GPU lane's semantics, and the
guard offers three different remedies (declare, reuse an existing token, or
allowlist as a harness/ABI name) that only that lane can choose between.

## Do not fix it by raising the ceiling

The control says so itself, and the reason is the point: *"If a change needs to
grow it, the honest move is to teach the resolver the new form, not to raise
this number."* The 1027 is **the size of the region where drift is invisible to
`no_new_mode_drift`** — the assertion cannot see drift arriving through these
forms either, and all it claims is that the hiding place has not grown. Raising
it to 1030 would buy a green board and enlarge the blind spot in the same edit.

The resolver needs to learn one form: a registrar called in a `for` loop over a
literal array of class-name constants registers, for each `r.register(param, …)`
in its body, the cross product of that array with those triples.
`register_pe2_string_marshaling`'s two-element array is the whole worked example.

## Why this record and not a patch

`fix/panama-segment-class-identity-20260822` is still moving — `a7c22ddc1` is
its **round 2** merge — and the refactor is that lane's. Teaching the resolver a
new form is a change to the gate every lane is judged by, made while its subject
is still being edited. Recorded with the attribution measured so the owning lane
can act on a fact rather than a guess.

**What is safe to conclude meanwhile:** `no_new_mode_drift` is weaker than its
green suggests, by an amount that grew on 2026-08-22 and is now unbounded by its
own ceiling.
