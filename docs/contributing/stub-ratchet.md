# Synthetic-stub ratchet

Run:

```text
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
```

The baseline equals the exact default-registry count. There is no slack.

**That is the policy, and the gate cannot enforce it.** The assertion is
`observed <= BASELINE`, so every DECREASE passes silently and the constant sits
above the tree until someone re-freezes it. Measured 2026-09-09 by disabling the
change's own arm and re-reading the printed number instead of subtracting from
the constant:

```text
                 constant   tree     slack
NO_MANAGEMENT      1635      1632       3
MANAGEMENT         1646      1643       3
SYNTHETIC_JDK      1644      1632      12
```

The change being measured moved all three by exactly +251. Against the
constants the same uniform movement reads +248 / +248 / +239 — three numbers for
one movement, and the account written from them would have attributed the drift
to the change. The `synthetic-jdk` arm's 12 is its own finding: its true count
is IDENTICAL to `no-management`'s, so that configuration adds no stub row at all
and its constant was frozen against a tree that no longer exists.

**So before re-freezing, take the before-number.** A one-line `false &&` in
whatever predicate your change added, then rebuild the test target only, is
cheaper than the worktree recipe below and answers the question the worktree
cannot: how far the constant had already drifted from the tree it claims to
freeze.

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

The census runs **every registration pass `vm/src/vm/vm_init.rs` runs** on
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

## Attribute a delta with `CRATONVM_RATCHET_ROWS=1`, not with the dump

The failure message used to say: diff `dump_synthetic_stubs` at this commit and
at the last freeze. **That diff can be empty while the number has moved by 30.**

This count is **registrations**. `dump_synthetic_stubs` prints **distinct
triples**. Plenty of triples are registered more than once —
`ExceptionInInitializerError.<init>()V` from both `lang_misc.rs` and `lib.rs`,
`Throwable.initCause` from both `lang_misc.rs` and `reflect_annotations.rs` — so
when one registration is already a `SyntheticStub` and a retirement re-tags the
other, the count rises by one and the distinct set does not change at all.

Measured 2026-09-11: lane 2's wave 2 moved this gate **+30 / +46 / +30** across
the three arms with a **byte-identical** dump. The natural misreading of that
silence is *"my retirement did nothing"*, and the lane nearly abandoned a correct
50-row table on it. The VM disagreed — `--jdk-only-report` refusals under
`java/lang` + `java/math` went **95 → 144**, zero survivors.

So: `CRATONVM_RATCHET_ROWS=1` is the attributing instrument, per-registration and
keyed by registering file. The dump is a summary, and a weaker one than it looks.

## A motionless count is still not proof for rows outside the configuration

Scope, printed on every run as `stub-ratchet(scope):`. Measured that day on one
tree, **132 `SyntheticStub` rows the shipped VM dispatches sit outside this
census** — 54 `native-awt`, 25 `jmx`, 21 `native-collections`, 15
`jar_manifest`, 12 `native-builtins/lib.rs`, 3 elsewhere.

Fourteen of lane 2's fifty were among them, every one a `java/lang/management/*`
triple registered by `jmx.rs`. In the no-management configuration those are
invisible **by construction** — widening the boot-path replay would not recover
them — which is why that arm moved +30 and the management arm +46. Each
configuration has its own constant and that is where such rows show.

## Take both sides of any comparison from ONE tree, and watch the target dir

Two ways this went wrong the same day, both producing confident wrong numbers:

* **Cross-tree.** This census on current sources against a
  `--dump-native-registry` from a binary built weeks earlier showed 452
  disagreements. Same-tree: 2. A census on one tree against a dump from another
  measures neither.
* **A shared `CARGO_TARGET_DIR`.** Building both arms into one target dir, the
  second build printed `Finished in 0.18s`, compiled nothing, and scored one tree
  with the other's binary — cargo's freshness is by **mtime**, and a `git merge`
  writes sources older than artefacts built after it. Assert a non-zero
  `Compiling` count, copy each binary out, and print both sha256s.

`W7-30-stub-ratchet-boot-path-scope.md` §12 carries the full account.

## The strict-registry bound is derived, because the number it guards is meant to fall

`strict_registry_has_zero_synthetic_stubs` and
`strict_registry_drops_only_the_stubs` both need to know that "zero synthetic
stubs" is a statement about a populated registry. Until 2026-09-11 they asked
that as an absolute floor, `strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS`, and
that constant was re-justified by hand four times in a month: 10,500 -> 10,200
-> 10,900 -> 10,600.

It is worth being precise about why, because the same shape will be tempting
again. Strict mode refuses a `SyntheticStub` **at the door**. So a retirement
wave that re-tags N `Bridge` registrations does two different things to the two
totals this file prints:

| | compatible | strict |
|---|---|---|
| before the wave | T | T - S |
| after re-tagging N | T (unchanged — a re-tag changes a KIND) | T - S - N |

The compatible total is what `MIN_TOTAL_REGISTRATIONS` guards and it genuinely
does not move; the strict total falls by exactly N, every time, by design. An
absolute lower bound on it is therefore not a detector with headroom — it is a
re-freeze chore with a deadline, and the deadline is however many retirements
fit in the headroom. Three lanes retiring shadows in one week spends 300 rows in
days, which is what happened: `bf03c1d38` lowered it for 87 `sun/misc/Unsafe`
registrations, and the wave before that had already spent most of the rest.

The bound is now `STRICT_UNEXPLAINED_DROP_MAX`, on the GAP rather than the level:

```text
compat_total - compat_stubs - strict_total <= STRICT_UNEXPLAINED_DROP_MAX
```

Both terms come from the same run, so no wave moves it. What is left for the
constant to cover is only `alias_class` fallout — an alias copied off a refused
stub is never attempted, so one refusal can remove more than one row — which was
**3** rows when this landed, against a slack of 64.

Two things this makes easier to get right:

* **A wave no longer has to touch this file at all.** Re-freezing
  `BASELINE_SYNTHETIC_STUBS` is still a wave's job, because that is the number
  whose movement is the wave's own claim. The strict bound is not.
* **When it does go red, it means what it says.** A module that fails to
  register sheds rows on the strict side without adding stubs on the compatible
  side, so it moves the gap; a retirement moves both sides together and leaves
  the gap alone. The failure message prints the shortfall, the stub count and the
  compatible total, so the first question — "is this a wave or a shed module?" —
  is answered by the message rather than by a second measurement.
