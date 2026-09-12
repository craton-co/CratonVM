# Lane 2's last fourteen rows: the hold was right, its premise was not

**2026-09-12.** Closes the one item
[`lane-2-lang-values-RETIRED-20260911.md`](lane-2-lang-values-RETIRED-20260911.md)
§6 calls "the whole of what is left here", and one it filed as somebody else's:

- the **fourteen `java/math/BigInteger` rows** held back from wave 1, now retired,
  and
- the **five `java/util/logging` triples** whose retirement had been inert since
  2026-08-11.

They are the same defect wearing two faces, and what closes them is mostly one
test.

## 1. What was owed

Wave 1 retired 10 of the 24 measured `BigInteger` triples. The other 14 were held
on a rule rather than a list:

> a row whose real body can dereference a null reference **argument** is exposed
> to the JIT's dropped `NullPointerException` message, and which rows show it
> varies run to run.

The rule was structural on purpose, and the page says why: six rows regressed on
the 24-triple binary while `modInverse`/`modPow` regressed instead on the
13-triple one, so holding exactly the rows in one diff would have frozen a coin
flip. Three of the 14 — `add`, `subtract`, `multiply` — were held for a second,
unrelated reason: a **survivor**.

`0d013f359` fixed the compiled-code message on 2026-09-11 and the page handed the
re-measurement on. The rule itself was never re-tested against the oracle.

## 2. The premise, asked of HotSpot

[`probes/L2BigIntNpe14.java`](../../../probes/L2BigIntNpe14.java) asks all
fourteen rows with a null argument, cold and then after 200,000 warm iterations
**of the same static method** — so the warm-up compiles the very body that then
receives the null, and a difference between the two lines of a pair is the
compiler and can be nothing else. On the oracle:

```text
  HotSpot 25.0.4+7, default flags           9 of 15 hot rows answer a bare `null`
  HotSpot -XX:-OmitStackTraceInFastThrow    0 of 15, hot == cold, all helpful
```

And across three default-flag runs of the same class file, **which** nine
changes:

```text
              add   subtract  multiply  remainder  gcd   modPow(exp)
  run 1       DROP  DROP      DROP      HELPFUL    HELP  DROP
  run 2       DROP  DROP      DROP      DROP       DROP  HELPFUL
  run 3       DROP  HELPFUL   DROP      HELPFUL    HELP  DROP
```

`OmitStackTraceInFastThrow` is on by default and lets C2 throw a **preallocated**
exception carrying neither message nor stack trace. So the behaviour wave 1
measured on this VM and called a regression is what the oracle does by default,
with the same instability, on the same rows.

**"Hot equals cold" was never the JDK contract.** The stable contract is the
interpreted message: identical under both oracle configurations and across runs.
A retired row that answers a bare `null` when hot is HotSpot-faithful, not
regressed.

That does not make wave 1 wrong to have held the rows. It held them on an
unexplained difference, which is the right move, and it said in writing that the
re-measurement was owed. What was wrong was the *reason* — and the reason was the
thing in the way.

## 3. The same question asked of this VM

Control and trial are the same tree; the fourteen table rows are the only
variable. Scoring rule, from §2: **a cold row must match HotSpot's message
exactly; a hot row may match it or answer the bare `null` the fast-throw
produces.**

```text
  CONTROL  ea6c958497694fde   the 14 rows NOT tabled -- natives serve
    default   rc=0  13s  30 rows  0 differing (cold 0)  fast-throw latitude used: 0
    jdkonly   rc=0  16s  30 rows  0 differing (cold 0)  fast-throw latitude used: 0

  TRIAL    828a1585978e33a2   the 14 rows tabled -- bytecode serves in strict mode
    default   rc=0  13s  30 rows  0 differing (cold 0)  fast-throw latitude used: 0
    jdkonly   rc=0  16s  30 rows  0 differing (cold 0)  fast-throw latitude used: 0

  TRIAL2   22b3f779a0bd375f   rebuilt from the FINAL tree, and reproduces it
    default   rc=0  11s  30 rows  0 differing (cold 0)  fast-throw latitude used: 0
    jdkonly   rc=0  15s  30 rows  0 differing (cold 0)  fast-throw latitude used: 0
```

Both binaries are HotSpot-exact on all 30 rows, and **this VM never takes the
fast-throw latitude**: its compiled code carries the helpful message on every hot
row, which is HotSpot's `-XX:-OmitStackTraceInFastThrow` behaviour. It answers
with strictly more information than default HotSpot and never with less.

### Two identical arms are a reason to check for engagement, not to conclude

Trial equals control here, and that is exactly what a retirement that did not
take would also look like. The natives were written to reproduce HotSpot's
message text, so the probe *cannot* tell the two dispatch routes apart by their
answers, and no amount of 0-differing rows would settle it. The refusal report
can:

```text
  --jdk-only-report, same probe run
                              refusals   BigInteger   with a survivor
    CONTROL                       3022           10                 0
    TRIAL                         3036           24                 0
```

+14 refusals, +14 `BigInteger` triples, and in the trial every one of the 24
reports `synthetic-native-registered` where the control reports the held
fourteen as `native-shadows-bytecode` / `bridge-ran-over-bytecode` /
`native-won`. The natives ran on the control and do not run on the trial. The
route changed; the answer did not.

The control's own **zero survivors** is the `java/util/logging` fix confirmed at
runtime: the control binary already carries the five deletions of §5, and where
the 2026-08-11 wave's report read "7 rows / 5 distinct triples with a survivor",
this one reads none.

## 4. The survivor, and the instrument that was missing

`add`, `subtract`, `multiply` were each registered twice:

```text
  Intrinsic  native-builtins/src/math_bignum.rs:1404/1410/1416   dead
  Bridge     native-builtins/src/phases_late.rs:8175/8185/8165    OWNER
```

`register()` re-tags a retired triple's `Bridge` to `SyntheticStub` and
`--jdk-only` refuses it. **A refusal is not a removal**: it declines to insert
*that* registration and leaves whatever is already in the slot. So the strict
registry fell back to the dead `Intrinsic`, which answers `null` for a null
argument where the real body throws. The retirement was inert — and because the
probe's rows moved, the wave still measured as accepted.

The same shape had shipped a month earlier in another wave, which lane 2's page
filed as a residual it did not own:

```text
  java/util/logging     5 triples  an Intrinsic in phases_early.rs   2026-08-11
  java/math/BigInteger  3 triples  an Intrinsic in math_bignum.rs    2026-09-10
```

Each was found by hand, months apart — the logging five from a 2029-refusal
`--jdk-only-report`, the `BigInteger` three from a probe whose rows moved for the
wrong reason. Both are answerable by `cargo test`, and now are:

```rust
#[test]
fn no_retired_triple_survives_the_strict_boot() {
    // strict_rows() ∩ the retirement tables, via triple_is_retired_shadow
}
```

One line of set arithmetic over the strict boot's own registry, asking which of
its surviving rows the tables claim. On the commit before this one it prints
exactly the five; with the 14 rows tabled and the deletions reverted it would
print exactly the three. No VM, no probe, no refusal report.

**Why the existing instruments could not see it.** `dump_registrations()` lists
every registration rather than survivors, and `NativeKind::Intrinsic` is exempt
at every dispatch door *and* exempt from the shadow census by construction — so
the native blocking a retirement is invisible to the instrument that scores the
retirement. The census is built so it cannot show this.

### The diagnosis was already in the source

`phases_early.rs` carried this above `Handler.setLevel`:

> `--jdk-only` never runs `register_synthetic_overrides`, so THIS body is the
> only one, and it owns the slot (MEASURED: `kind=intrinsic owns_slot=true
> overwrote=null inv=12`). The `java/util/logging/` shadow retirement does not
> reach it either: `retired_shadow.rs` lists the triple, but that retag fires
> only on an effective category of `Bridge` and this function's ambient category
> is `Intrinsic`.

Every clause is true, it is measured, and together they describe an inert
retirement: tabled row, no surviving stub in the census, native still
dispatching. Two earlier passes (W7-25, W7-56) had lifted *other* rows out of the
same ambient `Intrinsic` block for the same reason. What was missing was never
the diagnosis. It was an instrument that asks on every commit instead of when
somebody happens to look.

## 5. The fix

Eight dead registrations deleted, not re-tagged. Deletion is right only because
they are dead, and that was measured in all three feature arms — a registration
dead in the default arm could be the owner in another, and deleting it would then
change compatible-mode behaviour rather than only strict-mode dispatch:

```text
  arm            the 5 logging triples        the 3 BigInteger triples
  default        dead; a later row owns       dead; phases_late owns
  management     dead; a later row owns       dead; phases_late owns
  synthetic-jdk  dead; a later row owns       dead; phases_late owns
```

Compatible mode therefore dispatches exactly what it dispatched before, and
`--jdk-only` now reaches the real bytecode. `math_bignum.rs`'s bodies stay:
`register_biginteger_natives` still registers all three for synthetic-jdk mode,
where there is no real bytecode to fall to.

The `java/util/logging` five keep their comment. The measurements in it are about
the null-argument **contract** — `Handler.setLevel` throws before the store,
`Logger.setLevel(null)` legally returns, and six `lr_set` bodies return — which
the real bytecode now has to satisfy and which the next person tempted to add a
null check to a JUL setter needs to read.

## 6. What is no longer true of `RETIRED_SHADOW_L2_TRIPLES`

The "no reference parameter" rule is gone from
`the_l2_table_holds_only_what_lane_2_measured`, and a closed population replaces
it: lane 2 measured 24 `BigInteger` triples, the table holds 24, and a 25th needs
its own per-class corpus screen and probe-tree A/B. The rule could not simply be
deleted — it was the only thing standing between the table and an unmeasured row
— so what replaces it has to refuse the same additions for a reason that is still
true.

## 7. Verification

Trial `22b3f779a0bd375f`, built from this tree; the corpus runner refuses unless
the working tree's diff sha still matches the one recorded at build time, so these
are not the numbers of a binary the sources have moved past.

```text
  probe, both modes           30 rows, 0 differing from the oracle, cold and hot
  refusal report              3022 -> 3036 refusals; BigInteger 10 -> 24
                              survivors 0 on both binaries
  CRATONVM_ARGS=--jdk-only    41 passed, 0 failed
  SUITE=all                  134 passed, 0 failed
  SUITE=core                  93 passed, 0 failed

  and again on the MERGED tree (25 dev commits), binary 293c7a622331d3ca,
  working tree clean:
  probe, both modes           30 rows, 0 differing
  CRATONVM_ARGS=--jdk-only    41 passed, 0 failed
  SUITE=all                  134 passed, 0 failed
  SUITE=core                  93 passed, 0 failed

  types                      639 passed, 0 failed
  native-api --lib           429 passed, 0 failed
  native-builtins default   4285 passed, 1 failed   dev-owned, filed
                management  4317 passed, 1 failed   raw_lock_constructions_do_not_grow
                syntheticjdk 4462 passed, 1 failed
  vm --lib                  2670 passed, 1 failed   dev-owned, filed
                                                    ffm_group_layout_force_native_*
```

**The `--tests` totals above are prefixes, and that had to be fixed before they
could be read.** `cargo test -p cratonvm-native-builtins --tests` is fail-fast
across TARGETS. `lock_discipline_ratchet` is red on dev and sorts fifth of ten, so
six targets ran and four never compiled — including `stub_ratchet` and both
registrar gates, which is to say every gate this change moves. The summary still
printed `4285 passed, 1 failed`, identical to dev's, which is what "nothing
changed" looks like. So the hidden tail was run BY NAME in all three arms:

```text
  registrar_drift          7/0    7/0    7/0
  registrar_reachability   5/0    5/0    5/0
  registry_contracts      10/0   10/0   11/0
  shim_inheritance_guard   3/0    3/0    6/0
  stub_ratchet            14/0   14/0   14/0     <- 14, not 13: the new gate runs
```

`srgates.sh` now counts the targets that ran against the targets that exist and
prints `*** PREFIX ONLY` when it has measured less than it appears to.

### The stub baselines rose by exactly the rows added

```text
  BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT   2915 -> 2929
  BASELINE_SYNTHETIC_STUBS_MANAGEMENT      2942 -> 2956
  BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK   2915 -> 2929
```

A table entry re-tags the triple's `Bridge` to `SyntheticStub` in COMPATIBLE mode
too — `register` applies the retag before `register_inner`, and only `--jdk-only`
goes on to refuse — so this count rising by the number of rows added is what an
accepted wave looks like here. The values were TAKEN from the failing run's own
output rather than computed, and the same +14 appears independently in the refusal
report.

### Throughput: two arms are the control for the third

Retiring these rows hands `mod`, `divide` and `multiply` back to interpreted
bytecode, which `phases_late.rs` measures at roughly 460x on `BigInteger.mod`.
BouncyCastle's prime search does ten `mod`s per candidate, so `RJdkSecurity` is
the vector that would show it.

Two runs, at different host loads, against the same three arms on dev tip without
the table:

```text
                   dev tip   run 1   ratio    run 2   ratio
  --jdk-only           60s     93s    1.55      57s    0.95
  SUITE=all           245s    383s    1.56     289s    1.18
  SUITE=core          196s    303s    1.55     233s    1.19
```

**`SUITE=all` and `SUITE=core` do not pass `--jdk-only`, so the retirement cannot
touch them — they are the control for the arm that can.** In run 1 all three moved
by the same 1.55x, which is the host (load average 15-40 from four sibling lanes).
In run 2, on a calmer host, the two control arms are 1.18x while `--jdk-only` is
0.95x — the one arm the retirement affects did not slow relative to the arms it
cannot.

So the cost is below this instrument's noise floor on two independent runs. That
is not a claim that it is zero; it is a claim that no vector fails, none times out,
and the arm carrying the retirement does not separate from its own controls. A real
throughput number would need a parameterised `BigInteger` workload rather than a
pass/fail corpus, which is what
`a-timeout-carries-no-number-parameterise-the-workload-first` is about.

## 8. What this does not claim

- **Not that the 14 rows are free.** They are correct; the throughput question is
  separate and measured in §7. `phases_late.rs` records these limb bodies
  replacing bytecode at roughly 460x on `BigInteger.mod`, and BouncyCastle's
  prime search does ten `mod`s per candidate. `--jdk-only` is the mode that
  chooses fidelity over speed, and the corpus times are the evidence that the
  choice is affordable there; compatible mode is untouched.
- **Not that `Intrinsic` should be refused under `--jdk-only`.** An `Intrinsic` is
  spec-exact by adjudication, and keeping one is legitimate. What is not
  legitimate is an `Intrinsic` on a triple a lane has *retired* — a contradiction
  between two records. The new gate makes that contradiction a build failure
  instead of resolving it silently in the registry's favour.
- **Not that the `java/util/logging` bodies were wrong.** They are the richer,
  real-layout-aware implementations, and in compatible mode a later and simpler
  registration has been winning over them all along. That is a separate
  observation about compatible mode, not touched here.
