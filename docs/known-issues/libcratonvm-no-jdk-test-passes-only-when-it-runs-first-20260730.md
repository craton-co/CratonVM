# `config_from_args_fails_loudly_when_no_jdk_is_available` passes only when it runs first

| | |
|---|---|
| **Status** | OPEN. Pre-existing; order-dependent, so it looks like a flake. |
| **Category** | TEST-ISOLATION (frozen flag snapshot vs. per-test env override) |
| **Found** | 2026-07-30, in a full-workspace run. |

## Symptom

```
cargo test -p libcratonvm --lib config_from_args_fails_loudly   # passes
cargo test -p libcratonvm --lib -- --test-threads=1             # FAILS
```

The failure message shows the resolved config carrying a real JDK:

```
real-JDK mode with no JDK must not boot an empty VM:
  VmConfig { ..., java_home: Some("C:\\Program Files\\Microsoft\\jdk-25.0.3.9-hotspot"), ... }
```

— which is exactly what the test's harness set out to prevent.

## Mechanism

`with_no_jdk` (`libcratonvm/src/lib.rs`) points `CRATONVM_JAVA_HOME` at an empty
scratch directory and unsets `JAVA_HOME`, then calls `config_from_args`.

But `CRATONVM_JAVA_HOME` is a *declared CratonVM flag*, and declared flags come
from one immutable process-wide snapshot:

```rust
pub fn flags() -> &'static VmFlags {
    FLAGS.get_or_init(VmFlags::from_env)
}
```

`FLAGS` is a `OnceLock` initialised from the environment on first access. Any
earlier test in the same test binary that touches `flags()` freezes the
snapshot, and from then on `with_env("CRATONVM_JAVA_HOME", ...)` changes the
process environment but not the value the code reads.

So the test asserts real behaviour only when it wins the race to initialise
`FLAGS`. When it does not, it silently exercises the developer's actual
installed JDK. Under `--test-threads=1` the order is deterministic and it loses
every time.

This is the vacuous-gate family: a test that passes because of when it ran, not
because of what the code does. It is not specific to this test — **any** test
in any binary that tries to override a declared CratonVM flag through the
environment has the same defect, and will show up as an order-dependent flake.

## Why it is filed rather than fixed

There is no supported way to install a flag overlay over the global snapshot.
`VmFlags::from_source(&dyn FlagSource)` exists and is what the `types` unit
tests use, but it builds a fresh `VmFlags` — it does not replace the `OnceLock`
that `flags()` hands out, and nothing public does.

Fixing this properly means one of:

1. Add a test-only installer (`#[cfg(test)]` or a `test-overlay` feature) that
   lets a test replace the process snapshot, and make `with_no_jdk` use it.
2. Have the JDK-resolution path in `libcratonvm` read `CRATONVM_JAVA_HOME`
   through `runtime_var` (live-read semantics) rather than the frozen snapshot.

Option 1 is the general fix and helps every test with this shape; option 2 fixes
only this one and moves a flag out of the snapshot discipline the flag system
exists to enforce. Either is an API decision in `cratonvm-types` that wants its
owner, so this records the diagnosis rather than guessing.

## Do not

Do not "fix" this by marking the test `#[ignore]` or by asserting on whichever
result comes back. Both convert an order-dependent test into a permanently
vacuous one, which is strictly worse than a visible flake.
