# Synthetic-stub ratchet

Run:

```text
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
```

The baseline equals the exact default-registry count. There is no slack.

- If a change removes stubs, lower `BASELINE_SYNTHETIC_STUBS` to the printed
  count in the same commit.
- If it adds one, implement the behavior as real bytecode, a Bridge, or an
  Intrinsic. Raising the baseline requires an explicit design explanation and
  a tracked removal issue.
- A zero-registration registry is rejected separately so broken census wiring
  cannot pass vacuously.

## The count rising is not the same as a stub being added

There are TWO ways to push this number up and they want opposite responses:

1. a **new fake** was written — that is the case the gate was built for;
2. an **existing** registration changed kind, `Bridge` -> `SyntheticStub`.

(2) is a fake being labelled honestly, so `--jdk-only` drops the row and the
JDK's own bytecode runs. It is the opposite of a regression and it still raises
the count — for example, a gate red at +31 rows can have the bulk of that
delta be (2): the `retired_shadow` table's `ArrayList` family, `Runtime.exec`,
the `java.util.function` default methods, and the `SharedSecrets` legacy alias
have all landed as this shape before. Treating rows like that as (1) means
un-doing the improvement.

So the first step on a failure is never "find what to implement". It is
**find out which rows moved**:

```text
git worktree add /tmp/freeze <the commit that last set the baseline>
cargo test -p cratonvm-native-builtins --test stub_ratchet dump_synthetic_stubs -- --nocapture
```

Run it in both trees and `comm -23` the sorted `@@STUB` lines. Then, for each
added triple, check `native-api/src/retired_shadow.rs` and the registration
site's own comment before concluding anything.

Measure BOTH configurations before re-freezing — `--features management` and
without — and paste the two printed numbers. The file's own history has a case
of a constant derived by arithmetic from the other configuration sitting six
above the truth for a week.

## What "the default registry" means, and how it stopped meaning less

The census runs **all six registration passes `vm/src/vm/vm_init.rs` runs** on
the real-JDK boot path, in its order, behind its `set_drop_real_layout_synthetic`
flag — see `register_boot_path` in the test.

It once ran `register_essential_natives` and nothing else, while
claiming to build the registry "exactly as the VM's real-JDK boot path does". It
therefore missed `register_concurrent_natives`, `register_forkjoin_quiescence`,
`register_stamped_lock_natives`, `register_io_natives` and
`register_collections_natives` — 2,279 registrations and **384 SyntheticStub
rows**. The baseline moved 165 → 549 for that reason alone; nothing was added.

**How it was caught, because the method generalises.** L7's retag moved 364
registrations from `Bridge` to `SyntheticStub`. `regression-suite/bridge-ratchet.sh`,
which censuses a *running VM*, counted every one. This gate did not move by a
single row. Two ratchets over one VM, 364 apart — and when two gates over the
same object disagree, at least one of them is measuring the wrong object. Cross-
check the two numbers whenever either moves.

`census_covers_more_than_the_essentials_registrar` now fails if the scope is ever
narrowed back, because a narrower census reads as an *improvement* — a lower
stub count — which is the shape most likely to be waved through.

## One assertion was removed, not weakened

`strict_registry_drops_only_the_stubs` used to assert
`refused <= compat_stubs` ("JdkOnly may reject SyntheticStub and nothing else").
That property is real (contract §4) but is **not observable from a differential
census**, for three independent reasons the test documents in full: refusal is
per-attempt while the registry is per-triple; compatible mode has its own drop
rules that run *after* the JdkOnly check (`EnumSet`); and one triple can be
registered with two different kinds (`ByteBuffer.allocate`). It is enforced
structurally in `register()` and asserted hermetically in
`native-api/tests/jdk_only_registry.rs` instead. Do not re-add it here.
