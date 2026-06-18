# The stub-ratchet gate

A one-way **compatibility ratchet** that prevents new synthetic-stub natives
from sneaking into CratonVM. It is the enforcement arm of the project rule
*"no NEW synthetic stubs"* (see `docs/synthetic-vs-real-explained.md` and the
`feedback_no_synthetic_stubs` memory note).

- **Test:** `native-builtins/tests/stub_ratchet.rs`
- **Run:** `cargo test -p cratonvm-native-builtins --test stub_ratchet`

---

## Background: the three native kinds

Every native registered in `NativeMethodRegistry`
(`native-api/src/registry.rs`) carries a `NativeKind` tag:

| Kind            | Meaning                                                                                     | Fate                |
| --------------- | ------------------------------------------------------------------------------------------- | ------------------- |
| `Intrinsic`     | A correct fast-path for a hot method (`Math.abs`, `String.length`). Same answer, faster.    | Kept forever.       |
| `Bridge`        | A native the VM genuinely needs: OS syscalls, `sun.*` internals, classes with no bytecode.  | Kept forever.       |
| `SyntheticStub` | A **fake**: placeholder / approximate / wrong return values, fabricated objects, "fake main" launcher short-circuits. Shadows correct real bytecode. | **The removal target.** |

The registry's `current_category` *defaults* to `SyntheticStub`, so anything an
author forgets to tag stays visible to the audit and is counted by this gate —
conservative by design.

---

## What the gate does

1. **Builds the default registry the way the VM does.** It calls the public
   `cratonvm_native_builtins::register_essential_natives` — the *same*
   unconditional registrar the real-JDK boot path uses
   (`vm/src/vm/vm_init.rs`). The feature-gated `synthetic-jdk` override block is
   **not** counted: the ratchet guards the default, shipped build.

2. **Censuses the `SyntheticStub` registrations.** It uses the registry's
   public census API, `NativeMethodRegistry::dump_registrations()`, which yields
   one `(class, method, descriptor, NativeKind)` row per registration, and
   counts the rows whose kind is `SyntheticStub`. No registry internals are
   touched — the test exercises the same public surface the VM ships, so no
   change to `registry.rs` was required.

3. **Asserts the count has not risen** above the frozen
   `BASELINE_SYNTHETIC_STUBS` constant in the test. A change that *adds* a
   synthetic stub pushes the count over the baseline and **fails CI**. A change
   that *removes* one is welcome.

A second test, `essential_registry_is_populated`, guards against the census
entrypoint silently breaking (which would make the ratchet pass vacuously with
"0 stubs because 0 registrations").

---

## When the gate fails

> `STUB-RATCHET REGRESSION: N SyntheticStub natives now registered, exceeding
> the frozen baseline of B.`

This means your change introduced a new synthetic stub. **The fix is almost
never to raise the baseline.** In order of preference:

1. **Make the native real.** If the method has real JDK bytecode, let it run —
   don't register a native at all, or register a correct `Bridge`/`Intrinsic`.
   This is the whole point of the no-stubs effort.
2. **Re-tag a mis-tagged native.** If you added a native that is genuinely a
   correct fast-path or an unavoidable VM bridge, tag it `Intrinsic` / `Bridge`
   via `registry.with_category(NativeKind::Bridge, |r| { ... })` (or
   `set_category`). It then leaves the `SyntheticStub` bucket and the gate
   passes — correctly, because it isn't a fake.
3. **Only as a last resort**, if a synthetic stub is genuinely, unavoidably
   needed right now, raise `BASELINE_SYNTHETIC_STUBS` and **explain why in the
   PR description**. Treat every such bump as debt to pay down.

---

## Updating the baseline

`BASELINE_SYNTHETIC_STUBS` is the **current observed count plus a small slack**
(`SLACK`, currently 16). The exact count is produced at runtime by the test, so
recompute it by running the test and reading the printed line:

```text
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
```

prints

```text
stub-ratchet: <N> SyntheticStub registrations out of <T> total (baseline <B>, slack 16)
```

- **Removing stubs (the good case):** lower `BASELINE_SYNTHETIC_STUBS` to
  `<N> + SLACK` to lock in the win. The ratchet never *forces* this, but a tight
  baseline is what makes the gate bite on the next regression. Tighten it.
- **First-run tightening:** the constant was seeded **generously** when the gate
  was first authored, because the registry is populated at runtime and could not
  be executed at authoring time. On the first CI run, read `<N>` and tighten the
  constant to `<N> + SLACK`. A loose baseline still catches a large regression; a
  tight one catches a single new stub.

The slack absorbs a couple of legitimately-needed stubs landing alongside an
unrelated change without forcing a baseline bump in the same PR. Keep it small.

---

## Wiring into CI

Add the test to the CI test matrix as its own invocation so a regression is
attributed clearly:

```bash
cargo test -p cratonvm-native-builtins --test stub_ratchet
```

It is a fast, VM-free unit-style integration test (it only builds the native
registry; it does not boot the VM, load classes, or touch the filesystem or
network), so it is cheap to run on every PR. Recommended placement: alongside the
other `cargo test` steps in the workspace CI job, or as a dedicated
"compat-ratchet" check that gates merges.

Default-feature build only — do **not** pass `--features synthetic-jdk`; the
ratchet intentionally measures the shipped default registry.
