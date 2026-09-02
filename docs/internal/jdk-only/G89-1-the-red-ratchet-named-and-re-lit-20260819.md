# G89-1 — the red ratchet, named and re-lit: 109 stubs, two columns, and a scope defect in CI

**Date** 2026-08-19
**Subject** `native-builtins/tests/stub_ratchet.rs`, `.github/workflows/ci.yml`
**Commit** `820d162da`
**Closes** `G83-1` N1 (find the 31) as a *search*; opens it as a *work list*. `G83-1` N2 (put the ratchet in CI) — it was already there, and the finding is what it was not covering.

---

## 1. The thing that was actually wrong

The stub ratchet is **blocking in CI** (`ci.yml:242`, `if: matrix.os ==
'ubuntu-latest'`). It had been failing there since **2026-08-14**.

That is worse than it sounds, and the reason is the whole point of this record.
A ratchet is a *one-way* gate: it exists so that the next change cannot add a
synthetic stub without someone saying why. Once it is red, it says nothing about
the next change — the assert fails identically whether the tree gained one stub
or a hundred. For five days every stub that landed entered behind a light that
was already red, and nothing distinguished them from the ones that made it red.

`G83-1` found the redness on 2026-08-19 and stopped at the count: 1308 against a
frozen 1277, `SLACK = 0`, 31 over. It recorded, honestly, that naming the 31
needed a script nobody had written. This is that script and its answer.

## 2. What a single frozen number cannot say

There are two ways this count rises and they have **opposite signs**:

* a **new fake** is registered — the regression the gate exists to catch;
* an **existing fake** stops claiming to be a `Bridge` — the gate becoming more
  honest about a backlog it was under-reporting.

The file already records one instance of the second kind: the *157 → 165* note
on `BASELINE_SYNTHETIC_STUBS_MANAGEMENT`, where eight `java.util.function`
registrations were re-labelled and the count rose because the tag had been
hiding them. That note argues the case in prose, per-instance, from memory of
what the change did.

It does not have to be prose. **A relabel keeps its row and moves only its kind;
a new fake adds a row.** The total registration count separates them
mechanically, and this file froze one number where it needed two.

## 3. The measurement

`synthetic_by_file()` and a `CRATONVM_RATCHET_ROWS=1` row dump were added to the
gate, and the same test run at three commits in a detached worktree, the row
sets diffed by `(file, class, method, descriptor)`:

| commit | | stubs | rows | Δ stubs | Δ rows |
|---|---|---:|---:|---:|---:|
| `8c6801820` | where 1277 was frozen, 2026-08-14 | 1277 | 12780 | — | — |
| `8a7e2727f` | last commit before this session | 1308 | 12792 | **+31** | +12 |
| `820d162da` | HEAD | 1386 | 12792 | **+78** | **0** |

The management configuration moves in lockstep: 1287 → 1396, the same +109, the
ten `jmx::*` registrars contributing their constant 10 in both.

### 3a. The 78 are relabels, and the row column proves it

Seventy-eight rows added, **zero removed, and the registry is the same 12792
rows it was**. Not one native was registered. Seventy-eight already-registered
fakes stopped claiming to be `Bridge`:

```
  +28  native-collections/src/lib.rs      java/util/Vector, all 28   (5546e0b7c)
  +27  native-io/src/watch.rs             the WatchService surface   (G88-1 §9)
  +12  native-io/src/stream_encoder.rs    sun/nio/cs StreamEncoder
  +10  native-io/src/stream_decoder.rs    sun/nio/cs StreamDecoder
  +1   native-io/src/lib.rs               the string reader/writer shim
```

This is the *157 → 165* case at ten times the scale, and this time the argument
is a column rather than a recollection.

**A third instrument agrees on `watch.rs`.** Those 27 are the retag `G88-1` §9
declined for want of an exercise and `G85-1` §3b refused to accept unverified.
They were adjudicated by the registry dump and by the arms; this census is an
*in-process boot replay*, which is neither. Three instruments, one number.

It also disagrees usefully about scale, and the disagreement is not an error.
The runtime dump reported 460 `native-collections` stubs where the session began
at 0; this replay had 432 of them before the session started. Both are right
about their own population — which is exactly why this file prints its
configuration beside every count, and why no figure from it should be quoted
without one.

### 3b. The 31 are inherited, and they are named

Thirty-five rows added, four removed, and the registry grew by twelve — so **at
most 12 of the 35 are new registrations and at least 23 are relabels**. By file:

```
  +14  native-collections/src/lib.rs
         java/util/ArrayList x12 (<init> x3, add, clear, contains, get,
         isEmpty, iterator, size, toArray x2), Arrays$ArrayList.iterator,
         Collections.synchronizedMap
  +8   native-builtins/src/lib.rs
         java/lang/Runtime.exec x6 (every overload),
         java/util/ArrayList.{<init>(I)V, iterator}
  +7   native-builtins/src/phases_late/streams.rs
         Predicate.{and,or,negate,not}, Consumer.andThen,
         BinaryOperator.{minBy,maxBy}
  +1   native-builtins/src/shared_secrets_bridge.rs   (+5 rows, -4 rows)
  +1   native-builtins/src/phases_late/ssl_security.rs
         javax/net/ssl/SSLSocketInputStream.skip(J)J
```

They arrived from merges between 2026-08-14 and 2026-08-18 and are not this
change's work. They are re-frozen **with this list** rather than absorbed
silently, because a re-freeze that does not say what it takes in is precisely
how they arrived unnoticed.

The seven in `phases_late/streams.rs` are the same shape as the eight that moved
this constant to 165: `java.util.function` **default methods**, which no JDK
declares `native` and which real bytecode already implements as one line
returning a lambda. Interface defaults, no state, no VM-owned container — by
`G88-1`'s safe-retag rule they are the cheapest end of the list, and the place
`G83-1` N1 should start.

### 3c. A standing prediction discharged, and it was off by one

The constant's merge note said, of a change (d) landing across two trees:

> exactly +2 over these constants is (d) landing and is the expected result —
> re-freeze to 1289 / 1279 and delete this section. Any other delta is a finding
> to attribute.

Measured: (d)'s file moved **+5 rows and −4, i.e. +1**, not +2 — and thirty
further stubs arrived from other merges in the same window. The attribution it
asked for is §3b. A derived number sat in that note for three days beside an
instruction to distrust it, and the instruction was right.

## 4. The scope defect in CI

The file freezes **two** constants. CI ran **one**.

`management` is the *shipping* resolve: `cratonvm-cli` enables it, so ten
`jmx::*` registrars and their stub rows are in the binary users run and were in
nothing CI adjudicated. Both configurations now run in the blocking job.

This is the identical shape to the two scope bugs the ratchet file already
records against itself — measuring `register_essential_natives` alone while
naming the boot registry, and an arm locator that matched a comment and censused
the sibling arm. **A gate whose stated population is wider than its measured one
passes for the wrong reason**, and it has now happened three times in one file.
The third instance was in the workflow rather than the test, which is why
neither of the file's own self-checks could see it.

## 5. What landed

* `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` 1277 → **1386**,
  `…_MANAGEMENT` 1287 → **1396**, with §§3a–3c as the constant's doc.
* `synthetic_by_file()` — the per-file breakdown, printed on **every** run and
  the top eight into the failure message. A green ratchet whose *composition*
  shifted underneath it is the case a single number is structurally unable to
  show.
* `CRATONVM_RATCHET_ROWS=1` — the population by name, so two commits can be
  diffed by row instead of by count. Off by default; 1386 lines is not gate
  output.
* `MEASURED_TOTAL_REGISTRATIONS_*` — the row column, recorded but **not**
  asserted (a new `Bridge` legitimately raises it, so a gate there would fire on
  correct work). The failure message uses it to classify: total unchanged means
  relabels, total up by roughly the stub delta means new fakes.
* `ci.yml` runs both configurations.

Verified: both configurations green (10 passed, 1 ignored, 1 `#[ignore]`d
end-state gate untouched); `ci.yml` parses. No `src/` file is touched, so the
three suite arms cannot move and were not re-run.

## 6. What this does NOT do

**It does not close the P0 row.** *Residual synthetic native set* closes when the
1330-odd stubs are retired, and `strict_mode_refuses_nothing` — the `#[ignore]`d
end-state gate in the same file — still reports them refused rather than gone.
Its own doc comment defers that to wave 2 under contract §8. What changed is
that the row's cited evidence is no longer red, and its backlog now has a
named first item instead of a search.

**It does not adjudicate the 31 as necessary.** `G83-1` N1 stands.

**It does not license the next re-freeze.** The next rise must arrive with both
columns and the same enumeration — which is now printed by the failing run
itself rather than left to whoever hits it.

## 7. Nominations

* **N1** — remove the seven `java.util.function` default-method stubs in
  `native-builtins/src/phases_late/streams.rs`. No JDK declares them native;
  real bytecode returns the lambda; no state is involved. Contract §8 names
  `native-builtins/src/lib.rs` **specifically**, and this is a different file,
  so the clause does not reach it on a literal reading.

  **But be clear what it would and would not buy.** `SyntheticStub` is not
  `allowed_in(JdkOnly)`, so all seven are ALREADY dropped under `--jdk-only`;
  deleting them changes **compatible** mode only. It lowers the ratchet and
  shrinks wave 2's backlog by seven. It moves strict mode by exactly zero.
  That is a real benefit and it is not this project's benefit — do not let a
  falling stub count read as strict-mode progress. The same caution applies to
  every other row in §3b.
* **N2** — the six `java/lang/Runtime.exec` overloads: `native-io/src/process.rs`
  already carries 26 stub rows for the process surface. Whether `exec` needs any
  native at all is a question the census can ask (`real_declaring_method` on a
  loaded `java.lang.Runtime`) and nobody has.
* **N3** — apply the two-column rule to the **bridge** ratchet
  (`bridge-ratchet.sh`, this gate's sibling). It counts a running VM and has the
  same blind spot in the same direction: a `SyntheticStub` quietly becoming a
  `Bridge` lowers *that* number while changing nothing.
