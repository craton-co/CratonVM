# H3-1 — the ratchet that did not compile, seven stubs deleted, and a bridge gate that could not have a second baseline

**Status:** **FIXED-UNVERIFIED** — no binary carrying these changes has been
built or run. Lane H3 was forbidden to build; every "after" figure below is
labelled **PREDICTED** and carries what would falsify it. The only things
MEASURED here are source facts (`grep`, `git show`, `rustfmt --check`), JDK
image facts (`javap`/`src.zip` on the real JDK 25.0.3+9 image), and two Python
self-tests that were actually executed.

> **VERIFIED AGAINST A BINARY 2026-09-02.** This record's title defect — a
> ratchet that did not compile, taking the whole `--test stub_ratchet` binary
> and nine other tests with it — is FIXED. Both configurations were built and
> run, separately, as §"The commands" insists ("Run both, paste both — never
> derive one from the other"):
>
> ```text
> stub-ratchet [management (the shipping cratonvm-cli registry)]:
>     1645 SyntheticStub registrations out of 13897 total (baseline 1645, slack 0)
> stub-ratchet [no-management (ten jmx registrars short of shipping)]:
>     1634 SyntheticStub registrations out of 13529 total (baseline 1634, slack 0)
> ```
>
> Both compile, both run, both pass with **slack 0**. That is what the record
> owed, and it had said "no binary carrying these changes has been built or run"
> for **13 days**.
>
> **The −7 IS NOT VERIFIED, and cannot be from here.** §5's table predicts:
>
> ```text
>                                     before   predicted after   measured 2026-09-02
> BASELINE_SYNTHETIC_STUBS_MANAGEMENT    1622        1615               1645
> BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT 1611        1604               1634
> MEASURED_TOTAL_REGISTRATIONS_MGMT     13160       13153              13897
> ```
>
> Every "before" value in that table is gone from the tree, so the delta has no
> subtrahend left to measure against. Thirteen days of other lanes added
> registrations — **+744 total registrations** on the management arm — and the
> baselines were re-frozen somewhere in there by whoever did it.
>
> What survives the drift is a consistency check, and it passes: both arms moved
> by **exactly the same amount** (+23 against this table's "before", +30 against
> its "after"). A delta that differed between the two arms would be a finding;
> an equal one is the signature of shared registrars growing, which is the
> expected background.
>
> So: the compile defect is closed and measured. §5's arithmetic is **stale, not
> wrong** — nobody can now tell whether the seven rows came out as predicted,
> because the ratchet froze over them. That is the cost of leaving a re-freeze
> unverified for thirteen days, and it is worth more as a lesson than the seven
> rows were.

**Date** 2026-08-20
**Branch** lane H3 worktree off `claude/jdk-only-mode-handoff-09b48c`
**Subject** `native-builtins/src/phases_late/streams.rs`, `native-io/src/process.rs`,
`native-builtins/tests/stub_ratchet.rs`, `vm/tests/stub_ratchet.rs`,
`regression-suite/bridge-ratchet.sh`, `scripts/jdk-only-bridge-ratchet.py`,
`scripts/baselines/jdk-only-bridge-ratchet.json`
**Discharges** `G89-1` N1 (in full), `G89-1` N2 (as an *analysis*; the code it
names is in a §8-reserved file), `G89-1` N3 (in full)

---

## 0. READ THIS FIRST — none of this moves strict mode

`SyntheticStub` is **not** `allowed_in(JdkOnly)`
(`native-api/src/registry.rs`). Every one of the seven registrations deleted
below was **already dropped** under `--jdk-only`, and so are the six
`Runtime.exec` overloads §3 adjudicates. Deleting them:

* lowers the **compatible-mode** backlog by seven rows;
* fixes **compatible-mode** behaviour on a surface where compatible mode is
  measurably worse than strict (§2.3);
* **moves `--jdk-only` by exactly zero.**

`HANDOFF-20260819.md` §1 and `G89-1` N1 both say this in as many words. It is
repeated here because the stub count is the number a future reader will find
first, and a falling stub count reads like progress on this project's goal
when it is progress on a different one. **Do not quote §5's expected
`1622 → 1615` as `--jdk-only` movement.** If you want strict-mode movement,
retire `Bridge`-tagged shadows (`G90-1`), not stubs.

---

## 1. THE FINDING — the stub ratchet has not compiled since 2026-08-19

This is the highest-value output of the lane and it was found while reading the
file in order to re-baseline it.

`native-builtins/tests/stub_ratchet.rs` **does not parse** at `26e4b5db4`
(`Merge branch 'claude/jdk-only-mode-completion-1351c0' into dev`, the tip this
lane branched from).

```text
$ rustfmt --edition 2021 --check native-builtins/tests/stub_ratchet.rs
error: unknown start of token: \
    --> native-builtins/tests/stub_ratchet.rs:1159:88
     |
1159 |          {BASELINE_SYNTHETIC_STUBS}. A change added a NEW synthetic stub. Make the new \
```

The merge combined two versions of `synthetic_stub_count_does_not_regress`'s
failure message and left **both**:

| side | commit | what it had |
|---|---|---|
| A | `19e5d0228` | the new "FIRST, find out WHICH rows" message, ONE format arg (`synthetic + SLACK`) |
| B | `083998c7b` | the old message plus the by-file breakdown, TWO args (`synthetic + SLACK`, `breakdown`) |

The textual merge kept A's message head, then spliced B's message tail onto it
**after A's closing `",`**, and kept B's argument list. Line 1158 closes the
first string literal; line 1159 begins `{BASELINE_SYNTHETIC_STUBS}. A change
added a NEW synthetic stub. Make the new \` — a block expression followed by
free identifiers and a stray `\`. Not an expression, not compilable.

### Why this is worse than the gate being red

`G89-1` §1 makes the argument for a red ratchet: *"Once it is red, it says
nothing about the next change."* A ratchet that **does not compile** is that,
plus it takes the whole `--test stub_ratchet` binary with it — nine other tests
in the file, including `census_covers_more_than_the_essentials_registrar`
(the guard against the census scope being narrowed back) and
`no_registration_runs_on_the_ambient_default`. And this file is:

* **blocking in CI**, in both configurations, since `G89-1` §4
  (`.github/workflows/ci.yml:256`–`262`);
* the cited evidence for the P0 row *Residual synthetic native set*.

So a CI-blocking gate that a P0 row rests on has been a build error, and the
sequence — `G83-1` found it red, `G89-1` re-lit it, a merge silently broke it —
is the third act of the same play. `G83-1`'s own line applies verbatim: **an
evidence citation that nobody executes is a claim, not evidence.**

### Fixed, and the cheap check that finds the next one

Repaired in this change: one coherent message that keeps both sides' content
(A's two-cause triage, B's by-file breakdown and the CLASSIFY-IT paragraph) and
uses **inline format captures only — zero positional arguments**, with a
`let refreeze = synthetic + SLACK;` local to make that possible. A message with
no positional arguments cannot be mis-spliced this way and cannot drift out of
step with its argument count.

**`rustfmt --edition 2021 --check <file>` is a parser, costs a second, and
never writes.** It reproduced the diagnosis above and confirmed the repair. Run
over all **147** `.rs` files the merge touched: **no other file fails to
parse.** This was a single instance, not a systemic merge failure.

> This is not licence to run `cargo fmt` — this repository is not rustfmt-clean
> and formatting it is a standing prohibition. `--check` on ONE file, reading
> only the `^error` lines, is a parse check, not a format pass.

---

## 2. H3-A — the seven `java.util.function` stubs (`G89-1` N1): DELETED

### 2.1 The seven, by exact class + method + descriptor

All seven were in `native-builtins/src/phases_late/streams.rs`, inside
`register_phase56_function_extras`. Contract §8 names
`native-builtins/src/lib.rs` **specifically**; this is a different file, so §8
does not reach it.

| # | class | method | descriptor | was | minted |
|---|---|---|---|---|---|
| 1 | `java/util/function/Predicate` | `and` | `(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;` | `SyntheticStub` (scope) | `Predicate$$Lambda$And` |
| 2 | `java/util/function/Predicate` | `or` | `(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;` | `SyntheticStub` (scope) | `Predicate$$Lambda$Or` |
| 3 | `java/util/function/Predicate` | `negate` | `()Ljava/util/function/Predicate;` | `SyntheticStub` (scope) | `Predicate$$Lambda$Negate` |
| 4 | `java/util/function/Predicate` | `not` | `(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;` | `SyntheticStub` (scope) | `Predicate$$Lambda$Negate` |
| 5 | `java/util/function/Consumer` | `andThen` | `(Ljava/util/function/Consumer;)Ljava/util/function/Consumer;` | `SyntheticStub` (stated) | `Consumer$AndThen` |
| 6 | `java/util/function/BinaryOperator` | `maxBy` | `(Ljava/util/Comparator;)Ljava/util/function/BinaryOperator;` | `SyntheticStub` (stated) | `BinaryOperator$MaxBy` |
| 7 | `java/util/function/BinaryOperator` | `minBy` | `(Ljava/util/Comparator;)Ljava/util/function/BinaryOperator;` | `SyntheticStub` (stated) | `BinaryOperator$MinBy` |

`git diff -U0 | grep '^-'` over the file shows **exactly seven** removed
`r.register`/`r.register_with_kind` call sites. Nothing else was removed. Each
deletion left a tombstone naming what went, why, and what it does not buy.

### 2.2 Image evidence, per row

MEASURED with the real image (`javap -p` and `lib/src.zip` on
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`, 2026-08-20):

* `Predicate.and/or/negate` are `default`, `Predicate.not` is `static`; none is
  `ACC_NATIVE`; each has a real body (`and` is `(t) -> test(t) && other.test(t)`).
* `Consumer.andThen` is `default`, not `ACC_NATIVE`, returns
  `(T t) -> { accept(t); after.accept(t); }`.
* `BinaryOperator.maxBy/minBy` are `static`, not `ACC_NATIVE`, bodies
  `(a, b) -> comparator.compare(a, b) >= 0 ? a : b` (and the mirror).

So real bytecode serves all seven the moment nothing shadows them.

### 2.3 The reachability proof, and it is measured rather than argued

The instruction for this lane was *"a stub deleted where the real path does not
work is a regression that looks like progress"*. For these seven the real path
is **already running, today, on this binary**:

`G62-1` §1 measured five vectors that **PASS under `--jdk-only` and FAIL in
Compatible mode on the same binary**, and one of them is
`RJdkFunctionCombinators` — the vector whose entire job is asserting that these
combinators do **not** return a fabricated class (`notFabricated`, an exact
name equality against eleven fabricated names, plus computed values and
short-circuit behaviour). Under `--jdk-only` the seven registrations are
dropped at the door and the vector passes; in Compatible they register, mint,
and it fails.

**Deleting the registration puts Compatible mode into the configuration strict
mode is already measured green in.** That is a stronger warrant than any
reading of the JDK source, and it is the reason these seven were deletable
where H3-B's six were not.

### 2.4 Consumer sweep — what depends on the fabricated receivers

Greps run over the whole tree (`Predicate$$Lambda`, `Consumer$AndThen`,
`BinaryOperator$MaxBy|MinBy`, and `"and"|"or"|"negate"|"not"|"andThen"|"maxBy"|"minBy"`
in `*.rs`):

| consumer | verdict |
|---|---|
| Rust callers of the seven triples | **none** |
| Other registrations of the seven triples | **none** — the seven are uniquely registered here, so the delta really is −7 rather than "−7 minus whatever a later registrar re-adds" |
| Other mint sites for the six carrier classes | **none** — these seven were the only ones |
| `native-builtins/tests/registrar_drift.rs` | does **not** table `register_phase56_function_extras`; its `register_phase52_function_extras` table lists `BinaryOperator.apply` and no combinator. Unaffected |
| synthetic-jdk corpus (`vm/tests/resources/cratonvm/*.class`) | `javap -c` on `StreamComplete`, `JucComplete`, `SyntheticDiff`, `TckUtil`: every `java/util/function` reference is an `invokedynamic` or a `java/util/stream` interface call. **No call to any of the seven.** So the synthetic-jdk build, where these natives ARE the class library, has no consumer either |
| `classloading/src/class_manager.rs` (interface tables, `fabricated_origin_for_name`, and the test at `:19246`) | name classification only — never calls the mint sites. Unaffected |
| `native-api/src/no_image_receiver.rs` | `NO_IMAGE_JDK_RECEIVERS` re-tags natives BY RECEIVER; the receivers still exist as table entries and the `test`/`accept` natives on them are still registered. Inert, not broken |
| **`vm/src/runtime/interpreter/native_override.rs:3175`** | **a forced-native arm for exactly `Predicate.and/or/negate/not`.** See §2.5 — this is the one real landmine, it is in a file H3 does not own, and it is in §6 |

### 2.5 The landmine, and why it does not explode

`force_native_over_real_jdk_bytecode` names the four `Predicate` triples. `G89-1`
N1's one-line case for removal ("no JDK declares them native; real bytecode
returns the lambda; no state is involved") **does not mention this**, and a
registration deleted while a forced-native list still names it is the shape that
produces an `UnsatisfiedLinkError`.

Traced, all three consulting sites:

* `dispatch_virtual.rs:767` — `force_native_registered = force_native && <registry
  resolve returns Some>`. Registry re-checked. Falls back to bytecode.
* `dispatch_virtual.rs:3449` — `if force { if let Some(..) =
  resolve_cached_native_registration(..) }`. Registry re-checked. Falls back.
* `jit_bridge.rs:3380` — `let direct = force_native_over_real_jdk_bytecode(..) ||
  find(..).is_some();` — **no registry re-check**, but the value only feeds
  `jit_method_calls_native_shadowed`, a conservative predicate that SEALS a
  method out of the JIT. A stale `true` costs a missed tier-up, never
  correctness.

**Verdict: the arm is now inert, and deleting the registration cannot raise
`UnsatisfiedLinkError`.** It is nevertheless a lie about the tree and must go;
the exact patch is in §6. This is the `[1 site != retired]` shape — a stand-in
retired at one dispatch site is still named at the others.

### 2.6 What was deliberately NOT deleted, and why

`Predicate$$Lambda${And,Or,Negate}.test` and `Consumer$AndThen.accept` are still
registered a few lines below the tombstones. Nothing mints those classes any
more, so they are **dead, not wrong**. Removing them too would make the ratchet
delta something other than the exact −7 that §5's re-freeze is derived from, and
a re-baseline whose arithmetic a reader cannot reproduce is how the 31 of
`G89-1` §3b got in. Nominated in §7, not done here.

Also untouched: `Function.compose/andThen/identity`, `UnaryOperator.identity`,
`Function$Identity.apply`, `Comparator$Native`. They are the same family and the
same argument, but they were not `G89-1` N1's seven, two of them are registered
a *second* time in `native-builtins/src/lib.rs` (§8-reserved), and the fixed −7
is what makes §5 mechanical.

### 2.7 PREDICTED, with its falsifier

* **PREDICTED:** the stub ratchet falls by exactly 7 in both configurations, and
  the total registration count falls by exactly 7 as well — a *deletion*, not a
  relabel. **Falsified by** any other delta in either column; if that happens,
  `CRATONVM_RATCHET_ROWS=1` here and at `26e4b5db4` and diff the sorted
  `stub-ratchet(row):` lines before touching a constant.
* **PREDICTED:** `RJdkFunctionCombinators` in Compatible mode gets *further*
  than it does today. **It is NOT predicted to pass.** Its `FABRICATED` array
  has eleven names; this change removes the only mint site for seven of them
  (`Predicate$$Lambda$And/$Or/$Negate`, `Consumer$AndThen`,
  `BinaryOperator$MaxBy/$MinBy`) and leaves four alive
  (`Function$AndThen/$Compose/$Identity`, `UnaryOperator$Identity`, plus
  `Comparator$Native` from `native-collections`). **Falsified by** a Compatible
  failure whose message names one of the seven removed names.
* **PREDICTED:** `--jdk-only` is byte-for-byte unchanged. **Falsified by** any
  movement in the 102-vector `CRATONVM_ARGS=--jdk-only` arm. This is the
  prediction most worth checking, because it is the one that says the lane
  was, for strict mode, a no-op.

---

## 3. H3-B — the six `Runtime.exec` overloads (`G89-1` N2)

### 3.1 The record and this lane's own brief are wrong about where they live

**`native-io/src/process.rs` does not register `java/lang/Runtime.exec`. It
never did.** `grep -n 'java/lang/Runtime' native-io/src/process.rs` returns
three hits, all of them `java/lang/RuntimeException` inside an error path.

All six overloads are registered in **`native-builtins/src/lib.rs`**, at the six
`registry.register_with_kind(` call sites on lines
**14651, 14658, 14665, 14672, 14679, 14686** — the ONE file contract §8 names
verbatim as off limits:

> *"Do not edit `native-builtins/src/lib.rs`; the stub reclassification is a
> separate wave with its own subsystem-per-PR discipline."*

`G89-1` **§3b is correct** (`+8 native-builtins/src/lib.rs — java/lang/Runtime.exec x6`).
Only `G89-1` **§7 N2's prose** points at `native-io/src/process.rs`, and this
lane's brief inherited it. The "26 stub rows" in N2 are real and they are in
`process.rs` — 13 on `cratonvm/synthetic/Process`, 1 `ProcessExitWaiter.run`, 5
on the synthetic pipe output stream, 6 on the synthetic pipe input stream, and
`java/lang/ProcessBuilder.start()Ljava/lang/Process;` = **26 exactly**. The
count matched, the contents did not, and that coincidence is what made the
mis-location survive.

**So no `exec` overload was deleted. Deleting one would have violated §8.**

### 3.2 The analysis N2 asks for, done anyway — and it was already answered

N2 says *"whether `exec` needs any native at all is a question … nobody has
[asked]"*. That is also wrong, and the answer is in the registration site's own
comment, dated **2026-08-12** (W7-17 N1), with a measured two-arm probe. This
lane re-derived it independently from the image rather than trusting the
comment.

MEASURED, `javap -p java.lang.Runtime` on JDK 25.0.3+9:

```text
  public java.lang.Process exec(java.lang.String)
  public java.lang.Process exec(java.lang.String, java.lang.String[])
  public java.lang.Process exec(java.lang.String, java.lang.String[], java.io.File)
  public java.lang.Process exec(java.lang.String[])
  public java.lang.Process exec(java.lang.String[], java.lang.String[])
  public java.lang.Process exec(java.lang.String[], java.lang.String[], java.io.File)
  public native int availableProcessors();
  public native long freeMemory();
  public native long totalMemory();
  public native long maxMemory();
  public native void gc();
```

**Not one `exec` overload is `ACC_NATIVE`.** The only natives on the class are
the five memory/CPU accessors. From `src.zip`
(`java.base/java/lang/Runtime.java`), the delegation chain is:

| # | descriptor | `ACC_NATIVE`? | delegates to |
|---|---|---|---|
| 1 | `(Ljava/lang/String;)Ljava/lang/Process;` | no | `exec(command, null, null)` |
| 2 | `(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/Process;` | no | `exec(command, envp, null)` |
| 3 | `(Ljava/lang/String;[Ljava/lang/String;Ljava/io/File;)Ljava/lang/Process;` | no | `StringTokenizer` split, then #6 |
| 4 | `([Ljava/lang/String;)Ljava/lang/Process;` | no | `exec(cmdarray, null, null)` |
| 5 | `([Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/Process;` | no | `exec(cmdarray, envp, null)` |
| 6 | `([Ljava/lang/String;[Ljava/lang/String;Ljava/io/File;)Ljava/lang/Process;` | no | `new ProcessBuilder(cmdarray).environment(envp).directory(dir).start()` |

The platform-native leaf is far below all six: `ProcessBuilder.start()` →
`ProcessImpl` → `ProcessImpl.create(...)` on Windows,
`ProcessImpl.forkAndExec(...)` on Linux. **Both leaves are registered as
genuine `Bridge`s by `register_process_natives` in `native-io/src/process.rs`**
(the Windows ten under `#[cfg(windows)]`, `forkAndExec` under
`#[cfg(target_os = "linux")]`), so the real bytecode path is served.

### 3.3 Per-overload verdict

**All six: KEEP AS A STUB, WITH A STATED REASON — and the reason is that they
are already `SyntheticStub`, already dropped under `--jdk-only`, and the file
they live in is §8-reserved.**

The residual is a *Compatible*-mode deletion, not a strict-mode one, and it is
wave-2 work by contract. What would break if the real path were not reachable —
the answer required by this lane's brief — is
`NoClassDefFoundError: cratonvm/synthetic/Process`, which is precisely the
symptom the 2026-08-12 probe recorded for `Runtime.exec(String[])` **before**
these six were tagged `SyntheticStub`, when `ProcessBuilder.start` had been
pinned and the older spawn API had not. That is a measured negative control for
the whole family: pinning one caller of a shared mint without enumerating the
others is what it looks like when it goes wrong.

`native-io/src/process.rs` now carries a block comment recording all of the
above at the registrar, so the next reader sent here by N2 finds the correction
where they land.

**Net H3-B delta to the stub ratchet: 0.**

---

## 4. H3-C — the two-column rule applied to the bridge ratchet (`G89-1` N3)

### 4.1 What the stub ratchet does, mirrored

| stub ratchet (`native-builtins/tests/stub_ratchet.rs`) | bridge ratchet (this change) |
|---|---|
| prints stub count **and** total registration count | prints every ratchet count **and** `registry rows: N (baseline M, delta ±d) by kind {...}` |
| `MEASURED_TOTAL_REGISTRATIONS_*` recorded, **not asserted** | row column read from the baseline's `observed` block, **not asserted** — a new genuine `Bridge` legitimately raises the total |
| failure message classifies relabel vs new fake | `_classify_row_delta()`, total over all four cases (up / down / unchanged-with-kind-movement / unchanged) |
| `synthetic_by_file()` printed every run | `render_by_file()` printed every run, keyed by `registered_by`'s file half, same granularity for the same reason |
| `CRATONVM_RATCHET_ROWS=1` row dump | `CRATONVM_RATCHET_ROWS=1` → `@@BRIDGEROW` lines (the ratchet population by name); `=all` → `@@ROW` lines (every registration **with its kind**, which is what a suspected relabel actually needs) |

The `=all` mode is the one addition beyond a straight mirror, and it is the
direction `G89-1` N3 names: *"a `SyntheticStub` quietly becoming a `Bridge`
lowers the stub number and raises the bridge number while changing nothing
real."* Two `=1` dumps cannot see that row at all; two `=all` dumps show it
changing kind.

A guard against the new views drifting from the number they describe:
`_adjudicated()` is the single predicate form of `without_acc_native`, and
`selftest` asserts `len(unadjudicated_rows(doc)) == by_file total ==
bridge.without_acc_native` on four discriminating census shapes (clean,
inherited-`ACC_NATIVE`, inherited-through-`java.lang.Object`, absent class). A
breakdown that names a different set from the number above it is worse than no
breakdown.

### 4.2 The scope defect — found, and it is not the one in CI

`G89-1` §4's defect was CI running one of two frozen configurations. **CI is
clean here**: `ci.yml` runs `bridge-ratchet.sh` blocking on ubuntu only, and
says why in its own comment (baselines are keyed `<feature>/<os>`, only
`25/linux` is committed, other legs would correctly refuse with exit 2), and
the advisory `jdk-only` matrix job reports refusals as `::notice::` with the
words *"This leg is not gated; that is a gap, not a pass."* That is the
opposite of the defect. Recorded as a negative result because a reader chasing
`G89-1` §4's shape here will otherwise re-derive it.

**The defect is one level down, in the baseline key.** The gate takes exactly
one census, `--real-jdk` (compatible). Two of its five ratchets make claims
about the *other* mode:

* `bridge.shadows_bytecode_anywhere` is contract **§1.4**, a `--jdk-only` rule;
* `superseded.stub_lost_to_admitted` says *"admitted under `--jdk-only`"* **in
  its own name**.

The strict registry ships and was measured by nothing. And it **could not have
been**: the key was `<jdk-feature>/<os>` with the mode recorded only *inside*
the entry, guarded by `entry["mode"] != block["mode"] → refuse`. So the two
modes shared one slot — `--update-baseline` on a strict census would have
**overwritten** the compatible baseline, after which every compatible run
refused. There was no way to give strict a baseline at all.

Fixed:

* `baseline_key()` is now `<feature>/<os>` for `compatible` (unchanged — every
  committed baseline still scores, on purpose) and `<feature>/<os>/<mode>`
  otherwise;
* `bridge-ratchet.sh` takes a **second census** under `--jdk-only` and scores it
  with the same gate. Gate 2 (the kind map) is deliberately not run on it: its
  committed TSVs are compatible-mode and it has no mode key;
* the strict leg is **REPORTED, NOT BLOCKING** until a strict baseline is
  committed. With no `25/<os>/jdk-only` entry the gate refuses (exit 2), the
  script prints *"A refusal is not a pass"*, and only exit 1 fails. It has
  never been run in CI, so its first landing must not be able to turn a build
  red for a reason nobody has diagnosed. Promoting it is deleting one branch;
* `BRIDGE_RATCHET_STRICT=0` skips the leg.

**No fabricated strict baseline was written.** That is the one thing that would
be worse than the gap.

### 4.3 What was actually executed

Two things in this section are MEASURED rather than predicted, because they need
no VM:

```text
$ python scripts/jdk-only-bridge-ratchet.py --selftest
selftest: 34 passed, 0 failed          (28 at HEAD; +6, all new)

$ sh regression-suite/bridge-ratchet.sh --selftest
bridge-ratchet self-test  34 passed, 0 failed
kind-map self-test        13 passed, 0 failed
$ sh -n regression-suite/bridge-ratchet.sh   # syntax OK
$ python -c "import json; json.load(open('scripts/baselines/jdk-only-bridge-ratchet.json'))"   # OK
```

The four new self-test checks are the ones that could be wrong by inspection: a
strict census with no strict baseline refuses; a strict census with a strict
baseline scores; the compatible entry is untouched by the strict one existing;
and an entry recording the wrong mode under the legacy key is still refused.
Plus three structural checks (the three views are one population; the dumps
survive null fields; the classifier answers every combination).

The full print path was additionally smoke-run against a synthetic census file
to exercise `render_by_file`, both row dumps and the row column end to end.

### 4.4 What this does NOT do

* It does **not** measure the strict bridge population. It makes measuring it
  possible and makes the absence loud. Until somebody runs
  `--update-baseline`, the strict leg is a printed notice.
* It does **not** re-freeze the compatible baseline, which has been marked
  `PENDING RE-FREEZE, NOT APPLIED` since 2026-08-12 and is 182+ commits stale.
  The new row column will report a large delta against it. That is the entry
  being stale, not the column being wrong.
* It does **not** assert the row count. A ratchet there would fire on correct
  work.

---

## 5. REBASELINE REQUIRED

**Nothing in this section has been measured. Every "expected" figure is
arithmetic from a source diff.** The constants are deliberately left at their
old values, marked `H3-1 REBASELINE REQUIRED` in the source. The assert is
`<=`, so leaving them high **passes** and prints the exact constant to paste —
the safe direction. A hand-written value that lands too HIGH silently re-admits
that many new stubs, and this file's own history has a constant derived by
arithmetic sitting six above the truth for a week.

| file | constant | old | expected Δ | expected new | status |
|---|---|---:|---:|---:|---|
| `native-builtins/tests/stub_ratchet.rs` | `BASELINE_SYNTHETIC_STUBS_MANAGEMENT` | 1622 | **−7** | 1615 | **NOT MEASURED** |
| `native-builtins/tests/stub_ratchet.rs` | `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` | 1611 | **−7** | 1604 | **NOT MEASURED** |
| `native-builtins/tests/stub_ratchet.rs` | `MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT` | 13160 | **−7** | 13153 | **NOT MEASURED** |
| `native-builtins/tests/stub_ratchet.rs` | `MEASURED_TOTAL_REGISTRATIONS_NO_MANAGEMENT` | 12792 | **−7** | 12785 | **NOT MEASURED** |
| `scripts/baselines/jdk-only-bridge-ratchet.json` | `25/linux` (all five ratchets + `observed`) | — | Bridge counts **unchanged**; `observed.total_rows` and `registrations["synthetic-stub"]` **−7** | — | already `PENDING RE-FREEZE` since 2026-08-12; H3-1 adds nothing new to fix |
| `scripts/baselines/jdk-only-bridge-ratchet.json` | `25/<os>/jdk-only` | **absent** | — | — | must be TAKEN, never written by hand |

`−7` and not `−13`: H3-A deletes seven rows; **H3-B deletes none** (§3).

### The commands

Both configurations. Run both, paste both — never derive one from the other:

```bash
cargo test -p cratonvm-native-builtins --features management \
    --test stub_ratchet synthetic_stub_count_does_not_regress -- --nocapture
cargo test -p cratonvm-native-builtins \
    --test stub_ratchet synthetic_stub_count_does_not_regress -- --nocapture
```

Each prints, verbatim, the line to paste:

```text
stub-ratchet: const BASELINE_SYNTHETIC_STUBS_MANAGEMENT: usize = <n>;
```

and `... out of {total} total`, which is the `MEASURED_TOTAL_REGISTRATIONS_*`
value. **Any delta other than −7 in either column is a finding to attribute
before re-freezing** — `CRATONVM_RATCHET_ROWS=1` at this commit and at
`26e4b5db4`, diff the sorted `stub-ratchet(row):` lines.

For the bridge ratchet, on a Linux host with a real JDK 25 image:

```bash
CV=<cratonvm> JAVA_HOME=<jdk25> sh regression-suite/bridge-ratchet.sh \
    --update-baseline --note "H3-1: re-freeze after the merge repair; first strict baseline"
```

That one invocation now freezes **both** `25/linux` and `25/linux/jdk-only`
from the same run.

---

## 6. OUT-OF-FILE EDITS REQUIRED

### 6.1 `vm/src/runtime/interpreter/native_override.rs` — delete the dead `Predicate` arm

Not owned by lane H3. **Required for correctness of the record, not of the
build**: the arm is inert after §2.5, but it names four triples that no longer
have a registration.

**File:** `vm/src/runtime/interpreter/native_override.rs`
**Lines:** 3175–3192 (inside `force_native_over_real_jdk_bytecode`)

Current text, exactly:

```rust
    if class_name == "java/util/function/Predicate"
        && matches!(
            (method_name, method_descriptor),
            (
                "and",
                "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;"
            ) | (
                "or",
                "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;"
            ) | ("negate", "()Ljava/util/function/Predicate;")
                | (
                    "not",
                    "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;"
                )
        )
    {
        return true;
    }
```

Replacement text, exactly:

```rust
    // The `java/util/function/Predicate` arm — `and`/`or`/`negate`/`not` — IS
    // GONE (H3-1, 2026-08-20), together with the four registrations it forced.
    // Those were `SyntheticStub` mint sites in
    // `native-builtins/src/phases_late/streams.rs` and were deleted; real
    // `java.base` bytecode serves all four in every mode now.
    //
    // The arm was already inert rather than dangerous, and that was traced
    // before the deletion landed rather than assumed: `dispatch_virtual.rs:767`
    // and `:3449` both re-check the registry (`force_native && resolve(..)`,
    // `if force { if let Some(..) = resolve_cached_native_registration(..) }`),
    // so a forced triple with no registration falls back to bytecode; and
    // `jit_bridge.rs:3380` uses the result only to SEAL a method out of the
    // JIT, where a stale `true` costs a missed tier-up and never correctness.
    // Left in place it would have been a claim about a registration that does
    // not exist, which is what this file's `java/lang/String` tombstone above
    // records the cost of.
```

Note: the preceding `java/util/Collections.emptyList` arm's own comment argues
that removing a forced-native entry is a PAIR needing a
`RETIRED_SHADOW_TRIPLES` entry. **That does not apply here.** That pairing is
for a live `Bridge` being handed back to bytecode; `RETIRED_SHADOW_TRIPLES`
re-tags `Bridge → SyntheticStub`. These four were already `SyntheticStub` and
are now not registered at all, so there is nothing to re-tag and no table entry
is needed. Verified by reading `native-api/src/retired_shadow.rs`'s header.

### 6.2 Nothing else is required

`native-builtins/tests/registrar_drift.rs`, `native-api/src/no_image_receiver.rs`,
`classloading/src/class_manager.rs`, `scripts/baselines/jdk-only-kind-map-25-linux.tsv`
and `scripts/baselines/jdk-only-dead-everywhere-GATED.tsv` were all checked and
none needs a change (§2.4). The `kind-map` TSV **does** carry rows for the seven
deleted triples (lines 8183–8185, 8211–8214), so
`scripts/jdk-only-kind-map.py` will report seven **REMOVED** registrations —
which its own self-test pins as a PASS (`"a REMOVED registration passes"`). No
edit; expect the report.

---

## 7. NOMINATIONS

* **N1 — run the two commands in §5 and commit the measured constants.** This
  is the orchestrator's, not a future lane's: until it happens the ratchet
  carries seven rows of slack, which is exactly seven new stubs it will not
  notice. Highest priority item in this record.
* **N2 — take the first `--jdk-only` bridge-ratchet baseline** (§4.2). One
  `--update-baseline` run on a Linux host now freezes both modes. Until then
  the strict registry is unmeasured and the script says so on every run. This
  is the item that turns §4 from a capability into a measurement.
* **N3 — `rustfmt --edition 2021 --check` every `.rs` file a merge touched,
  as a merge-hygiene step.** It costs seconds, it needs no build, and it is the
  only thing that caught §1. Consider a CI step: a message-heavy `assert!` is
  the shape that merges badly, and this repository has a great many of them.
  The scan over the 147 files `26e4b5db4` touched found exactly one break, so
  the false-positive rate on a non-rustfmt-clean tree is zero when you read
  only the `^error` lines.
* **N4 — delete the four now-dead carrier natives** in
  `native-builtins/src/phases_late/streams.rs`
  (`Predicate$$Lambda${And,Or,Negate}.test(Ljava/lang/Object;)Z`,
  `Consumer$AndThen.accept(Ljava/lang/Object;)V`) once §5's re-freeze has
  landed with a measured number. Held back here only so the −7 stays derivable
  (§2.6). They are provably unreachable: their sole mint sites are the
  tombstones this change left.
* **N5 — the same treatment for the rest of the family**, which `G89-1` N1 did
  not enumerate: `Function.compose/andThen/identity`,
  `UnaryOperator.identity`, `Function$Identity.apply`, and
  `Comparator$Native`. `RJdkFunctionCombinators` asserts against all of them
  and will keep failing in Compatible mode until they go. Two of them are
  registered a **second** time in `native-builtins/src/lib.rs`, so this one is
  §8-blocked and belongs to wave 2 — which is also the answer to *why*
  Compatible mode cannot be made to match strict on this vector in wave 1.
* **N6 — `G89-1` §7 N2's prose needs correcting** to say
  `native-builtins/src/lib.rs` (§3.1). `G89-1` §3b already says it correctly, so
  the record disagrees with itself; a reader who starts at §7, as this lane's
  brief did, is sent to the wrong file and finds 26 stub rows there that make
  the mis-location look confirmed.
* **N7 — `HANDOFF-20260819.md`'s JDK path is wrong on this host** (§8).

---

## 8. Where this record disagrees with the tree or with another record

Backed by a grep or a command in every case.

1. **`native-builtins/tests/stub_ratchet.rs` does not compile at `26e4b5db4`.**
   Nothing in this directory says so; `G89-1` §5 records the file "green (10
   passed, 1 ignored…)", which was true at `820d162da` and stopped being true
   at the merge. `rustfmt --edition 2021 --check` reproduces it. Repaired here.
2. **`G89-1` §7 N2 puts the `Runtime.exec` overloads in
   `native-io/src/process.rs`.** They are in `native-builtins/src/lib.rs`
   (§3.1). `grep -n 'java/lang/Runtime' native-io/src/process.rs` → three
   `RuntimeException` hits and nothing else. §3b of the same record is right.
3. **`G89-1` N2 says whether `exec` needs a native is a question nobody has
   asked.** It was asked and answered on 2026-08-12 at the registration site,
   with a probe, and that is why all six are `register_with_kind(...,
   SyntheticStub)` today (§3.2).
4. **`G89-1` N1's case for deleting the seven omits
   `force_native_over_real_jdk_bytecode`.** Four of the seven are additionally
   named by a forced-native arm in the interpreter (§2.5). The omission is not
   fatal — the arm turns out to be inert — but "no JDK declares them native;
   real bytecode returns the lambda; no state is involved" is not, by itself,
   sufficient grounds to delete a registration in this VM.
5. **`HANDOFF-20260819.md`'s JDK path does not exist on this host.** It says
   `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`;
   `ls` → *No such file or directory*. The real image is
   `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`. The `src.zip` advice in
   the same paragraph is correct and works against the Microsoft path.
6. **The bridge ratchet's baseline key could not hold two modes**, while two of
   its five ratchets are named after the mode it cannot hold (§4.2). Neither
   `bridge-ratchet.sh`'s header nor `jdk-only-bridge-ratchet.py`'s docstring
   noticed, and both discuss the gate's blind spots at length.
7. **CI does *not* have `G89-1` §4's scope defect for the bridge ratchet**
   (§4.2). Stated as a negative result so the next reader does not spend an
   hour re-deriving it from the shape.

---

## 9. Files touched

| file | what |
|---|---|
| `native-builtins/src/phases_late/streams.rs` | seven registrations deleted, three tombstones |
| `native-io/src/process.rs` | block comment at `register_process_natives`: the 26 rows, where `Runtime.exec` really is, and the per-overload verdict |
| `native-builtins/tests/stub_ratchet.rs` | **parse error repaired**; four `H3-1 REBASELINE REQUIRED` markers; a third classification case (total DOWN = deletion) |
| `vm/tests/stub_ratchet.rs` | module-doc note: the sibling did not compile, and its four constants need a measured re-freeze |
| `regression-suite/bridge-ratchet.sh` | SCOPE header; `--jdk-only` census leg (reported, non-blocking); `BRIDGE_RATCHET_STRICT`, `CRATONVM_RATCHET_ROWS` documented |
| `scripts/jdk-only-bridge-ratchet.py` | two-column rule; per-file breakdown; `CRATONVM_RATCHET_ROWS=1`/`all`; mode-qualified baseline key; 4 new self-tests + 3 structural ones |
| `scripts/baselines/jdk-only-bridge-ratchet.json` | note only — **no number changed, no strict entry fabricated** |
| `docs/known-issues/jdk-only/H3-1-…-20260820.md` | this record |
