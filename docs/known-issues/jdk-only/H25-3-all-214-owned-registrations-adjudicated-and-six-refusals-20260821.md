# H25-3 — all 214 registrations in this lane's owned files adjudicated, **zero retired**, and six refusals with the evidence for each

**Status: OPEN — MEASURED adjudication, ARGUED refusals.** One
`--dump-native-registry --explain-jdk-only --jdk-only` dump from the prebuilt
`C:/craton/cratonvm-r8.exe` (`025780ff7`), `javap` against Microsoft JDK
25.0.3.9-hotspot, and reading of the tree. **No source change; no build; no
suite run.** Lane H25, 2026-08-21. Third of three; see `H25-1` and `H25-2`.

This lane was pointed at `H14-2` §4's largest unowned block — **`java.lang` core,
168 rows, claimed by no P0/P1/P2 row** — with write access to
`native-builtins/src/{lang_class,lang_string,deprecated_lang}.rs`. It applied
`H14-1` §4's verbs to every registration those files make and **retired
nothing.** This record is why, per row.

---

## 1. The owned surface, measured

The join is `registered_by` → the enclosing **top-level** `fn` (`--fn-indent 0`;
`H14-1` §2a is why that qualifier is load-bearing). Over the strict-mode dump,
the three owned files make **214 registrations** from **four** registrars:

| registrations | registrar |
|---:|---|
| **192** | `lang_string.rs::register_string_builder_natives` |
| 10 | `lang_class.rs::i2_register_classloader_package_natives` |
| 10 | `deprecated_lang.rs::register_deprecated_lang_natives` |
| 2 | `lang_string.rs::register_string_utf16_natives` |

### 1.1 Adjudication of all 214

| registrar | retire (real bytecode) | inherited | dead (`H25-1`) | no impl (`H25-2`) |
|---|---:|---:|---:|---:|
| `register_string_builder_natives` | 179 | 9 | 3 | **1** |
| `i2_register_classloader_package_natives` | 9 | **1** | 0 | 0 |
| `register_deprecated_lang_natives` | 4 | 0 | **6** | 0 |
| `register_string_utf16_natives` | 1 | 0 | **1** | 0 |
| **total** | **193** | **10** | **10** | **1** |

All 214 are `kind_stated: false` — consistent with `H14-1` §3.5's finding that
the tag was never the mechanism. **No exception in this lane's files.**

### 1.2 A correction to this lane's own brief

The task statement says, of the `StringBuilder` catastrophe:

> `StringBuilder`/`StringBuffer` are NOT in your owned files; treat this as the
> cautionary case, not a task.

**They are.** `register_string_builder_natives` is at
`native-builtins/src/lang_string.rs:158`, an owned file, and it is **192 of this
lane's 214 registrations — 90%**. Its signature is
`fn register_string_builder_natives(registry: &mut NativeMethodRegistry, class: &str)`
— it is **parameterised by class and called three times**, which is why the 192
divides exactly:

```
java/lang/AbstractStringBuilder   64
java/lang/StringBuffer            64
java/lang/StringBuilder           64
```

The instruction's *intent* is unambiguous and was followed to the letter: the
registrar was not touched. But the fact matters, because it means **the single
most destructive registrar known to the effort sits inside this lane's write
surface**, where `H14-3` measured it at *zero vectors* and the task statement
records what arming it actually does — every `append` silently discarded,
`toString()` returning empty, `rc=0`, no exception. A lane that had trusted the
brief's premise instead of the dump would have believed it could not reach that
code, and it can.

## 2. The six refusals

### R1 — `java/lang/Thread.stop()V`: retiring the winner PROMOTES a worse body

The exact `H22` shape, reproduced in a new file with a new pair. MEASURED from
the dump:

```
stop  ()V  site lib.rs:14080             owns_slot=False  overwrote=None
stop  ()V  site deprecated_lang.rs:375   owns_slot=True   overwrote=bridge
```

What each body does, read from source:

| registration | body |
|---|---|
| **winner**, `deprecated_lang.rs:375` `native_thread_stop` | gated on `ALLOW_THREAD_STOP` (default **false**) → throws `UnsupportedOperationException("Thread.stop() is not supported")` |
| **loser**, `lib.rs:14080` (inline closure) | `ctx.thread_interrupt(target); Ok(None)` — **returns normally, silently interrupting the target thread** |

And what the real image does (`javap -c java.lang.Thread`):

```
public final void stop();
  0: new  #369  // class java/lang/UnsupportedOperationException
  3: dup
  4: invokespecial #371
  7: athrow
```

So the three candidate behaviours are: **real bytecode** throws `UOE`;
**the winner** throws `UOE` with a message; **the loser** interrupts and returns.

**Deleting `deprecated_lang.rs:375` does not expose the bytecode. It exposes
`lib.rs:14080`** — and turns a method that correctly refuses into one that
silently interrupts a thread. The census would fall by one and the change would
score as a win.

**REFUSED.** The correct action is to delete **both** registrations in one
commit, which yields the real `UOE` and is a message-fidelity *improvement*.
`lib.rs` is not this lane's file, so the paired deletion cannot be made here.
Nominated as N1.

`Thread.stop0` is the same pair (`lib.rs:14063` loses,
`deprecated_lang.rs:369` wins) and is moot for behaviour — JDK 25 declares no
`stop0` at all (`H25-1`) — but it is the same trap and is listed for the same
reason.

### R2 — the six dead `deprecated_lang.rs` rows are pinned by tests in two crates, neither owned

`H25-1` shows six of this registrar's ten registrations name methods JDK 25
declares nowhere, confirmed by `javap -p java.lang.Thread`, which returns
**only** `public final void stop();` — no `stop0`, `suspend0`, `resume0`,
`destroy` or `countStackFrames` — and by `javap -p java.lang.System`, where
`runFinalizersOnExit` is absent. They are provably never dispatched.

They are also **asserted to exist by two separate test files**:

* `native-builtins/src/deprecated_verify.rs:257`
  `t85_1_all_deprecated_apis_registered` walks `deprecated_api_manifest()` —
  which lists all six at `:33`–`:41` — and asserts none is missing.
* `vm/tests/t8_deprecated_conformance.rs:29,31,39,40,47,55,87` asserts each of
  the six is `.find(...).is_some()`.

Neither file is owned by this lane, and they are in **two different crates**. A
retirement in `deprecated_lang.rs` alone turns both red.

**REFUSED.** Note also the near-miss avoided: `deprecated_verify.rs:241`
`register_missing_deprecated_shims` re-registers any checklist entry that is
missing, which would have made the retirement *inert* rather than merely
red — but `grep -rn` shows its only two call sites are at `:486` and `:496`,
**both inside `#[cfg(test)] mod tests`**, so it does not run on the boot path.
That was checked rather than assumed; asserting it either way without the grep
would have been wrong.

### R3 — `java/lang/StringUTF16.isBigEndian()Z` is deliberate, and the tree says so

The one row in this lane's files that looked like a free retirement. It is not.
`lang_string.rs:12422` carries a 56-line comment whose heading is *"On JDK 25
this registration never fires, and that is not a defect"*, quotes the identical
census row this lane re-derived, and concludes:

> So this stays registered for images that DO declare the method (JDK 17/21),
> where it must give the same answer `UnsafeConstants` gives, which it does.

**REFUSED**, and it is the witness behind `H25-1` §1.6: "declared nowhere" is a
one-image property, this host has only JDK 25 installed, and the multi-image
sweep that would separate deliberate from dead could not be run here.

### R4 — `i2_register_classloader_package_natives`: the corpus cannot see any of its ten methods, so a dial zero would mean nothing

Ten registrations, **all `owns_slot: true`**, nine *retire*, one *inherited* —
the most retirable-looking block in this lane's files by the standard rule. Each
one has a specific, named, application-level reason recorded at the site:

| rows | reason recorded in `lang_class.rs:19447`+ |
|---:|---|
| 5 | ByteBuddy + Mockito cannot complete `JavaDispatcher.<clinit>` — NPE on the never-initialised private `packages` field of user-instantiated `ClassLoader` subclasses |
| 2 | WildFly 39 boot — `getPackages()`'s stream pipeline leaks a `ReferencePipeline$Head` typed as `Package[]`, NPE on `arraylength` in `org/jboss/modules/ConcurrentClassLoader.<clinit>` |
| 1 | `Package.equals` — CratonVM allocates a **fresh** synthetic `Package` per `Class.getPackage()` where HotSpot interns one per (loader, name), so identity `equals` is always false and Spring's `MvcParamPredicate.hasMvcAnnotation` misclassified annotations by declaring package |
| 1 | `Package.hashCode` — keeps the equals/hashCode contract with the above |
| 1 | `checkCerts` — the JDK reads `package2certs`, also null on user-instantiated subclasses, NPE in `preDefineClass` |

**The decisive measurement is that the corpus cannot price any of this.**
`grep` over all 107 vectors in `regression-suite/src`:

* **no vector calls `getDefinedPackage`, `getPackages()` or `getPackage()`** —
  zero hits;
* the only mentions of Mockito / ByteBuddy / WildFly / JBoss / Spring across the
  corpus are **comments and class-name string literals** (e.g.
  `RJdkStrict.java:46-50` lists `"org.jboss.modules.Module"` as a *string*), not
  execution.

So arming `java/lang/ClassLoader` + `java/lang/Package` would report **zero
failures**, and that zero would carry no information whatever. It is the same
trap the task statement names for `StringBuilder` — *a zero over a corpus that
does not check the value is not evidence the value is right* — and `reach≠defect`
is the standing note: a narrow probe reports its own reach, not the defect.

**REFUSED.** The arm was not run, both because the finding does not depend on it
and because another lane's `cratonvm.exe` was resident throughout this session
(§4). **`H17-2`, merged into this branch after these records were drafted, adds
a second and independent reason not to trust such an arm:** the dial is wired to
**one** dispatch door, so a zero from it is silent about every call that does not
reach step 1 cold. For this block the two failures compound — an empty corpus
measured through a partial instrument.

`Package.equals` additionally carries the correction in `H25-2` §3.3: it is
`H14-1`'s *inherited* verdict, for which the standing prescription is *relocate
the registration to the class that declares the method*. Here that prescription
would delete the fix. It is a **deliberate override of an inherited method**.

### R5 — `java/lang/AbstractStringBuilder.toString()` is a second `Path.toString()`

MEASURED from the dump: `lang_string.rs:306`, `owns_slot: true`, image verdict
*declared, no `Code`, not `ACC_NATIVE`*. Confirmed against the image:

```
$ javap -p java.lang.AbstractStringBuilder
abstract class java.lang.AbstractStringBuilder implements Appendable, CharSequence {
  public abstract java.lang.String toString();
```

**`public abstract`, no `Code`.** The registration is the only implementation
for that receiver — precisely `H14-1` §4's *"a retirement of those two removes
the only implementation there is"*, in a third place, inside this lane's files.
**REFUSED**, and it is one data point behind `H25-2`'s 1,405.

### R6 — the `appendCodePoint` duplicate is real, and the tree has already decided to keep it

`register_string_builder_natives` registers
`appendCodePoint(I)L<class>;` **twice, 113 lines apart, in one function**, for
each of the three classes — six registrations, three slots:

```
lang_string.rs:215  -> native_sb_append_codepoint    owns_slot=False
lang_string.rs:328  -> native_sb_append_code_point   owns_slot=True
```

Two functions whose names differ by a single underscore. This is `H14-1` N4's
shape exactly (*"24 of the 162 are two registrations in ONE file"*) and `H5-1`'s
mechanism. It is also **already documented**, at `lang_string.rs:3112`, which
records that the truncating copy once owned the slot and the correct expansion
never ran, that the winner is now a delegation to the loser's body, and:

> the duplicate registration is left in place because removing it would move a
> census count for no behavioural gain.

**REFUSED** — deleting the loser is behaviourally inert (the winner delegates to
the loser's function), it is inside the registrar the brief forbids touching,
and the tree holds an explicit decision to retain it. Nominated as N4, because
the stated reason — *"would move a census count for no behavioural gain"* — is
an argument **against** the goal of an effort whose success metric is that
census.

## 3. Why zero retirements is the right outcome here

The task's own ordering — *"a refusal with evidence is worth more than a
retirement without it"* — and `H22`'s best result (refusing two of five) both
point the same way. Restated as a count: of 214 owned registrations,

* **192 (90%)** are in the registrar the brief forbids touching, which is also
  the one measured to be catastrophic when suppressed;
* **10** are pinned by assertions in two unowned files in two crates (R2), or
  are the double-registration trap (R1);
* **10** are a block whose entire justification lies outside the corpus, so no
  arm this lane could run would price them (R4);
* **1** is a second `Path.toString()` (R5);
* **1** is documented as deliberately cross-version (R3).

**There is no free retirement in this lane's surface.** That is a finding about
the surface, not a failure to find one — and every one of the six refusals is a
row a plan driven by `H14-1` §4's row counts would have deleted.

## 4. Acceptance, and what was NOT run

**No suite run was performed, and the reason is deliberate.** A foreign
`cratonvm.exe` was resident for the whole session (PID 18524, 135 MB rising to
2.6 GB — the profile of a live GC-stress vector), and `.guard-tmp` is a FIXED
shared path. `H14-3` §5 is the standing case: two concurrent sweeps moved a
published cell by **twenty vectors** and three arms had to be discarded. Starting
a run into that would have produced a number worse than no number.

**This lane changed no source.** Its entire diff is three new files under
`docs/known-issues/jdk-only/`. So the acceptance figures — `--jdk-only` 105/105,
`SUITE=all` 103/105, `SUITE=core` 65/65 — **cannot have moved**, because nothing
this lane wrote is compiled into anything. That is **ARGUED from the diff, not
MEASURED**, and it is the weakest sentence in these three records. Anyone
wanting the measurement should run the three arms on a quiet host; the
prediction is that they are byte-identical to `025780ff7`'s.

## 5. What this does NOT establish

* **No refusal here was tested by attempting the retirement.** Each is argued
  from the image, the dump, and the tree. R1 in particular predicts what
  promoting `lib.rs:14080` would do by reading its body, not by running it.
* **The 179 *retire*-verdict rows in `register_string_builder_natives` were not
  adjudicated individually.** They were excluded as a block on the brief's
  instruction. Whether any of them is separable from the catastrophic
  `append`/`toString` core is **unmeasured**, and `H14-3` priced only the
  whole-class arm.
* **R4's corpus grep is a search for call sites in vector SOURCE**, not a
  dynamic reachability proof. A vector could reach `getDefinedPackage` through
  JDK internals without naming it. The claim is that nothing in the corpus
  *targets* those methods, which is enough to make a zero uninformative, not
  enough to prove the methods are never entered.
* **No multi-image sweep** (`H25-1` §1.6). R3 is the only row in this lane
  proven cross-version; others may be.
* **`i2_register_classloader_package_natives` is invoked from
  `lang_invoke.rs:12499`**, lane H24's file. This lane did not touch it and did
  not check whether H24 has changed it.

## 6. NOMINATIONS

* **N1 — delete BOTH `java/lang/Thread.stop()V` registrations in one commit**
  (R1), `deprecated_lang.rs:375` and `lib.rs:14080`, and both `stop0`
  registrations with them. The result is the real JDK bytecode, which throws the
  `UnsupportedOperationException` the current winner throws anyway — so it is a
  behaviour-preserving retirement plus a message-fidelity fix. It needs one lane
  that owns both files. **Deleting either one alone is a regression.**
* **N2 — `deprecated_api_manifest()` should be adjudicated against the image,
  not maintained by hand** (R2). Six of its `java.lang` entries name methods JDK
  25 does not declare; the manifest asserts they stay registered forever. Either
  the manifest gains an "absent from the supported images" column, or the six
  registrations and their six assertions move together. Both files are outside
  this lane.
* **N3 — a registrar whose rows are justified by ByteBuddy, Mockito, WildFly and
  Spring cannot be priced by this corpus** (R4). Either the corpus gains a
  vector that calls `getDefinedPackage`/`getPackages`/`Package.equals`
  directly — cheap, they are three lines each — or the block is marked
  explicitly as *not dial-priceable*, so the next lane does not read a zero as
  permission. The three-line vector is the better answer and nothing prevents
  it.
* **N4 — revisit the `appendCodePoint` decision** (R6). *"Removing it would move
  a census count for no behavioural gain"* was written before the census became
  the effort's success metric. Deleting `lang_string.rs:215` is inert by
  construction (the winner delegates to the loser's body) and removes one of
  `H14-1`'s 162 multi-registered triples.
* **N5 — 90% of this lane's write surface is one registrar nobody may touch**
  (§1.2). If `register_string_builder_natives` is genuinely off-limits until
  someone re-implements `StringBuilder` on the real compact-string layout, then
  the `java.lang` core block `H14-2` §4 sized at 168 rows is not addressable by
  file-scoped lane ownership at all — the rows live in `lib.rs`'s 14,301-line
  `register_essential_natives_with_shims`, which no lane owns either.
* **N6 — record the brief-correction in whatever generates lane briefs**
  (§1.2). This lane was told a registrar was outside its files when it is 90% of
  them. The dump answers that question in one command and the brief did not.

---

## CORRECTION TO THE BRIEF (lane H0, 2026-08-21)

This lane reports that its brief was wrong on a fact, and it is right.

I wrote: *"`StringBuilder`/`StringBuffer` are NOT in your owned files; treat
this as the cautionary case, not a task."*

```
$ git grep -n "fn register_string_builder_natives" -- '*.rs'
native-builtins/src/lang_string.rs:158:   pub(crate) fn register_string_builder_natives(...)
```

**It is in `lang_string.rs`, an owned file** — and by this lane's count it is
**192 of its 214 registrations, 90% of its write surface.** The registrar that
`H22` measured as **catastrophic** — armed across the three classes it covers,
every `append` silently discarded and `toString()` returning empty, with no
exception and `rc=0` — was inside the lane's reach, not outside it, and my brief
told it the opposite.

The lane followed the instruction's *intent* and left it alone. Had it followed
the instruction's *stated fact* instead — "not yours, so the biggest thing in
your file must be someone else's" — it could have reasoned its way into
retiring it.

**A brief that is wrong about ownership is more dangerous than one that is
vague about it**, because it substitutes a false certainty for a check the lane
would otherwise have made itself. The rule this earns: *state which files a lane
owns, and let the lane discover what is in them.*
